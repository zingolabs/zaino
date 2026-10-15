//! Serving summary every [`INTERVAL`], from status-code tallies every request feeds (latency and
//! bytes out = the `zaino.grpc.*` metrics)
//!
//! - `Serving`: rate, connections held, then the window's problems by count (silent while nothing
//!   served or held)
//! - `Serving` level: WARN once any request failed, was refused, stalled or ran slow, else INFO
//! - `High load` (WARN) instead while any cap is past [`HIGH_LOAD`]: those caps as `used/max`
//! - `Method served` (DEBUG): the same, per method with traffic
//! - First server fault (ERROR, `Request failed`) and first unavailable answer (WARN, `Request
//!   unavailable`) of a window logged as they happen, the rest only counted
//! - Counters = relaxed atomics, swapped to zero per summary: a request costs a `fetch_add` or
//!   two, a summary a few hundred swaps

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tonic::Code;
use tracing::{debug, error, field::display, info, warn};

use crate::emit::{label, slot, CODES, LABELS};
use crate::service::Method;

pub(crate) const INTERVAL: Duration = Duration::from_secs(60);

/// Time to first message past this = a slow request
const SLOW: Duration = Duration::from_secs(1);

/// Share of a cap (work streams, subscriptions, connections) held that logs `High load`
const HIGH_LOAD: f64 = 0.25;

/// Who a status code blames: `Client` = request / client gone (4xx-like), `Refused` = not now
/// (capacity, nothing served yet, validator down), `Failed` = this server (5xx-like)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Ok,
    Client,
    Refused,
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
    slow: AtomicU64,
}

impl MethodWindow {
    const fn new() -> Self {
        Self { codes: [const { AtomicU64::new(0) }; CODES.len()], slow: AtomicU64::new(0) }
    }
}

/// Everything counted since the last summary (`*_logged` = this window's first already logged)
struct Window {
    methods: [MethodWindow; LABELS],
    connections_refused: AtomicU64,
    stalled: AtomicU64,
    failure_logged: AtomicBool,
    unavailable_logged: AtomicBool,
}

impl Window {
    const fn new() -> Self {
        Self {
            methods: [const { MethodWindow::new() }; LABELS],
            connections_refused: AtomicU64::new(0),
            stalled: AtomicU64::new(0),
            failure_logged: AtomicBool::new(false),
            unavailable_logged: AtomicBool::new(false),
        }
    }

