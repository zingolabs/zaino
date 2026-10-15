//! Adaptive work-stream limit: Netflix concurrency-limits' Gradient2 over server time per message
//!
//! - sample = time the server spent producing one message (admission → first DATA frame, then
//!   each poll → the next)
//! - client backpressure never counts (hyper stops polling)
//! - per window: `limit × clamp(TOLERANCE × long / short, 0.5, 1) + √limit`, smoothed, in
//!   `floor..=ceiling` (`max_work_streams`)
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
    window: GradientWindow,
}

#[derive(Debug, Default)]
struct GradientWindow {
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
            Gradient { estimated: initial as f64, long: None, window: GradientWindow::default() };
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
