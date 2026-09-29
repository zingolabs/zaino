//! Serving summary every [`INTERVAL`], from counters every request feeds (rate, errors,
//! latency, saturation)
//!
//! - `Serving requests`: requests + rate, success latency (time to first message, p50 / p99 /
//!   max), bytes out per second, permits and connections held against their caps, then the
//!   window's problems by count; WARN once any request failed, was refused, stalled or ran slow,
//!   INFO otherwise; silent while nothing is served or held
//! - `Method served` (DEBUG): the same, per method with traffic
//! - First server fault (ERROR, `Request failed`) and first unavailable answer (WARN, `Request
//!   unavailable`) of a window logged as they happen, the rest only counted
//! - Counters = relaxed atomics, swapped to zero per summary: a request costs a handful of
//!   `fetch_add`s, a summary a few thousand swaps

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tonic::Code;
use tracing::{
    debug, error,
    field::{display, DisplayValue},
    info, warn,
};

use crate::emit::{Method, CODES, METHODS};

pub(crate) const INTERVAL: Duration = Duration::from_secs(60);

/// Time to first message past this = a slow request
const SLOW: Duration = Duration::from_secs(1);

/// Latency buckets: `2^SUB_BITS` per power of two of µs (each ≤ 12.5% wide), exact below that
const SUB_BITS: u32 = 3;
/// Longest latency told apart (~38 h); anything longer shares the top bucket
const MAX_US: u64 = (1 << 37) - 1;
const BUCKETS: usize = bucket(MAX_US) + 1;

/// `us`'s latency bucket
const fn bucket(us: u64) -> usize {
    let us = if us > MAX_US { MAX_US } else { us };
    if us < 1 << SUB_BITS {
        return us as usize;
    }
    let exp = 63 - us.leading_zeros();
    let mantissa = (us >> (exp - SUB_BITS)) & ((1 << SUB_BITS) - 1);
    (((exp - SUB_BITS + 1) as usize) << SUB_BITS) + mantissa as usize
}

/// Largest µs `index`'s bucket holds
fn ceiling(index: usize) -> u64 {
    let sub = 1 << SUB_BITS;
    if index < sub {
        return index as u64;
    }
    let shift = (index >> SUB_BITS) as u32 - 1;
    let mantissa = (index & (sub - 1)) as u64;
    ((sub as u64 + mantissa) << shift) + (1 << shift) - 1
}

/// Who a status code blames
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    /// The request itself, or its client walking away (4xx-like: never this server's error)
    Client,
    /// Not served now: at capacity, index syncing, validator unreachable
    Refused,
    /// This server's fault (5xx-like)
    Failed,
}

impl Outcome {
    fn of(code: Code) -> Self {
        match code {
            Code::Ok => Self::Ok,
            Code::Unavailable | Code::ResourceExhausted => Self::Refused,
            Code::Unknown | Code::Internal | Code::DataLoss => Self::Failed,
            _ => Self::Client,
        }
    }
}

/// One method's counters for the current window
struct MethodWindow {
    codes: [AtomicU64; CODES.len()],
    latency: [AtomicU64; BUCKETS],
    sent: AtomicU64,
    max_us: AtomicU64,
    slow: AtomicU64,
}

impl MethodWindow {
    const fn new() -> Self {
        Self {
            codes: [const { AtomicU64::new(0) }; CODES.len()],
            latency: [const { AtomicU64::new(0) }; BUCKETS],
            sent: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            slow: AtomicU64::new(0),
        }
    }
}

/// Everything counted since the last summary
struct Window {
    methods: [MethodWindow; METHODS.len()],
    at_capacity: AtomicU64,
    connections_refused: AtomicU64,
    stalled: AtomicU64,
    /// A failure / unavailable answer already logged this window
    failure_logged: AtomicBool,
    unavailable_logged: AtomicBool,
}

impl Window {
    const fn new() -> Self {
        Self {
            methods: [const { MethodWindow::new() }; METHODS.len()],
            at_capacity: AtomicU64::new(0),
            connections_refused: AtomicU64::new(0),
            stalled: AtomicU64::new(0),
            failure_logged: AtomicBool::new(false),
            unavailable_logged: AtomicBool::new(false),
        }
    }

