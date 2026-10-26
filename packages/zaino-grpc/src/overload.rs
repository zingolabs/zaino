//! Overload latch per summary window (`summary::INTERVAL`) → readiness
//!
//! - one latch per [`OverloadCause`]: over its threshold for its `enter` → latched, under half of
//!   it (refusals: under 2 %) for its `exit` → cleared; reported = the first latched
//! - signals: admission refusals (counted here), CPU / IO pressure (`pressure.rs`)
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
    CpuPressure,
    IoPressure,
    RefusingWork,
}

impl OverloadCause {
    const ALL: [Self; 3] = [Self::CpuPressure, Self::IoPressure, Self::RefusingWork];

    /// Readiness `detail.message`
    pub const fn detail(self) -> &'static str {
        match self {
            Self::CpuPressure => "Overloaded: CPU pressure",
            Self::IoPressure => "Overloaded: IO pressure",
            Self::RefusingWork => "Overloaded: refusing work",
        }
    }

    /// `zaino_grpc_overload_latched{cause}`
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::CpuPressure => "cpu_pressure",
            Self::IoPressure => "io_pressure",
            Self::RefusingWork => "refusing_work",
        }
    }

    /// (enter, exit)
    fn rule(self) -> (Duration, Duration) {
        let minutes = |m: u64| Duration::from_secs(m * 60);
        match self {
            Self::CpuPressure | Self::IoPressure => (minutes(2), minutes(3)),
            // admission already sheds within seconds: only a persistent shortfall counts
            Self::RefusingWork => (minutes(5), minutes(5)),
        }
    }
}

/// One window's signals (`None` = not measured here)
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Signals {
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

    const MINUTE: Duration = Duration::from_secs(60);

    /// One minute of `per_second` work admissions, `refused_percent` of them refused → the cause
    fn refusal_minute(
        overload: &Overload,
        per_second: u64,
        refused_percent: u64,
    ) -> Option<OverloadCause> {
        let total = per_second * 60;
        for _ in 0..total * (100 - refused_percent) / 100 {
            overload.admitted(Class::Work);
        }
        for _ in 0..total * refused_percent / 100 {
            overload.refused(Class::Work);
        }
        overload.tick(MINUTE, Signals::default());
        overload.cause()
    }

    /// Enter: >= 10 % refused at >= 1 refusal/s for 5 min; exit: < 2 % for 5 min
    #[test]
    fn refusing_work_latches_after_five_sustained_minutes_and_clears_after_five_quiet() {
        let (none, refusing) = (None, Some(OverloadCause::RefusingWork));
        let latched_5 = [vec![none; 4], vec![refusing]].concat();
        // case, work admissions/s, % refused each minute, cause after each minute
        #[rustfmt::skip]
        let cases = [
            ("9 % for 10 min",                100, vec![9; 10],                       vec![none; 10]),
            ("10 % for 5 min",                100, vec![10; 5],                       latched_5.clone()),
            ("30 %, one minute at 25 %",      100, vec![30, 30, 25, 30, 30],          latched_5.clone()),
            ("50 % of 1/s = 0.5 refusals/s",  1,   vec![50; 10],                      vec![none; 10]),
            ("latched, then 2 % for 5 min",   100, [vec![10; 5], vec![2; 5]].concat(), [latched_5.clone(), vec![refusing; 5]].concat()),
            ("latched, then 1 % for 5 min",   100, [vec![10; 5], vec![1; 5]].concat(), [latched_5.clone(), vec![refusing; 4], vec![none]].concat()),
        ];
        for (case, per_second, refused_percent, causes) in cases {
            let overload = Overload::new(OverloadLimits::default());

            let latched: Vec<_> =
                refused_percent.iter().map(|&p| refusal_minute(&overload, per_second, p)).collect();

            assert_eq!(latched, causes, "{case}: cause after each minute");
        }
    }

    #[test]
    fn a_window_counts_every_refusal_and_reports_work_refusals_per_second() {
        let overload = Overload::new(OverloadLimits::default());
        for _ in 0..120 {
            overload.refused(Class::Work);
        }
        overload.refused(Class::Subscription);

        let refused = overload.tick(MINUTE, Signals::default());

        assert_eq!(refused, 121, "120 work + 1 subscription refused");
        assert_eq!(overload.refused_per_s(), 2.0, "120 work refusals over 60 s");
        assert_eq!(overload.tick(MINUTE, Signals::default()), 0, "next window starts at zero");
    }

    /// One minute's pressure shares (`None` = unmeasured)
    fn signals(cpu: Option<f64>, io: Option<f64>) -> Signals {
        Signals { cpu, io }
    }

    /// CPU / IO 25 %: enter over the limit for 2 min, exit under half of it for 3
    #[test]
    fn each_pressure_cause_latches_over_its_limit_and_clears_under_half_of_it() {
        let (none, cpu, io) =
            (None, Some(OverloadCause::CpuPressure), Some(OverloadCause::IoPressure));
        let limits = OverloadLimits::default();
        #[rustfmt::skip]
        let cases = [
            ("CPU 30 %; 15 % (not under 12.5) holds; unmeasured 3 min clears",
                [signals(Some(0.3), None), signals(Some(0.3), None), signals(Some(0.15), None),
                 signals(None, None), signals(None, None), signals(None, None)],
                [none, cpu, cpu, cpu, cpu, none]),
            ("IO 25 % exactly latches; 20 % holds; 10 % for 3 min clears",
                [signals(None, Some(0.25)), signals(None, Some(0.25)), signals(None, Some(0.2)),
                 signals(None, Some(0.1)), signals(None, Some(0.1)), signals(None, Some(0.1))],
                [none, io, io, io, io, none]),
            ("CPU + IO both latch: CPU named first, then IO",
                [signals(Some(0.3), Some(0.3)), signals(Some(0.3), Some(0.3)), signals(Some(0.15), Some(0.3)),
                 signals(Some(0.1), Some(0.3)), signals(Some(0.1), Some(0.3)), signals(Some(0.1), Some(0.3))],
                [none, cpu, cpu, cpu, cpu, io]),
        ];
        for (case, minutes, causes) in cases {
            let overload = Overload::new(limits.clone());

            let latched = minutes.map(|minute| {
                overload.tick(MINUTE, minute);
                overload.cause()
            });

            assert_eq!(latched, causes, "{case}: cause after each minute");
        }
    }

    #[test]
    fn operator_settings_apply_to_a_latched_cause_live() {
        let cpu = Some(OverloadCause::CpuPressure);
        let limits = OverloadLimits { cpu_pressure_percent: 25, ..OverloadLimits::default() };
        let overload = Overload::new(limits.clone());
        for _ in 0..2 {
            overload.tick(MINUTE, signals(Some(0.3), None));
        }
        assert_eq!(overload.cause(), cpu, "CPU 30 % for 2 min: latched");

        overload.set(limits.clone(), false);
        let detection_off = overload.cause();
        overload.set(OverloadLimits { cpu_pressure_percent: 0, ..limits }, true);
        let limit_off = [0; 3].map(|_| {
            overload.tick(MINUTE, signals(Some(1.0), None));
            overload.cause()
        });

        assert_eq!(detection_off, None, "detection off: latched, never reported");
        assert_eq!(limit_off, [cpu, cpu, None], "CPU limit 0 = off: clears by its 3 min exit");
    }
}
