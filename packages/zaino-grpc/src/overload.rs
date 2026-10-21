//! Overload latch per summary window (`summary::INTERVAL`): admission refusals, egress queue and
//! retransmits (`egress.rs`), CPU / IO pressure (`pressure.rs`) → readiness
//!
//! - every admission outcome recorded here once (a refusal's metric included)
//! - read = last closed window
//! - overload = persistent lack of capacity, for a load balancer (admission sheds within seconds)
//! - one latch per [`OverloadCause`]: enters when over its threshold for its `enter`, clears when
//!   under half of it (refusals: under 2 %) for its `exit`; overloaded = any latched

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use tracing::{info, warn};

use crate::admission::Class;
use crate::egress::WindowVerdict;
use crate::metrics;
use crate::pressure::PressureShares;
use crate::OverloadLimits;

/// Refusals: >= ENTER_SHARE of work (at >= ENTER_REFUSALS_PER_S) / < EXIT_SHARE
const ENTER_SHARE: f64 = 0.10;
const ENTER_REFUSALS_PER_S: f64 = 1.0;
const EXIT_SHARE: f64 = 0.02;

const MINUTE: Duration = Duration::from_secs(60);

/// Why the node is overloaded: the first latched, in this order (most specific first)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverloadCause {
    EgressQueue,
    Retransmits,
    CpuPressure,
    IoPressure,
    RefusingWork,
}

impl OverloadCause {
    const ALL: [Self; 5] = [
        Self::EgressQueue,
        Self::Retransmits,
        Self::CpuPressure,
        Self::IoPressure,
        Self::RefusingWork,
    ];

    /// Metric label (`zaino_grpc_overloaded{cause}`)
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::EgressQueue => "egress_queue",
            Self::Retransmits => "retransmits",
            Self::CpuPressure => "cpu_pressure",
            Self::IoPressure => "io_pressure",
            Self::RefusingWork => "refusing_work",
        }
    }

    /// Readiness `detail`
    pub const fn detail(self) -> &'static str {
        match self {
            Self::EgressQueue => "Overloaded: egress queue",
            Self::Retransmits => "Overloaded: retransmits",
            Self::CpuPressure => "Overloaded: CPU pressure",
            Self::IoPressure => "Overloaded: IO pressure",
            Self::RefusingWork => "Overloaded: refusing work",
        }
    }

    /// How long over enters, how long under clears
    const fn rule(self) -> (Duration, Duration) {
        match self {
            // queueing on the link: a window's median already spans 12 samples
            Self::EgressQueue => (MINUTE, Duration::from_secs(3 * 60)),
            Self::Retransmits | Self::CpuPressure | Self::IoPressure => {
                (Duration::from_secs(2 * 60), Duration::from_secs(3 * 60))
            }
            // admission already sheds within seconds: only a persistent shortfall counts
            Self::RefusingWork => (Duration::from_secs(5 * 60), Duration::from_secs(5 * 60)),
        }
    }
}

/// One cause's state (`flipping_for` = how long the opposite condition has held)
#[derive(Clone, Copy, Debug, Default)]
struct Latch {
    held: bool,
    flipping_for: Duration,
}

impl Latch {
    /// One window (`elapsed` long) where the cause is `over` its threshold / `under` its exit
    fn step(
        &mut self,
        over: bool,
        under: bool,
        elapsed: Duration,
        (enter, exit): (Duration, Duration),
    ) {
        let (flipping, needed) = match self.held {
            false => (over, enter),
            true => (under, exit),
        };
        self.flipping_for = if flipping { self.flipping_for + elapsed } else { Duration::ZERO };
        if self.flipping_for >= needed {
            self.held = !self.held;
            self.flipping_for = Duration::ZERO;
        }
    }
}

/// `value` against `percent` (0 = off): over at it, under below half of it, none = under
fn against(value: Option<f64>, percent: u8) -> (bool, bool) {
    let limit = f64::from(percent) / 100.0;
    match value {
        _ if percent == 0 => (false, true),
        Some(value) => (value >= limit, value < limit / 2.0),
        None => (false, true),
    }
}

#[derive(Debug)]
pub(crate) struct Overload {
    admitted_work: AtomicU64,
    refused_work: AtomicU64,
    refused_subscriptions: AtomicU64,
    closed: Mutex<Closed>,
}

