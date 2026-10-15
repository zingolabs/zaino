//! Read-layer metrics: each engine read, timed once.
//!
//! Every read an adapter makes goes through an [`EngineSnapshot`] port method,
//! so that is where a read is timed: from the call to its answer, whichever
//! tiers or validator ports it took. Each instrumented method owns a static
//! [`Read`] named after it; its metric handles are resolved on first use, so
//! the hot path only records. Without the `prometheus` feature [`measured`] is
//! a plain `.await`. Streams are not timed here: a stream's duration is set by
//! its consumer's pace, and the wire layer already measures streams. The rules
//! every emitting crate follows are in `docs/observability.md`.
//!
//! [`EngineSnapshot`]: crate::EngineSnapshot

use std::future::Future;

#[cfg(feature = "prometheus")]
use std::sync::OnceLock;
#[cfg(feature = "prometheus")]
use std::time::Instant;

use zaino_service::error::{
    AddressReadError, BlockDeltasError, BlockHashReadError, BlockReadError, ReadError,
    SpendReadError, TransactionViewError, TreestateReadError, TxReadError,
};

use crate::routing::PlacementKind;

#[cfg(feature = "prometheus")]
const READ_SECONDS: &str = "zaino.read.duration_seconds";

/// Where a read was answered: the `placement` label.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source {
    /// From the local tiers only.
    Local,
    /// From the validator only.
    Passthrough,
    /// From both: local tiers and validator ports in one read.
    Composed,
}

impl From<PlacementKind> for Source {
    fn from(kind: PlacementKind) -> Self {
        match kind {
            PlacementKind::Local => Source::Local,
            // A withheld read refuses without asking anyone; it is counted with
            // the validator reads, whose refusal it stands in for.
            PlacementKind::Passthrough | PlacementKind::Withheld => Source::Passthrough,
        }
    }
}

impl Source {
    /// A read whose bytes always come from the validator, with a part routed
    /// by `kind`: composed when that part is answered locally.
    pub(crate) fn alongside_validator(kind: PlacementKind) -> Self {
        match kind {
            PlacementKind::Local => Source::Composed,
            PlacementKind::Passthrough | PlacementKind::Withheld => Source::Passthrough,
        }
    }
}

#[cfg(feature = "prometheus")]
impl Source {
    fn label(self) -> &'static str {
        match self {
            Source::Local => "local",
            Source::Passthrough => "passthrough",
            Source::Composed => "composed",
        }
    }
}

/// How a read ended. Classified only when the feature records it; the
/// classification stays compiled so every read error keeps a mapping.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
pub(crate) enum Outcome {
    /// Answered, including a domain miss (`Ok(None)`).
    Ok,
    /// The backing index or source is not built or offered to here.
    NotServiceable,
    /// Likely to resolve on retry.
    Transient,
    /// A read failure retrying cannot fix.
    Fatal,
    /// A typed refusal of the request itself (over a ceiling, malformed).
    Refused,
}

#[cfg(feature = "prometheus")]
impl Outcome {
    const ALL: [Outcome; 5] = [
        Outcome::Ok,
        Outcome::NotServiceable,
        Outcome::Transient,
        Outcome::Fatal,
        Outcome::Refused,
    ];

    fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::NotServiceable => "not_serviceable",
            Outcome::Transient => "transient",
            Outcome::Fatal => "fatal",
            Outcome::Refused => "refused",
        }
    }

    fn slot(self) -> usize {
        match self {
            Outcome::Ok => 0,
            Outcome::NotServiceable => 1,
            Outcome::Transient => 2,
            Outcome::Fatal => 3,
            Outcome::Refused => 4,
        }
    }
}

/// The outcome class of a read error.
pub(crate) trait Failure {
    /// Which [`Outcome`] this error ends a read with.
    #[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
    fn outcome(&self) -> Outcome;
}

/// The three-way read errors share one shape.
#[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
fn read_failure(not_serviceable: bool, transient: bool) -> Outcome {
    match (not_serviceable, transient) {
        (true, _) => Outcome::NotServiceable,
        (false, true) => Outcome::Transient,
        (false, false) => Outcome::Fatal,
    }
}

impl Failure for BlockReadError {
    fn outcome(&self) -> Outcome {
        read_failure(
            matches!(self, Self::NotServiceable(_)),
            matches!(self, Self::Transient(_)),
        )
    }
}

impl Failure for TxReadError {
    fn outcome(&self) -> Outcome {
        read_failure(
            matches!(self, Self::NotServiceable(_)),
            matches!(self, Self::Transient(_)),
        )
    }
}

impl Failure for TreestateReadError {
    fn outcome(&self) -> Outcome {
        read_failure(
            matches!(self, Self::NotServiceable(_)),
            matches!(self, Self::Transient(_)),
        )
    }
}

impl Failure for AddressReadError {
    fn outcome(&self) -> Outcome {
        match self {
            Self::NotServiceable(_) => Outcome::NotServiceable,
            Self::Transient(_) => Outcome::Transient,
            Self::Fatal(_) => Outcome::Fatal,
            Self::TooLarge { .. } | Self::Unsupported(_) => Outcome::Refused,
        }
    }
}

impl Failure for SpendReadError {
    fn outcome(&self) -> Outcome {
        read_failure(
            matches!(self, Self::NotServiceable(_)),
            matches!(self, Self::Transient(_)),
        )
    }
}