    fn finished(&self, method: Option<Method>, code: Code, latency: Option<Duration>) {
        let counters = &self.methods[slot(method)];
        counters.codes[code as usize].fetch_add(1, Ordering::Relaxed);
        if latency.is_some_and(|latency| latency > SLOW) {
            counters.slow.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Every counter read and zeroed (a request landing mid-swap counts in one window or the next)
    fn take(&self) -> WindowCounts {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        self.failure_logged.store(false, Ordering::Relaxed);
        self.unavailable_logged.store(false, Ordering::Relaxed);
        WindowCounts {
            methods: self
                .methods
                .iter()
                .map(|method| MethodTotals {
                    codes: std::array::from_fn(|at| take(&method.codes[at])),
                    slow: take(&method.slow),
                })
                .collect(),
            connections_refused: take(&self.connections_refused),
            stalled: take(&self.stalled),
        }
    }
}

static WINDOW: Window = Window::new();

/// Stream close-out (`latency` = time to its first message, for an answered work request)
pub(crate) fn finished(method: Option<Method>, code: Code, latency: Option<Duration>) {
    WINDOW.finished(method, code, latency);
}

pub(crate) fn connection_refused() {
    WINDOW.connections_refused.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn connection_stalled() {
    WINDOW.stalled.fetch_add(1, Ordering::Relaxed);
}

/// A server fault or unavailable answer (never an admission refusal): the window's first of each
/// is logged now, with the message the client got
pub(crate) fn answered_badly(method: Option<Method>, code: Code, message: &str) {
    let first = |logged: &AtomicBool| !logged.swap(true, Ordering::Relaxed);
    let method = label(slot(method));
    match Outcome::of(code) {
        Outcome::Failed if first(&WINDOW.failure_logged) => {
            error!(method, code = CODES[code as usize], error = message, "Request failed");
        }
        Outcome::Refused if first(&WINDOW.unavailable_logged) => {
            warn!(method, error = message, "Request unavailable");
        }
        _ => {}
    }
}

/// Each cap as `(in use, max)` at a summary
#[derive(Clone, Copy, Debug)]
pub(crate) struct CapsInUse {
    pub(crate) work_streams: (usize, usize),
    pub(crate) subscriptions: (usize, usize),
    pub(crate) connections: (usize, usize),
}

struct WindowCounts {
    methods: Vec<MethodTotals>,
    connections_refused: u64,
    stalled: u64,
}

struct MethodTotals {
    codes: [u64; CODES.len()],
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

/// Logs the window since the last call (`elapsed` long, `at_capacity` admission refusals) and
/// starts the next
pub(crate) fn summarise(elapsed: Duration, caps: CapsInUse, at_capacity: u64) {
    let counts = WINDOW.take();
    let secs = elapsed.as_secs_f64().max(f64::EPSILON);
    let (mut total, mut slow, mut slowest) = (Tally::default(), 0, None);
    let mut most_slow = 0;

    for (at, method) in counts.methods.iter().enumerate() {
        let tally = Tally::of(&method.codes);
        if tally.requests == 0 {
            continue;
        }
        let name = label(at);
        debug!(
            method = name,
            requests = tally.requests,
            client = tally.client,
            refused = tally.refused,
            failed = tally.failed,
            slow = method.slow,
            "Method served"
        );
        total.requests += tally.requests;
        total.ok += tally.ok;
        total.client += tally.client;
        total.refused += tally.refused;
        total.failed += tally.failed;
        slow += method.slow;
        if method.slow > most_slow {
            (most_slow, slowest) = (method.slow, Some(name));
        }
    }

    let holding = caps.work_streams.0 + caps.subscriptions.0 + caps.connections.0 > 0;
    let problems =
        total.refused + total.failed + slow + counts.stalled + counts.connections_refused;
    if total.requests == 0 && !holding && problems == 0 {
        return;
    }
    let nonzero = |count: u64| (count > 0).then_some(count);
    let high = |(used, cap): (usize, usize)| used as f64 > cap as f64 * HIGH_LOAD;
    let over = |in_use: (usize, usize)| high(in_use).then(|| display(Used(in_use)));
    let busy = high(caps.work_streams) || high(caps.subscriptions) || high(caps.connections);
    let conns = match high(caps.connections) {
        true => Used(caps.connections).to_string(),
        false => Thousands(caps.connections.0 as u64).to_string(),
    };

    macro_rules! summary {
        ($level:ident, $message:literal) => {
            $level!(
                rps = %Rate(total.requests as f64 / secs),
                conns = %conns,
                work = over(caps.work_streams),
                subs = over(caps.subscriptions),
                failed = nonzero(total.failed),
                refused = nonzero(total.refused),
                at_capacity = nonzero(at_capacity),
                slow = nonzero(slow),
                slowest,
                stalled = nonzero(counts.stalled),
                conns_refused = nonzero(counts.connections_refused),
                $message
            )
        };
    }
    match (busy, problems) {
        (true, _) => summary!(warn, "High load"),
        (false, 0) => summary!(info, "Serving"),
        (false, _) => summary!(warn, "Serving"),
    }
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

    /// - codes split by who they blame
    /// - slow = past [`SLOW`] to the first message
    #[test]
    fn a_window_tallies_outcomes_and_slow_requests() {
        let window = Window::new();
        let method = Some(Method::GetBlock);
        let ms = Duration::from_millis;
        for latency in 1..=100 {
            window.finished(method, Code::Ok, Some(ms(latency)));
        }
        window.finished(method, Code::Ok, Some(ms(1_500)));
        for code in [Code::NotFound, Code::Cancelled, Code::Unavailable, Code::Internal] {
            window.finished(method, code, None);
        }

        let counts = window.take();
        let totals = &counts.methods[slot(method)];
        assert_eq!(
            (Tally::of(&totals.codes), totals.slow),
            (Tally { requests: 105, ok: 101, client: 2, refused: 1, failed: 1 }, 1),
            "NotFound and Cancelled blame the client; one past 1 s"
        );

        let drained = window.take();
        let drained = &drained.methods[slot(method)];
        assert_eq!((Tally::of(&drained.codes), drained.slow), (Tally::default(), 0));
    }

    #[test]
    fn values_read_as_one_short_token() {
        let shown = |value: &dyn fmt::Display| value.to_string();
        assert_eq!(shown(&Thousands(0)), "0");
        assert_eq!(shown(&Thousands(1_234_567)), "1,234,567");
        assert_eq!(shown(&Rate(0.35)), "0.35");
        assert_eq!(shown(&Rate(42.14)), "42.1");
        assert_eq!(shown(&Rate(1_204.4)), "1,204");
        assert_eq!(shown(&Used((4, 2_048))), "4/2,048");
    }
}
