//! Adaptive work-stream limit: Netflix concurrency-limits' Gradient2 over server time per message
//!
//! - sample = time the server spent producing one message (admission → first DATA frame, then
//!   each poll → the next)
//! - client backpressure never counts (hyper stops polling)
//! - per window: `limit × clamp(TOLERANCE × long / short, 0.5, 1) + √limit`, smoothed, in
//!   `floor..=ceiling` (`max_work_streams`)
//! - window whose peak in flight < limit / 2 = app-limited: no change (a quiet node never inflates)
//! - refusal unchanged: `UNAVAILABLE` + pushback (`admission.rs`)

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
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
///
/// - `sample` = relaxed atomics only; the one sample that closes a window takes `gradient`
#[derive(Debug)]
pub(crate) struct Limiter {
    limit: AtomicUsize,
    floor: usize,
    ceiling: usize,
    adaptive: bool,
    window: OpenWindow,
    gradient: Mutex<Gradient>,
}

/// Samples since the last close (`opened_us` = that close, since `epoch`)
///
/// - a sample racing the close may land in either window (±1 of ≥ [`WINDOW_SAMPLES`])
#[derive(Debug)]
struct OpenWindow {
    epoch: Instant,
    opened_us: AtomicU64,
    sum_us: AtomicU64,
    samples: AtomicU32,
    peak: AtomicUsize,
}

/// `long` = baseline RTT (seconds, `None` before the first window)
#[derive(Debug)]
struct Gradient {
    estimated: f64,
    long: Option<f64>,
}

impl Limiter {
    pub(crate) fn new(ceiling: usize, adaptive: bool) -> Self {
        let floor = FLOOR.min(ceiling);
        let initial = if adaptive { INITIAL.clamp(floor, ceiling) } else { ceiling };
        let window = OpenWindow {
            epoch: Instant::now(),
            opened_us: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            samples: AtomicU32::new(0),
            peak: AtomicUsize::new(0),
        };
        Self {
            limit: AtomicUsize::new(initial),
            floor,
            ceiling,
            adaptive,
            window,
            gradient: Mutex::new(Gradient { estimated: initial as f64, long: None }),
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.limit.load(Relaxed)
    }

    /// One message's production time, `in_flight` work streams as it landed
    pub(crate) fn sample(&self, took: Duration, in_flight: usize) {
        if !self.adaptive {
            return;
        }
        let window = &self.window;
        let micros = |duration: Duration| u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        window.sum_us.fetch_add(micros(took), Relaxed);
        let samples = window.samples.fetch_add(1, Relaxed) + 1;
        window.peak.fetch_max(in_flight, Relaxed);
        let (now, opened) = (micros(window.epoch.elapsed()), window.opened_us.load(Relaxed));
        // saturating: a racing closer may have stamped a later `now` than ours
        if now.saturating_sub(opened) < micros(WINDOW) || samples < WINDOW_SAMPLES {
            return;
        }
        // one closer per window
        if window.opened_us.compare_exchange(opened, now, Relaxed, Relaxed).is_err() {
            return;
        }
        let sum = window.sum_us.swap(0, Relaxed) as f64 / 1e6;
        let (samples, peak) = (window.samples.swap(0, Relaxed), window.peak.swap(0, Relaxed));
        let mut gradient = self.gradient.lock().expect("adaptive limiter poisoned");
        let limit =
            gradient.update(sum / f64::from(samples.max(1)), peak, self.floor, self.ceiling);
        self.limit.store(limit, Relaxed);
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
        // one window passed, then filled with `rtt` samples: the limit after it
        let window = async |limiter: &Limiter, rtt, in_flight| {
            tokio::time::advance(WINDOW).await;
            for _ in 0..WINDOW_SAMPLES {
                limiter.sample(rtt, in_flight);
            }
            limiter.limit()
        };
        let limiter = Limiter::new(2048, true);
        assert_eq!(limiter.limit(), INITIAL, "starts at INITIAL");

        let mut grown = Vec::new();
        for _ in 0..20 {
            grown.push(window(&limiter, ms(10), 4_096).await);
        }
        assert!(grown.windows(2).all(|pair| pair[1] >= pair[0]), "steady RTT: never shrinks");
        assert!(*grown.last().expect("20 windows") > INITIAL + 40, "grew: {grown:?}");

        let before = limiter.limit();
        let mut shrunk = Vec::new();
        for _ in 0..10 {
            shrunk.push(window(&limiter, ms(40), 4_096).await);
        }
        let after = *shrunk.last().expect("10 windows");
        assert!(after < before / 2, "5 s at 4× the RTT: {before} → {shrunk:?}");
        for _ in 0..200 {
            window(&limiter, ms(400), 4_096).await;
        }
        assert_eq!(limiter.limit(), FLOOR, "never below the floor");

        let floor = limiter.limit();
        for _ in 0..20 {
            window(&limiter, ms(10), 4_096).await;
        }
        assert!(limiter.limit() > floor, "RTT recovered: grows again");

        // the open window still carries busy samples: closed once before measuring quiet
        let quiet = window(&limiter, ms(10), 1).await;
        for _ in 0..20 {
            window(&limiter, ms(400), 1).await;
        }
        assert_eq!(limiter.limit(), quiet, "app-limited: unchanged");

        let fixed = Limiter::new(2048, false);
        for _ in 0..20 {
            window(&fixed, ms(400), 4_096).await;
        }
        assert_eq!(fixed.limit(), 2048, "not adaptive: the ceiling");
    }
}
