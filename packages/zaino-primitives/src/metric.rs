//! Metric descriptors: one `METRICS` table per emitting crate, registered by zainod at boot

use metrics::{KeyName, SharedString, Unit};

/// One metric's registration: `# HELP`, unit, histogram bucket edges
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
    Histogram(&'static [f64]),
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

    pub const fn histogram(
        name: &'static str,
        buckets: &'static [f64],
        help: &'static str,
    ) -> Self {
        Self { name, kind: Kind::Histogram(buckets), unit: None, help }
    }

    pub const fn unit(self, unit: Unit) -> Self {
        Self { unit: Some(unit), ..self }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Histogram bucket edges (`None` = not a histogram)
    pub fn buckets(&self) -> Option<&'static [f64]> {
        match self.kind {
            Kind::Histogram(buckets) => Some(buckets),
            Kind::Counter | Kind::CounterFromZero | Kind::Gauge => None,
        }
    }

    /// `# HELP` + unit into the current recorder
    pub fn describe(&self) {
        let (name, help) = (KeyName::from_const_str(self.name), SharedString::const_str(self.help));
        metrics::with_recorder(|recorder| match self.kind {
            Kind::Counter | Kind::CounterFromZero => {
                recorder.describe_counter(name, self.unit, help)
            }
            Kind::Gauge => recorder.describe_gauge(name, self.unit, help),
            Kind::Histogram(_) => recorder.describe_histogram(name, self.unit, help),
        });
        if let Kind::CounterFromZero = self.kind {
            metrics::counter!(self.name).absolute(0);
        }
    }
}
