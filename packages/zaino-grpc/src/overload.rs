//! Overload latch per summary window (`summary::INTERVAL`) → readiness
//!
//! - one latch per [`OverloadCause`]: over its threshold for its `enter` → latched, under half of
//!   it (refusals: under 2 %) for its `exit` → cleared; reported = the first latched
//! - signals: admission refusals (counted here), network queueing or loss (`network.rs`, either =
//!   saturated), CPU / IO pressure (`pressure.rs`)
//! - operator settings live (`set`): while detection is off the latches still step, none reported

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tracing::{info, warn};

use crate::admission::Class;
use crate::metrics;
use crate::OverloadLimits;

/// Refusals: >= ENTER_SHARE of work (at >= ENTER_REFUSALS_PER_S) / < EXIT_SHARE
const ENTER_SHARE: f64 = 0.10;
const ENTER_REFUSALS_PER_S: f64 = 1.0;
const EXIT_SHARE: f64 = 0.02;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverloadCause {
    NetworkSaturated,
    CpuPressure,
    IoPressure,
    RefusingWork,
}

impl OverloadCause {
    const ALL: [Self; 4] =
        [Self::NetworkSaturated, Self::CpuPressure, Self::IoPressure, Self::RefusingWork];

    /// Readiness `detail.message`
    pub const fn detail(self) -> &'static str {
        match self {
            Self::NetworkSaturated => "Overloaded: network saturated",
            Self::CpuPressure => "Overloaded: CPU pressure",
            Self::IoPressure => "Overloaded: IO pressure",
            Self::RefusingWork => "Overloaded: refusing work",
        }
    }

    /// `zaino_grpc_overload_latched{cause}`
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NetworkSaturated => "network_saturated",
            Self::CpuPressure => "cpu_pressure",
            Self::IoPressure => "io_pressure",
            Self::RefusingWork => "refusing_work",
        }
    }

    /// (enter, exit)
    fn rule(self) -> (Duration, Duration) {
        let minutes = |m: u64| Duration::from_secs(m * 60);
        match self {
            Self::NetworkSaturated | Self::CpuPressure | Self::IoPressure => {
                (minutes(2), minutes(3))
            }
            // admission already sheds within seconds: only a persistent shortfall counts
            Self::RefusingWork => (minutes(5), minutes(5)),
        }
    }
}

/// One window's signals (`None` = not measured here)
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Signals {
    pub(crate) network_delay: Option<Duration>,
    pub(crate) network_loss: Option<f64>,
    pub(crate) cpu: Option<f64>,
    pub(crate) io: Option<f64>,
}

/// One cause's state (`flipping_for` = how long the opposite condition has held)
#[derive(Clone, Copy, Debug, Default)]
struct Latch {
    held: bool,
    flipping_for: Duration,
}

/// `value` against `limit` (0 = off): (over, under half)
fn against(value: Option<f64>, limit: f64) -> (bool, bool) {
    match value {
        Some(value) if limit > 0.0 => (value >= limit, value < limit / 2.0),
        _ => (false, true),
    }
}

#[derive(Debug)]
pub(crate) struct Overload {
    admitted_work: AtomicU64,
    refused_work: AtomicU64,
    refused_subscriptions: AtomicU64,
    closed: Mutex<Closed>,
}

/// As of the last closed window + the operator's settings
#[derive(Debug)]
struct Closed {
    limits: OverloadLimits,
    detection: bool,
    refused_per_s: f64,
    latches: [Latch; OverloadCause::ALL.len()],
}

impl Overload {
    pub(crate) fn new(limits: OverloadLimits) -> Self {
        let closed =
            Closed { limits, detection: true, refused_per_s: 0.0, latches: Default::default() };
        Self {
            admitted_work: AtomicU64::new(0),
            refused_work: AtomicU64::new(0),
            refused_subscriptions: AtomicU64::new(0),
            closed: Mutex::new(closed),
        }
    }

    pub(crate) fn set(&self, limits: OverloadLimits, detection: bool) {
        let mut closed = self.closed.lock().expect("overload latch poisoned");
        (closed.limits, closed.detection) = (limits, detection);
    }

    pub(crate) fn limits(&self) -> OverloadLimits {
        self.closed.lock().expect("overload latch poisoned").limits.clone()
    }