    fn finished(&self, method: Method, code: Code, latency: Option<Duration>, sent: u64) {
        let counters = &self.methods[method.index()];
        counters.codes[code as usize].fetch_add(1, Ordering::Relaxed);
        counters.sent.fetch_add(sent, Ordering::Relaxed);
        if let Some(latency) = latency {
            let us = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
            counters.latency[bucket(us)].fetch_add(1, Ordering::Relaxed);
            counters.max_us.fetch_max(us, Ordering::Relaxed);
            if latency > SLOW {
                counters.slow.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Every counter read and zeroed (a request landing mid-swap counts in one window or the next)
    fn take(&self) -> Snapshot {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        self.failure_logged.store(false, Ordering::Relaxed);
        self.unavailable_logged.store(false, Ordering::Relaxed);
        Snapshot {
            methods: self
                .methods
                .iter()
                .map(|method| MethodTotals {
                    codes: std::array::from_fn(|at| take(&method.codes[at])),
                    latency: method.latency.iter().map(take).collect(),
                    sent: take(&method.sent),
                    max_us: take(&method.max_us),
                    slow: take(&method.slow),
                })
                .collect(),
            at_capacity: take(&self.at_capacity),
            connections_refused: take(&self.connections_refused),
            stalled: take(&self.stalled),
        }
    }
}

static WINDOW: Window = Window::new();

/// A stream's close-out; `latency` = time to its first message, for an answered work request
pub(crate) fn finished(method: Method, code: Code, latency: Option<Duration>, sent: u64) {
    WINDOW.finished(method, code, latency, sent);
}

/// A request refused for want of an admission permit
pub(crate) fn at_capacity() {
    WINDOW.at_capacity.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn connection_refused() {
    WINDOW.connections_refused.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn connection_stalled() {
    WINDOW.stalled.fetch_add(1, Ordering::Relaxed);
}

/// A server fault or unavailable answer (never an admission refusal): the window's first of each
/// is logged now, with the message the client got
pub(crate) fn answered_badly(method: Method, code: Code, message: &str) {
    let first = |logged: &AtomicBool| !logged.swap(true, Ordering::Relaxed);
    match Outcome::of(code) {
        Outcome::Failed if first(&WINDOW.failure_logged) => {
            error!(
                method = method.name(),
                code = CODES[code as usize],
                error = message,
                "Request failed"
            );
        }
        Outcome::Refused if first(&WINDOW.unavailable_logged) => {
            warn!(method = method.name(), error = message, "Request unavailable");
        }
        _ => {}
    }
}

/// Held against the cap it counts towards
#[derive(Clone, Copy, Debug)]
pub(crate) struct Held {
    pub(crate) streams: (usize, usize),
    pub(crate) subscriptions: (usize, usize),
    pub(crate) connections: (usize, usize),
}

/// One window's counts
struct Snapshot {
    methods: Vec<MethodTotals>,
    at_capacity: u64,
    connections_refused: u64,
    stalled: u64,
}

struct MethodTotals {
    codes: [u64; CODES.len()],
    latency: Vec<u64>,
    sent: u64,
    max_us: u64,
    slow: u64,
}

/// Requests by [`Outcome`]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tally {
    requests: u64,
    ok: u64,
    client: u64,
    refused: u64,
    failed: u64,
}

impl Tally {
    fn of(codes: &[u64; CODES.len()]) -> Self {
        let mut tally = Self::default();
        for (at, &count) in codes.iter().enumerate() {
            tally.requests += count;
            let slot = match Outcome::of(Code::from_i32(at as i32)) {
                Outcome::Ok => &mut tally.ok,
                Outcome::Client => &mut tally.client,
                Outcome::Refused => &mut tally.refused,
                Outcome::Failed => &mut tally.failed,
            };
            *slot += count;
        }
        tally
    }
}

/// Latency at quantile `q` (µs, never above `max_us`); `None` = nothing timed
fn quantile(buckets: &[u64], max_us: u64, q: f64) -> Option<u64> {
    let timed: u64 = buckets.iter().sum();
    let rank = ((q * timed as f64).ceil() as u64).max(1);
    let mut seen = 0;
    for (index, &count) in buckets.iter().enumerate() {
        seen += count;
        if seen >= rank {
            return Some(ceiling(index).min(max_us));
        }
    }
    None
}

/// Logs the window since the last call (`elapsed` long) and starts the next
pub(crate) fn summarise(elapsed: Duration, held: Held) {
    let snapshot = WINDOW.take();
    let secs = elapsed.as_secs_f64().max(f64::EPSILON);
    let mut latency = vec![0; BUCKETS];
    let (mut total, mut sent, mut max_us, mut slow) = (Tally::default(), 0, 0, 0);
    let mut slowest = None;

    for (at, method) in snapshot.methods.iter().enumerate() {
        let tally = Tally::of(&method.codes);
        if tally.requests == 0 {
            continue;
        }
        let name = METHODS[at];
        let (p50, p99) = (
            quantile(&method.latency, method.max_us, 0.5),
            quantile(&method.latency, method.max_us, 0.99),
        );
        debug!(
            method = name,
            requests = tally.requests,
            p50 = timed(p50),
            p99 = timed(p99),
            out = %ByteRate(method.sent as f64 / secs),
            client = tally.client,
            refused = tally.refused,
            failed = tally.failed,
            "Method served"
        );
        for (sum, count) in latency.iter_mut().zip(&method.latency) {
            *sum += count;
        }
        total.requests += tally.requests;
        total.ok += tally.ok;
        total.client += tally.client;
        total.refused += tally.refused;
        total.failed += tally.failed;
        sent += method.sent;
        slow += method.slow;
        if method.max_us > max_us {
            (max_us, slowest) = (method.max_us, Some(name));
        }
    }

    let holding = held.streams.0 + held.subscriptions.0 + held.connections.0 > 0;
    let problems =
        total.refused + total.failed + slow + snapshot.stalled + snapshot.connections_refused;
    if total.requests == 0 && !holding && problems == 0 {
        return;
    }
    let nonzero = |count: u64| (count > 0).then_some(count);
    let (p50, p99) = (quantile(&latency, max_us, 0.5), quantile(&latency, max_us, 0.99));

    macro_rules! summary {
        ($level:ident) => {
            $level!(
                requests = total.requests,
                rps = %Rate(total.requests as f64 / secs),
                p50 = timed(p50),
                p99 = timed(p99),
                max = timed(p50.and(Some(max_us))),
                out = %ByteRate(sent as f64 / secs),
                streams = %Used(held.streams),
                subs = %Used(held.subscriptions),
                conns = %Used(held.connections),
                failed = nonzero(total.failed),
                refused = nonzero(total.refused),
                at_capacity = nonzero(snapshot.at_capacity),
                slow = nonzero(slow),
                slowest = (slow > 0).then_some(slowest).flatten(),
                stalled = nonzero(snapshot.stalled),
                conns_refused = nonzero(snapshot.connections_refused),
                "Serving requests"
            )
        };
    }
    match problems {
        0 => summary!(info),
        _ => summary!(warn),
    }
}

/// A latency field; `None` = nothing timed, the field left out
fn timed(us: Option<u64>) -> Option<DisplayValue<Latency>> {
    us.map(|us| display(Latency(us)))
}

/// `2,048` from 1,000 up
struct Thousands(u64);

impl fmt::Display for Thousands {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.0.to_string();
        for (at, digit) in digits.chars().enumerate() {
            if at > 0 && (digits.len() - at).is_multiple_of(3) {
                f.write_str(",")?;
            }
            write!(f, "{digit}")?;
        }
        Ok(())
    }
}

/// Per second: `0.35`, `42.1`, `1,204`
struct Rate(f64);

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            rate if rate < 10.0 => write!(f, "{rate:.2}"),
            rate if rate < 100.0 => write!(f, "{rate:.1}"),
            rate => write!(f, "{}", Thousands(rate.round() as u64)),
        }
    }
}

