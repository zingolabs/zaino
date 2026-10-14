//! Admission outcomes per summary window (`report::INTERVAL`) → refusal rate + overload latch
//!
//! - every outcome recorded here once (a refusal's metric included); read = last closed window
//! - overload = persistent over-capacity, for readiness (backstop: admission sheds within seconds)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::admission::Class;
use crate::emit;

/// Overloaded once every window refuses >= `ENTER_SHARE` of work (at >= `ENTER_REFUSALS_PER_S`)
/// for `SUSTAINED`; cleared once every window stays < `EXIT_SHARE` as long
const SUSTAINED: Duration = Duration::from_secs(5 * 60);
const ENTER_SHARE: f64 = 0.10;
const ENTER_REFUSALS_PER_S: f64 = 1.0;
const EXIT_SHARE: f64 = 0.02;

#[derive(Debug, Default)]
pub(crate) struct Overload {
    admitted_work: AtomicU64,
    refused_work: AtomicU64,
    refused_subscriptions: AtomicU64,
    closed: Mutex<Closed>,
}

/// As of the last closed window (`flipping_for` = how long the opposite condition has held)
#[derive(Debug, Default)]
struct Closed {
    refused_per_s: f64,
    overloaded: bool,
    flipping_for: Duration,
}

impl Overload {
    pub(crate) fn admitted(&self, class: Class) {
        if class == Class::Work {
            self.admitted_work.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn refused(&self, class: Class) {
        emit::stream_rejected(class);
        let refused = match class {
            Class::Work => &self.refused_work,
            Class::Subscription => &self.refused_subscriptions,
        };
        refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Closes the window (`elapsed` long): its refusals, every class
    pub(crate) fn tick(&self, elapsed: Duration) -> u64 {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        let (admitted, refused) = (take(&self.admitted_work), take(&self.refused_work));
        let share = match admitted + refused {
            0 => 0.0,
            attempts => refused as f64 / attempts as f64,
        };
        let refused_per_s = refused as f64 / elapsed.as_secs_f64().max(f64::EPSILON);

        let mut closed = self.closed.lock().expect("overload latch poisoned");
        let flipping = match closed.overloaded {
            false => share >= ENTER_SHARE && refused_per_s >= ENTER_REFUSALS_PER_S,
            true => share < EXIT_SHARE,
        };
        closed.refused_per_s = refused_per_s;
        closed.flipping_for = if flipping { closed.flipping_for + elapsed } else { Duration::ZERO };
        if closed.flipping_for >= SUSTAINED {
            closed.overloaded = !closed.overloaded;
            closed.flipping_for = Duration::ZERO;
        }

        refused + take(&self.refused_subscriptions)
    }

    /// Work refusals per second
    pub(crate) fn refused_per_s(&self) -> f64 {
        self.closed.lock().expect("overload latch poisoned").refused_per_s
    }

    pub(crate) fn overloaded(&self) -> bool {
        self.closed.lock().expect("overload latch poisoned").overloaded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-minute windows of 100 work admissions/s:
    /// - 5 % refused for 10 min: never overloaded (under 10 %)
    /// - 30 % refused (one window dipping to 25 %): overloaded after exactly 5 min
    /// - back to 0 %: cleared after exactly 5 min under 2 %
    /// - 50 % of 1 admission/s (0.5 refusals/s): under the 1/s floor, never overloaded
    /// - a tick = its window's refusals, subscriptions included; `refused_per_s` = work only
    #[test]
    fn overload_latches_after_five_sustained_minutes_and_clears_after_five_quiet() {
        let minute = Duration::from_secs(60);
        // one minute of `per_second` work admissions, `refused_percent` of them refused
        let window = |overload: &Overload, per_second: u64, refused_percent: u64| {
            let total = per_second * 60;
            for _ in 0..total * (100 - refused_percent) / 100 {
                overload.admitted(Class::Work);
            }
            for _ in 0..total * refused_percent / 100 {
                overload.refused(Class::Work);
            }
            overload.tick(minute);
            overload.overloaded()
        };

        let mild = Overload::default();
        let mild: Vec<_> = (0..10).map(|_| window(&mild, 100, 5)).collect();
        assert_eq!(mild, [false; 10], "5 % refused: not overloaded");

        let overload = Overload::default();
        let loaded = [30, 30, 25, 30, 30].map(|refused| window(&overload, 100, refused));
        assert_eq!(loaded, [false, false, false, false, true], "latched at 5 min, dip included");
        let quiet = [0; 5].map(|refused| window(&overload, 100, refused));
        assert_eq!(quiet, [true, true, true, true, false], "cleared at 5 min under 2 %");

        let trickle = Overload::default();
        let trickle: Vec<_> = (0..10).map(|_| window(&trickle, 1, 50)).collect();
        assert_eq!(trickle, [false; 10], "0.5 refusals/s: under the floor");

        let counted = Overload::default();
        for _ in 0..120 {
            counted.refused(Class::Work);
        }
        counted.refused(Class::Subscription);
        assert_eq!(counted.tick(minute), 121, "every class refused");
        assert_eq!(counted.refused_per_s(), 2.0, "work only, per second of the window");
        assert_eq!(counted.tick(minute), 0, "a window starts at zero");
    }
}