impl Failure for ReadError {
    fn outcome(&self) -> Outcome {
        read_failure(
            matches!(self, Self::NotServiceable(_)),
            matches!(self, Self::Transient(_)),
        )
    }
}

impl Failure for TransactionViewError {
    fn outcome(&self) -> Outcome {
        match self {
            Self::Unavailable { .. } => Outcome::Transient,
            Self::MissingPrevout { .. } | Self::PrevoutIndexOutOfRange { .. } => Outcome::Fatal,
            Self::PrevoutFanoutTooLarge { .. } => Outcome::Refused,
        }
    }
}

impl Failure for BlockHashReadError {
    fn outcome(&self) -> Outcome {
        match self {
            Self::TierRead { source } => source.outcome(),
            Self::MissingHeader { .. } => Outcome::Fatal,
        }
    }
}

impl Failure for BlockDeltasError {
    fn outcome(&self) -> Outcome {
        match self {
            Self::TransactionView(source) => source.outcome(),
            Self::Block(source) => source.outcome(),
            Self::MissingHeader { .. } | Self::InputValueOutOfRange { .. } => Outcome::Fatal,
            Self::Orphan { .. } => Outcome::Refused,
        }
    }
}

/// One instrumented engine read: its `read` label, and its series once
/// resolved.
pub(crate) struct Read {
    #[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
    name: &'static str,
    /// Per outcome; the placement is fixed for the process (one routing), so
    /// it is taken from the first call.
    #[cfg(feature = "prometheus")]
    series: OnceLock<[metrics::Histogram; Outcome::ALL.len()]>,
}

impl Read {
    /// The engine read named `name`.
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            name,
            #[cfg(feature = "prometheus")]
            series: OnceLock::new(),
        }
    }
}

/// Run `read_body` as the read `read` from `source`, timing it and recording its
/// outcome.
#[cfg_attr(not(feature = "prometheus"), allow(unused_variables))]
pub(crate) async fn measured<T, E: Failure>(
    read: &'static Read,
    source: impl Into<Source>,
    read_body: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    #[cfg(feature = "prometheus")]
    let started = Instant::now();
    let answer = read_body.await;
    #[cfg(feature = "prometheus")]
    {
        let source = source.into();
        let outcome = match &answer {
            Ok(_) => Outcome::Ok,
            Err(error) => error.outcome(),
        };
        let series = read.series.get_or_init(|| {
            Outcome::ALL.map(|outcome| {
                metrics::histogram!(
                    READ_SECONDS,
                    "read" => read.name,
                    "placement" => source.label(),
                    "outcome" => outcome.label()
                )
            })
        });
        series[outcome.slot()].record(started.elapsed().as_secs_f64());
    }
    answer
}

/// `(metric, bucket edges)` for the exporter: a cached local read is
/// microseconds, an address scan seconds, a validator relay in between.
#[cfg(feature = "prometheus")]
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[(
    READ_SECONDS,
    &[
        10e-6, 50e-6, 100e-6, 250e-6, 500e-6, 1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 100e-3, 1.0, 10.0,
        60.0,
    ],
)];

/// `# HELP` registrations for every metric this crate emits.
#[cfg(feature = "prometheus")]
pub fn describe_metrics() {
    metrics::describe_histogram!(
        READ_SECONDS,
        metrics::Unit::Seconds,
        "One engine read, call to answer, by read, placement and outcome"
    );
}

#[cfg(all(test, feature = "prometheus"))]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use zaino_service::Capability;
    use zaino_service::error::TreestateReadError;

    use super::{Read, Source, measured};

    /// `(read, placement, outcome, samples)` per recorded series.
    fn recorded(recorder: &DebuggingRecorder) -> Vec<(String, String, String, usize)> {
        let mut series = Vec::new();
        for (key, _, _, value) in recorder.snapshotter().snapshot().into_vec() {
            let DebugValue::Histogram(samples) = value else {
                continue;
            };
            if samples.is_empty() {
                continue;
            }
            let label = |name: &str| {
                key.key()
                    .labels()
                    .find(|label| label.key() == name)
                    .map(|label| label.value().to_owned())
                    .unwrap_or_default()
            };
            series.push((
                label("read"),
                label("placement"),
                label("outcome"),
                samples.len(),
            ));
        }
        series.sort();
        series
    }

    /// A read records once per call, under its placement and the outcome its
    /// answer maps to: an answer is `ok`, an unbuilt index `not_serviceable`.
    #[test]
    fn a_read_records_its_placement_and_outcome() {
        let recorder = DebuggingRecorder::new();
        metrics::with_local_recorder(&recorder, || {
            static READ: Read = Read::new("treestate");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a test runtime");
            runtime.block_on(async {
                let answered: Result<u32, TreestateReadError> =
                    measured(&READ, Source::Local, async { Ok(7) }).await;
                assert_eq!(answered.ok(), Some(7));
                let refused: Result<u32, TreestateReadError> =
                    measured(&READ, Source::Local, async {
                        Err(TreestateReadError::NotServiceable(Capability::Treestate))
                    })
                    .await;
                assert!(refused.is_err());
            });
        });
        assert_eq!(
            recorded(&recorder),
            vec![
                (
                    "treestate".into(),
                    "local".into(),
                    "not_serviceable".into(),
                    1
                ),
                ("treestate".into(), "local".into(), "ok".into(), 1),
            ]
        );
    }
}