/// µs as `850µs`, `4.25ms`, `38.4ms`, `412ms`, `2.31s`
struct Latency(u64);

impl fmt::Display for Latency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0 as f64 / 1e3;
        match self.0 {
            us if us < 1_000 => write!(f, "{us}µs"),
            us if us < 10_000 => write!(f, "{ms:.2}ms"),
            us if us < 100_000 => write!(f, "{ms:.1}ms"),
            us if us < 1_000_000 => write!(f, "{ms:.0}ms"),
            _ => write!(f, "{:.2}s", ms / 1e3),
        }
    }
}

/// Bytes per second, binary units: `0B/s`, `12.0KiB/s`, `3.1MiB/s`
struct ByteRate(f64);

impl fmt::Display for ByteRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let (mut value, mut unit) = (self.0, 0);
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        match unit {
            0 => write!(f, "{value:.0}B/s"),
            _ => write!(f, "{value:.1}{}/s", UNITS[unit]),
        }
    }
}

/// Held of its cap: `4/2,048`
struct Used((usize, usize));

impl fmt::Display for Used {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (held, cap) = self.0;
        write!(f, "{}/{}", Thousands(held as u64), Thousands(cap as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every latency lands in a bucket whose ceiling covers it within 12.5%, buckets ascend
    /// without gaps, and past the top everything shares the last bucket
    #[test]
    fn buckets_cover_every_latency_within_an_eighth() {
        for us in (0..5_000).chain((1..37).flat_map(|exp| {
            let base = 1u64 << exp;
            [base - 1, base, base + 1, base + base / 3]
        })) {
            let at = bucket(us);
            let top = ceiling(at);
            assert!(top >= us, "{us} above its ceiling {top}");
            assert!((top - us) as f64 <= us as f64 / 8.0 + 1.0, "{us} → {top}");
            assert!(at == 0 || ceiling(at - 1) < us, "{us} fits the bucket below");
        }
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        assert_eq!(ceiling(BUCKETS - 1), MAX_US);
    }

    /// Codes split by who they blame; quantiles read off the buckets, capped at the max seen
    #[test]
    fn a_window_tallies_outcomes_and_reads_quantiles() {
        let window = Window::new();
        let method = Method::of("/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlock");
        let ms = Duration::from_millis;
        for latency in 1..=100 {
            window.finished(method, Code::Ok, Some(ms(latency)), 1_000);
        }
        window.finished(method, Code::Ok, Some(ms(1_500)), 0);
        for code in [Code::NotFound, Code::Cancelled, Code::Unavailable, Code::Internal] {
            window.finished(method, code, None, 0);
        }

        let snapshot = window.take();
        let totals = &snapshot.methods[method.index()];
        let tally = Tally::of(&totals.codes);
        assert_eq!(
            tally,
            Tally { requests: 105, ok: 101, client: 2, refused: 1, failed: 1 },
            "NotFound and Cancelled blame the client"
        );
        assert_eq!((totals.sent, totals.slow, totals.max_us), (100_000, 1, 1_500_000));
        let p50 = quantile(&totals.latency, totals.max_us, 0.5).expect("timed");
        assert!((51_000..=57_000).contains(&p50), "p50 {p50}");
        let p99 = quantile(&totals.latency, totals.max_us, 0.99).expect("timed");
        assert!((100_000..=112_500).contains(&p99), "p99 {p99}");
        assert_eq!(quantile(&totals.latency, totals.max_us, 1.0), Some(1_500_000), "max caps");
        assert_eq!(quantile(&[0; 4], 0, 0.5), None);

        let drained = window.take();
        assert_eq!(Tally::of(&drained.methods[method.index()].codes), Tally::default());
    }

    #[test]
    fn values_read_as_one_short_token() {
        let shown = |value: &dyn fmt::Display| value.to_string();
        assert_eq!(shown(&Thousands(0)), "0");
        assert_eq!(shown(&Thousands(1_234_567)), "1,234,567");
        assert_eq!(shown(&Rate(0.35)), "0.35");
        assert_eq!(shown(&Rate(42.14)), "42.1");
        assert_eq!(shown(&Rate(1_204.4)), "1,204");
        assert_eq!(shown(&Latency(850)), "850µs");
        assert_eq!(shown(&Latency(4_250)), "4.25ms");
        assert_eq!(shown(&Latency(38_420)), "38.4ms");
        assert_eq!(shown(&Latency(412_300)), "412ms");
        assert_eq!(shown(&Latency(2_310_000)), "2.31s");
        assert_eq!(shown(&ByteRate(0.0)), "0B/s");
        assert_eq!(shown(&ByteRate(3.1 * 1024.0 * 1024.0)), "3.1MiB/s");
        assert_eq!(shown(&Used((4, 2_048))), "4/2,048");
    }
}