    pub(crate) fn admitted(&self, class: Class) {
        if class == Class::Work {
            self.admitted_work.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn refused(&self, class: Class) {
        metrics::stream_rejected(class);
        let refused = match class {
            Class::Work => &self.refused_work,
            Class::Subscription => &self.refused_subscriptions,
        };
        refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Closes the window (`elapsed` long) on its `signals`: its refusals, every class
    pub(crate) fn tick(&self, elapsed: Duration, signals: Signals) -> u64 {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        let (admitted, refused) = (take(&self.admitted_work), take(&self.refused_work));
        let share = match admitted + refused {
            0 => 0.0,
            attempts => refused as f64 / attempts as f64,
        };
        let refused_per_s = refused as f64 / elapsed.as_secs_f64().max(f64::EPSILON);

        let mut closed = self.closed.lock().expect("overload latch poisoned");
        let limits = &closed.limits;
        let percent = |p: u8| f64::from(p) / 100.0;
        let conditions = OverloadCause::ALL.map(|cause| match cause {
            OverloadCause::NetworkSaturated => {
                let (queued, unqueued) = against(
                    signals.network_delay.map(|delay| delay.as_secs_f64()),
                    f64::from(limits.network_queue_delay_ms) / 1000.0,
                );
                let (lossy, clean) =
                    against(signals.network_loss, percent(limits.network_retransmit_percent));
                (queued || lossy, unqueued && clean)
            }
            OverloadCause::CpuPressure => {
                against(signals.cpu, percent(limits.cpu_pressure_percent))
            }
            OverloadCause::IoPressure => against(signals.io, percent(limits.io_pressure_percent)),
            OverloadCause::RefusingWork => {
                (share >= ENTER_SHARE && refused_per_s >= ENTER_REFUSALS_PER_S, share < EXIT_SHARE)
            }
        });
        let detection = closed.detection;
        for (cause, (over, under)) in OverloadCause::ALL.into_iter().zip(conditions) {
            let latch = &mut closed.latches[cause as usize];
            let (enter, exit) = cause.rule();
            let (flipping, needed) = if latch.held { (under, exit) } else { (over, enter) };
            latch.flipping_for =
                if flipping { latch.flipping_for + elapsed } else { Duration::ZERO };
            if latch.flipping_for >= needed {
                (latch.held, latch.flipping_for) = (!latch.held, Duration::ZERO);
                let cause = cause.detail();
                match latch.held {
                    true => warn!(cause, detection, ?signals, refused = share, "Overload latched"),
                    false => info!(cause, ?signals, refused = share, "Overload cleared"),
                }
            }
            metrics::overload_latched(cause, latch.held);
        }
        closed.refused_per_s = refused_per_s;

        refused + take(&self.refused_subscriptions)
    }

    /// Work refusals per second
    pub(crate) fn refused_per_s(&self) -> f64 {
        self.closed.lock().expect("overload latch poisoned").refused_per_s
    }

    /// The first latched cause (`None` while detection is off)
    pub(crate) fn cause(&self) -> Option<OverloadCause> {
        let closed = self.closed.lock().expect("overload latch poisoned");
        let latched =
            OverloadCause::ALL.into_iter().find(|&cause| closed.latches[cause as usize].held);
        latched.filter(|_| closed.detection)
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
    /// - tick = its window's refusals, subscriptions included
    /// - `refused_per_s` = work only
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
            overload.tick(minute, Signals::default());
            overload.cause()
        };
        let new = || Overload::new(OverloadLimits::default());
        let refusing = Some(OverloadCause::RefusingWork);

        let mild = new();
        let mild: Vec<_> = (0..10).map(|_| window(&mild, 100, 5)).collect();
        assert_eq!(mild, [None; 10], "5 % refused: not overloaded");

        let overload = new();
        let loaded = [30, 30, 25, 30, 30].map(|refused| window(&overload, 100, refused));
        assert_eq!(loaded, [None, None, None, None, refusing], "latched at 5 min, dip included");
        let quiet = [0; 5].map(|refused| window(&overload, 100, refused));
        assert_eq!(quiet, [refusing, refusing, refusing, refusing, None], "cleared at 5 min");

        let trickle = new();
        let trickle: Vec<_> = (0..10).map(|_| window(&trickle, 1, 50)).collect();
        assert_eq!(trickle, [None; 10], "0.5 refusals/s: under the floor");

        let counted = new();
        for _ in 0..120 {
            counted.refused(Class::Work);
        }
        counted.refused(Class::Subscription);
        assert_eq!(counted.tick(minute, Signals::default()), 121, "every class refused");
        assert_eq!(counted.refused_per_s(), 2.0, "work only, per second of the window");
        assert_eq!(counted.tick(minute, Signals::default()), 0, "a window starts at zero");
    }

    /// 10 ms / 2 % network, 25 % CPU, one-minute windows:
    /// - 14 ms + 30 % CPU: both latch at 2 min, network named first
    /// - 7 ms (not under 5) holds; 3 windows unmeasured clear network, CPU named
    /// - 3 % loss without queueing latches network; 1.5 % (not under 1) holds, then clears
    /// - detection off: nothing named; a limit of 0 = that cause off (clears by its exit)
    #[test]
    fn each_cause_latches_on_its_rule_and_the_operator_settings_apply() {
        let minute = Duration::from_secs(60);
        let window = |overload: &Overload, delay_ms: Option<u64>, loss: Option<f64>, cpu: f64| {
            let network_delay = delay_ms.map(Duration::from_millis);
            let (cpu, io) = (Some(cpu), Some(0.0));
            overload.tick(minute, Signals { network_delay, network_loss: loss, cpu, io });
            overload.cause()
        };
        let network = Some(OverloadCause::NetworkSaturated);
        let cpu = Some(OverloadCause::CpuPressure);
        let limits = OverloadLimits {
            network_queue_delay_ms: 10,
            network_retransmit_percent: 2,
            ..OverloadLimits::default()
        };

        let overload = Overload::new(limits.clone());
        let loaded = [Some(14), Some(14), Some(7), None, None, None]
            .map(|ms| window(&overload, ms, None, 0.3));
        assert_eq!(loaded, [None, network, network, network, network, cpu]);

        let policed = Overload::new(limits.clone());
        let dropping = [0.03, 0.03, 0.015, 0.0, 0.0, 0.0]
            .map(|loss| window(&policed, Some(2), Some(loss), 0.0));
        assert_eq!(dropping, [None, network, network, network, network, None]);

        overload.set(limits.clone(), false);
        assert_eq!(overload.cause(), None, "detection off");
        overload.set(OverloadLimits { cpu_pressure_percent: 0, ..limits }, true);
        let off = [0; 3].map(|_| window(&overload, None, None, 1.0));
        assert_eq!(off, [cpu, cpu, None], "0 = off");
    }
}