/// As of the last closed window + the operator's settings (live: [`Overload::set`])
///
/// - `cause` = the latched one, reported only while `detection` (latches step either way: the
///   signals stay measured, a re-enable names what holds now)
#[derive(Debug)]
struct Closed {
    limits: OverloadLimits,
    detection: bool,
    refused_per_s: f64,
    latches: [Latch; OverloadCause::ALL.len()],
    cause: Option<OverloadCause>,
}

impl Overload {
    pub(crate) fn new(limits: OverloadLimits) -> Self {
        let closed = Closed {
            limits,
            detection: true,
            refused_per_s: 0.0,
            latches: Default::default(),
            cause: None,
        };
        Self {
            admitted_work: AtomicU64::new(0),
            refused_work: AtomicU64::new(0),
            refused_subscriptions: AtomicU64::new(0),
            closed: Mutex::new(closed),
        }
    }

    /// The operator's thresholds + whether a latched cause is reported, from the next window
    pub(crate) fn set(&self, limits: OverloadLimits, detection: bool) {
        let mut closed = self.closed.lock().expect("overload latch poisoned");
        (closed.limits, closed.detection) = (limits, detection);
    }

    pub(crate) fn egress_min_networks(&self) -> usize {
        self.closed.lock().expect("overload latch poisoned").limits.egress_min_networks.get()
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

    /// Closes the window (`elapsed` long) with its egress + pressure verdicts → its refusals, every
    /// class
    pub(crate) fn tick(
        &self,
        elapsed: Duration,
        egress: WindowVerdict,
        pressure: PressureShares,
    ) -> u64 {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        let (admitted, refused) = (take(&self.admitted_work), take(&self.refused_work));
        let share = match admitted + refused {
            0 => 0.0,
            attempts => refused as f64 / attempts as f64,
        };
        let refused_per_s = refused as f64 / elapsed.as_secs_f64().max(f64::EPSILON);

        let mut closed = self.closed.lock().expect("overload latch poisoned");
        let limits = closed.limits.clone();
        let delay_limit = Duration::from_millis(limits.egress_queue_delay_ms.get().into());
        let conditions = |cause| match cause {
            OverloadCause::EgressQueue => (
                egress.queue_delay.is_some_and(|delay| delay >= delay_limit),
                egress.queue_delay.is_none_or(|delay| delay < delay_limit / 2),
            ),
            OverloadCause::Retransmits => against(egress.retransmit, limits.retransmit_percent),
            OverloadCause::CpuPressure => against(pressure.cpu, limits.cpu_pressure_percent),
            OverloadCause::IoPressure => against(pressure.io, limits.io_pressure_percent),
            OverloadCause::RefusingWork => {
                (share >= ENTER_SHARE && refused_per_s >= ENTER_REFUSALS_PER_S, share < EXIT_SHARE)
            }
        };

        for (latch, cause) in closed.latches.iter_mut().zip(OverloadCause::ALL) {
            let (over, under) = conditions(cause);
            latch.step(over, under, elapsed, cause.rule());
        }
        let cause = OverloadCause::ALL
            .into_iter()
            .zip(closed.latches)
            .find(|(_, l)| l.held)
            .map(|(c, _)| c);
        let detection = closed.detection;
        match (closed.cause, cause) {
            (None, Some(cause)) => {
                warn!(cause = cause.label(), detection, ?egress, ?pressure, "Overloaded")
            }
            (Some(was), None) => info!(cause = was.label(), "Overload cleared"),
            _ => {}
        }
        closed.refused_per_s = refused_per_s;
        closed.cause = cause;
        let named = cause.filter(|_| detection);
        let causes = OverloadCause::ALL.map(|c| (c.label(), Some(c) == named));
        metrics::overload_window(egress, pressure, detection, causes);

        refused + take(&self.refused_subscriptions)
    }

    /// Work refusals per second
    pub(crate) fn refused_per_s(&self) -> f64 {
        self.closed.lock().expect("overload latch poisoned").refused_per_s
    }

    /// The latched cause readiness names (`None` while detection is off)
    pub(crate) fn cause(&self) -> Option<OverloadCause> {
        let closed = self.closed.lock().expect("overload latch poisoned");
        closed.cause.filter(|_| closed.detection)
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
        let quiet = |overload: &Overload| {
            overload.tick(minute, WindowVerdict::default(), PressureShares::default())
        };
        // one minute of `per_second` work admissions, `refused_percent` of them refused
        let window = |overload: &Overload, per_second: u64, refused_percent: u64| {
            let total = per_second * 60;
            for _ in 0..total * (100 - refused_percent) / 100 {
                overload.admitted(Class::Work);
            }
            for _ in 0..total * refused_percent / 100 {
                overload.refused(Class::Work);
            }
            quiet(overload);
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
        let cleared = [0; 5].map(|refused| window(&overload, 100, refused));
        assert_eq!(
            cleared,
            [refusing, refusing, refusing, refusing, None],
            "cleared at 5 min under 2 %"
        );

        let trickle = new();
        let trickle: Vec<_> = (0..10).map(|_| window(&trickle, 1, 50)).collect();
        assert_eq!(trickle, [None; 10], "0.5 refusals/s: under the floor");

        let counted = new();
        for _ in 0..120 {
            counted.refused(Class::Work);
        }
        counted.refused(Class::Subscription);
        assert_eq!(quiet(&counted), 121, "every class refused");
        assert_eq!(counted.refused_per_s(), 2.0, "work only, per second of the window");
        assert_eq!(quiet(&counted), 0, "a window starts at zero");
    }

    /// One-minute windows against the default limits (10 ms queue, 5 % retransmits, 25 % CPU):
    /// - egress queue: latched after 1 window at 14 ms; 7 ms (under 10, not under 5) holds it and
    ///   restarts the exit; cleared after 3 windows under 5 ms or without a verdict (breadth lost)
    /// - CPU pressure: 2 windows at 30 % to latch; the egress cause named first while both hold
    /// - a percent of 0 turns its cause off
    #[test]
    fn each_cause_latches_on_its_own_window_rule_and_the_most_specific_is_named() {
        let minute = Duration::from_secs(60);
        let ms = |ms| Some(Duration::from_millis(ms));
        let window = |overload: &Overload, delay, cpu| {
            let egress = WindowVerdict { networks: 40, queue_delay: delay, retransmit: Some(0.0) };
            overload.tick(minute, egress, PressureShares { cpu, io: Some(0.0) });
            overload.cause()
        };
        let (egress, cpu) = (Some(OverloadCause::EgressQueue), Some(OverloadCause::CpuPressure));

        let overload = Overload::new(OverloadLimits::default());
        let queued = [(ms(14), Some(0.3)); 2].map(|(delay, share)| window(&overload, delay, share));
        assert_eq!(queued, [egress, egress], "egress at once; CPU latches at 2, egress named");
        let easing =
            [ms(4), ms(7), ms(4), ms(4), None].map(|delay| window(&overload, delay, Some(0.3)));
        assert_eq!(
            easing,
            [egress, egress, egress, egress, cpu],
            "7 ms restarts the 3-window exit"
        );
        let calm = [(None, Some(0.1)); 3].map(|(delay, share)| window(&overload, delay, share));
        assert_eq!(calm, [cpu, cpu, None], "CPU clear after 3 windows under 12.5 %");

        let off =
            Overload::new(OverloadLimits { cpu_pressure_percent: 0, ..OverloadLimits::default() });
        let ignored = [Some(1.0); 4].map(|share| window(&off, None, share));
        assert_eq!(ignored, [None; 4], "cpu_pressure_percent = 0: off");
    }

    /// The operator's settings, live (`set`), at 30 % CPU pressure every minute:
    /// - detection off: the latch still steps, nothing reported; on again = named at once
    /// - threshold raised to 80 %: 30 % is under half of it, cleared by the usual 3-window exit
    /// - `egress_min_networks` read back as set (the sampler's breadth rule)
    #[test]
    fn operator_settings_apply_from_the_next_window() {
        let minute = Duration::from_secs(60);
        let window = |overload: &Overload| {
            let egress = WindowVerdict::default();
            overload.tick(minute, egress, PressureShares { cpu: Some(0.3), io: Some(0.0) });
            overload.cause()
        };
        let cpu = Some(OverloadCause::CpuPressure);
        let defaults = OverloadLimits::default();

        let overload = Overload::new(defaults.clone());
        overload.set(defaults.clone(), false);
        assert_eq!([(); 3].map(|_| window(&overload)), [None; 3], "detection off: none named");
        overload.set(defaults.clone(), true);
        assert_eq!(overload.cause(), cpu, "on again: the cause latched meanwhile, at once");

        let relaxed = OverloadLimits {
            cpu_pressure_percent: 80,
            egress_min_networks: std::num::NonZeroUsize::new(3).expect("3 is non-zero"),
            ..defaults
        };
        overload.set(relaxed, true);
        assert_eq!(overload.egress_min_networks(), 3);
        assert_eq!([(); 3].map(|_| window(&overload)), [cpu, cpu, None], "under 40 % for 3 min");
    }
}
