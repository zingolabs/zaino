//! Adaptive work-stream limit: Netflix concurrency-limits' Gradient2 over server time per message
//!
//! - sample = time the server spent producing one message (admission → first DATA frame, then
//!   each poll → the next); a client's backpressure never counts (hyper stops polling)
//! - per window: `limit × clamp(TOLERANCE × long / short, 0.5, 1) + √limit`, smoothed, in
//!   `floor..=ceiling` (`max_streams`)
//! - window whose peak in flight < limit / 2 = app-limited: no change (a quiet node never inflates)
//! - refusal unchanged: `UNAVAILABLE` + pushback (`admission.rs`)

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Short-term RTT may reach this × the long-term baseline before the limit shrinks
const TOLERANCE: f64 = 1.5;

/// Weight of each window's new limit
const SMOOTHING: f64 = 0.2;

/// Windows the long-term RTT averages over (5 min at [`WINDOW`])
const LONG_WINDOWS: f64 = 600.0;

/// A window closes once both this long and [`WINDOW_SAMPLES`] in
const WINDOW: Duration = Duration::from_millis(500);
const WINDOW_SAMPLES: u32 = 10;

/// Lowest adaptive limit (clamped to the ceiling)
const FLOOR: usize = 32;

/// Starting limit (clamped to `FLOOR..=ceiling`): growth needs load, so a cold start admits a burst
const INITIAL: usize = 256;

/// `adaptive` off = the limit stays the ceiling
#[derive(Debug)]
pub(crate) struct Limiter {
    limit: AtomicUsize,
    floor: usize,
    ceiling: usize,
    adaptive: bool,
    gradient: Mutex<Gradient>,
}

/// `long` = baseline RTT (seconds, `None` before the first window)
#[derive(Debug)]
struct Gradient {
    estimated: f64,
    long: Option<f64>,
    window: Window,
}

#[derive(Debug, Default)]
struct Window {
    opened: Option<Instant>,
    sum: f64,
    samples: u32,
    peak: usize,
}

