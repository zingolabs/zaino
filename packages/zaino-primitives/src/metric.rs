//! Metric descriptors: one `METRICS` table per emitting crate, registered by zainod at boot
//!
//! - [`CallMetrics`]: the one recording rule for every outbound call family

use std::time::Duration;

use metrics::{KeyName, Label, SharedString, Unit};

/// One metric's registration: `# HELP` + unit (every histogram native: no bucket edges)
#[derive(Debug, Clone, Copy)]
pub struct Metric {
    name: &'static str,
    kind: Kind,
    unit: Option<Unit>,
    help: &'static str,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Counter,
    CounterFromZero,
    Gauge,
    Histogram,
}

impl Metric {
    pub const fn counter(name: &'static str, help: &'static str) -> Self {
        Self { name, kind: Kind::Counter, unit: None, help }
    }

    /// Unlabelled counter published at 0 on registration (unregistered = indistinguishable from
    /// never emitted)
    pub const fn counter_from_zero(name: &'static str, help: &'static str) -> Self {
        Self { name, kind: Kind::CounterFromZero, unit: None, help }
    }

    pub const fn gauge(name: &'static str, help: &'static str) -> Self {
        Self { name, kind: Kind::Gauge, unit: None, help }
    }

    pub const fn histogram(name: &'static str, help: &'static str) -> Self {
        Self { name, kind: Kind::Histogram, unit: None, help }
    }

    pub const fn unit(self, unit: Unit) -> Self {
        Self { unit: Some(unit), ..self }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// `# HELP` + unit into the current recorder
    pub fn describe(&self) {
        let (name, help) = (KeyName::from_const_str(self.name), SharedString::const_str(self.help));
        metrics::with_recorder(|recorder| match self.kind {
            Kind::Counter | Kind::CounterFromZero => {
                recorder.describe_counter(name, self.unit, help)
            }
            Kind::Gauge => recorder.describe_gauge(name, self.unit, help),
            Kind::Histogram => recorder.describe_histogram(name, self.unit, help),
        });
        if let Kind::CounterFromZero = self.kind {
            metrics::counter!(self.name).absolute(0);
        }
    }
}

/// One outbound call family's series (validator RPC, peer requests): calls in flight, the round
/// trip of each answered call, failures by a bounded `failure` label
#[derive(Debug, Clone, Copy)]
pub struct CallMetrics {
    pub in_flight: &'static str,
    pub duration: &'static str,
    pub failures: &'static str,
}

/// How one call settled (`&str` = its bounded `failure` label value)
///
/// - `Refused` = the remote answered with an error: round trip + failure
/// - `Unanswered` = no answer (refused connection, timeout): failure only (µs of nothing or the
///   timeout itself, never a round trip)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome<'a> {
    Answered(Duration),
    Refused(Duration, &'a str),
    Unanswered(&'a str),
}

impl CallMetrics {
    /// Gauge up until the guard drops (whatever ends the call, a cancel included)
    pub fn in_flight(&self, labels: &[Label]) -> InFlight {
        let gauge = metrics::gauge!(self.in_flight, labels.iter());
        gauge.increment(1.0);
        InFlight(gauge)
    }

    pub fn settled(&self, labels: &[Label], outcome: CallOutcome<'_>) {
        let (round_trip, failure) = match outcome {
            CallOutcome::Answered(took) => (Some(took), None),
            CallOutcome::Refused(took, failure) => (Some(took), Some(failure)),
            CallOutcome::Unanswered(failure) => (None, Some(failure)),
        };
        if let Some(took) = round_trip {
            metrics::histogram!(self.duration, labels.iter()).record(took.as_secs_f64());
        }
        if let Some(failure) = failure {
            let mut labels = labels.to_vec();
            labels.push(Label::new("failure", failure.to_owned()));
            metrics::counter!(self.failures, labels).increment(1);
        }
    }
}

/// One call in flight ([`CallMetrics::in_flight`])
#[derive(Debug)]
#[must_use = "dropping the guard ends the call's in-flight count"]
pub struct InFlight(metrics::Gauge);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.decrement(1.0);
    }
}