impl Limiter {
    pub(crate) fn new(ceiling: usize, adaptive: bool) -> Self {
        let floor = FLOOR.min(ceiling);
        let initial = if adaptive { INITIAL.clamp(floor, ceiling) } else { ceiling };
        let gradient =
            Gradient { estimated: initial as f64, long: None, window: Window::default() };
        Self {
            limit: AtomicUsize::new(initial),
            floor,
            ceiling,
            adaptive,
            gradient: Mutex::new(gradient),
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// One message's production time, `in_flight` work streams as it landed
    pub(crate) fn sample(&self, took: Duration, in_flight: usize) {
        if !self.adaptive {
            return;
        }
        let now = Instant::now();
        let mut gradient = self.gradient.lock().expect("adaptive limiter poisoned");
        let window = &mut gradient.window;
        let opened = *window.opened.get_or_insert(now);
        window.sum += took.as_secs_f64();
        window.samples += 1;
        window.peak = window.peak.max(in_flight);
        if now - opened < WINDOW || window.samples < WINDOW_SAMPLES {
            return;
        }
        let closed = std::mem::take(&mut gradient.window);
        let short = closed.sum / f64::from(closed.samples);
        let limit = gradient.update(short, closed.peak, self.floor, self.ceiling);
        self.limit.store(limit, Ordering::Relaxed);
    }
}

impl Gradient {
    /// Gradient2's update, one closed window (`short` = its mean RTT, `peak` = most in flight)
    fn update(&mut self, short: f64, peak: usize, floor: usize, ceiling: usize) -> usize {
        // a zero-time window carries no signal (and would divide by zero)
        let short = short.max(1e-6);
        let long = self.long.get_or_insert(short);
        *long += (short - *long) / LONG_WINDOWS;
        // load gone: the baseline follows the faster RTT down
        if *long / short > 2.0 {
            *long *= 0.95;
        }
        if (peak as f64) < self.estimated / 2.0 {
            return self.estimated as usize;
        }
        let gradient = (TOLERANCE * *long / short).clamp(0.5, 1.0);
        let next = self.estimated * gradient + self.estimated.sqrt();
        let smoothed = self.estimated * (1.0 - SMOOTHING) + next * SMOOTHING;
        self.estimated = smoothed.clamp(floor as f64, ceiling as f64);
        self.estimated as usize
    }
}

/// Refusals per second, decaying with [`REFUSAL_DECAY`] (no ticker: read decays to now)
#[derive(Debug)]
pub(crate) struct RefusalRate {
    rate: Mutex<(f64, Instant)>,
}

/// Time constant of [`RefusalRate`]
const REFUSAL_DECAY: Duration = Duration::from_secs(10);

impl Default for RefusalRate {
    fn default() -> Self {
        Self { rate: Mutex::new((0.0, Instant::now())) }
    }
}

impl RefusalRate {
    pub(crate) fn refused(&self) {
        let now = Instant::now();
        let mut rate = self.rate.lock().expect("refusal rate poisoned");
        *rate = (decayed(*rate, now) + 1.0 / REFUSAL_DECAY.as_secs_f64(), now);
    }

    pub(crate) fn per_second(&self) -> f64 {
        decayed(*self.rate.lock().expect("refusal rate poisoned"), Instant::now())
    }
}

fn decayed((rate, at): (f64, Instant), now: Instant) -> f64 {
    rate * (-(now - at).as_secs_f64() / REFUSAL_DECAY.as_secs_f64()).exp()
}

/// Refused share of work admissions (refused / (admitted + refused)), averaged over this
const SHARE_DECAY: Duration = Duration::from_secs(60);

/// Overloaded once the share stays ≥ [`ENTER_SHARE`] (at ≥ [`ENTER_REFUSALS`]/s) this long;
/// cleared once it stays < [`EXIT_SHARE`] as long
const SUSTAINED: Duration = Duration::from_secs(5 * 60);
const ENTER_SHARE: f64 = 0.10;
const ENTER_REFUSALS: f64 = 1.0;
const EXIT_SHARE: f64 = 0.02;

/// Persistent over-capacity, for readiness (a backstop: admission sheds within seconds)
///
/// - counters bumped per admission, rates + latch updated on each [`Overload::overloaded`] read
#[derive(Debug, Default)]
pub(crate) struct Overload {
    admitted: std::sync::atomic::AtomicU64,
    refused: std::sync::atomic::AtomicU64,
    state: Mutex<Option<Latch>>,
}

/// `rates` = (admitted, refused) per second; `since` = when the opposite condition began holding
#[derive(Debug)]
struct Latch {
    read_at: Instant,
    counted: (u64, u64),
    rates: (f64, f64),
    overloaded: bool,
    since: Option<Instant>,
}

impl Overload {
    pub(crate) fn admitted(&self) {
        self.admitted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn refused(&self) {
        self.refused.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn overloaded(&self) -> bool {
        let now = Instant::now();
        let counted = (self.admitted.load(Ordering::Relaxed), self.refused.load(Ordering::Relaxed));
        let mut state = self.state.lock().expect("overload latch poisoned");
        let latch = state.get_or_insert(Latch {
            read_at: now,
            counted,
            rates: (0.0, 0.0),
            overloaded: false,
            since: None,
        });
        let elapsed = (now - latch.read_at).as_secs_f64();
        if elapsed > 0.0 {
            let keep = (-elapsed / SHARE_DECAY.as_secs_f64()).exp();
            let rate = |was: f64, from: u64, to: u64| {
                was * keep + (to - from) as f64 / elapsed * (1.0 - keep)
            };
            latch.rates = (
                rate(latch.rates.0, latch.counted.0, counted.0),
                rate(latch.rates.1, latch.counted.1, counted.1),
            );
            (latch.read_at, latch.counted) = (now, counted);
        }
        let (admitted, refused) = latch.rates;
        let share = match admitted + refused {
            total if total > 0.0 => refused / total,
            _ => 0.0,
        };
        let flipping = match latch.overloaded {
            false => share >= ENTER_SHARE && refused >= ENTER_REFUSALS,
            true => share < EXIT_SHARE,
        };
        match (flipping, latch.since) {
            (false, _) => latch.since = None,
            (true, None) => latch.since = Some(now),
            (true, Some(since)) if now - since >= SUSTAINED => {
                latch.overloaded = !latch.overloaded;
                latch.since = None;
            }
            (true, Some(_)) => {}
        }
        latch.overloaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paused clock, ceiling 2048, 10 ms baseline:
    /// - loaded at a steady RTT: grows (≈ +√limit / 5 per window) toward the ceiling
    /// - RTT 4× the baseline under load: shrinks within seconds, never below the floor
    /// - RTT back to the baseline: grows again
    /// - app-limited (peak in flight < limit / 2): unchanged even as RTT rises
    /// - not adaptive: the ceiling, whatever the samples
    #[tokio::test(start_paused = true)]
    async fn the_limit_tracks_latency_under_load_and_holds_while_app_limited() {
        let ms = Duration::from_millis;
        let advance = || tokio::time::advance(WINDOW);
        let windows = |limiter: &Limiter, count: usize, rtt, in_flight| {
            let mut limits = Vec::new();
            for _ in 0..count {
                for _ in 0..WINDOW_SAMPLES {
                    limiter.sample(rtt, in_flight);
                }
                limits.push(limiter.limit());
            }
            limits
        };
        let limiter = Limiter::new(2048, true);
        assert_eq!(limiter.limit(), INITIAL, "starts at INITIAL");

        let mut grown = Vec::new();
        for _ in 0..20 {
            advance().await;
            grown.extend(windows(&limiter, 1, ms(10), 4_096));
        }
        assert!(grown.windows(2).all(|pair| pair[1] >= pair[0]), "steady RTT: never shrinks");
        assert!(*grown.last().expect("20 windows") > INITIAL + 40, "grew: {grown:?}");

        let before = limiter.limit();
        let mut shrunk = Vec::new();
        for _ in 0..10 {
            advance().await;
            shrunk.extend(windows(&limiter, 1, ms(40), 4_096));
        }
        let after = *shrunk.last().expect("10 windows");
        assert!(after < before / 2, "5 s at 4× the RTT: {before} → {shrunk:?}");
        for _ in 0..200 {
            advance().await;
            windows(&limiter, 1, ms(400), 4_096);
        }
        assert_eq!(limiter.limit(), FLOOR, "never below the floor");

        let floor = limiter.limit();
        for _ in 0..20 {
            advance().await;
            windows(&limiter, 1, ms(10), 4_096);
        }
        assert!(limiter.limit() > floor, "RTT recovered: grows again");

        // the open window still carries busy samples: closed once before measuring quiet
        advance().await;
        windows(&limiter, 1, ms(10), 1);
        let quiet = limiter.limit();
        for _ in 0..20 {
            advance().await;
            windows(&limiter, 1, ms(400), 1);
        }
        assert_eq!(limiter.limit(), quiet, "app-limited: unchanged");

        let fixed = Limiter::new(2048, false);
        for _ in 0..20 {
            advance().await;
            windows(&fixed, 1, ms(400), 4_096);
        }
        assert_eq!(fixed.limit(), 2048, "not adaptive: the ceiling");
    }

    /// Paused clock, read every 10 s, 100 admissions/s:
    /// - 5 % refused for 10 min: never overloaded (under 10 %)
    /// - 30 % refused: overloaded only after 5 min held
    /// - a 10 s dip to 1 % inside that never resets the clock (the 60 s average carries it)
    /// - back to 0 %: cleared only after 5 min under 2 %
    /// - 50 % of 1 admission/s (0.5 refusals/s): below the 1/s floor, never overloaded
    #[tokio::test(start_paused = true)]
    async fn overload_latches_after_five_minutes_sustained_and_clears_after_five_quiet() {
        /// 10 s of `per_second` work admissions, `refused_percent` of them refused, then a read
        async fn tick(overload: &Overload, per_second: u64, refused_percent: u64) -> bool {
            let total = per_second * 10;
            for _ in 0..total * (100 - refused_percent) / 100 {
                overload.admitted();
            }
            for _ in 0..total * refused_percent / 100 {
                overload.refused();
            }
            tokio::time::advance(Duration::from_secs(10)).await;
            overload.overloaded()
        }

        let mild = Overload::default();
        for _ in 0..60 {
            assert!(!tick(&mild, 100, 5).await, "5 % refused: not overloaded");
        }

        let overload = Overload::default();
        let mut states = Vec::new();
        for at in 0..60 {
            let refused = if at == 15 { 1 } else { 30 };
            states.push(tick(&overload, 100, refused).await);
        }
        let latched = 10 * (states.iter().position(|state| *state).expect("within 10 min") + 1);
        assert!((300..=420).contains(&latched), "after 5 min held, dip included: {latched} s");

        for _ in 0..30 {
            assert!(tick(&overload, 100, 0).await, "still within 5 min of the drop");
        }
        let mut cleared = Vec::new();
        for _ in 0..30 {
            cleared.push(tick(&overload, 100, 0).await);
        }
        assert!(cleared.contains(&false), "cleared within 10 min of the drop: {cleared:?}");

        let trickle = Overload::default();
        for _ in 0..120 {
            assert!(!tick(&trickle, 1, 50).await, "0.5 refusals/s: under the floor");
        }
    }

    /// One refusal per 100 ms for 30 s ≈ 10/s; 30 s of none decays it below 1/s
    #[tokio::test(start_paused = true)]
    async fn the_refusal_rate_converges_and_decays() {
        let rate = RefusalRate::default();
        for _ in 0..300 {
            rate.refused();
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        let busy = rate.per_second();
        assert!((9.0..=10.5).contains(&busy), "{busy}");
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(rate.per_second() < 1.0, "{}", rate.per_second());
    }
}
