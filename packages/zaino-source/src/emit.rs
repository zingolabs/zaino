//! Source-port metrics: one trip to the validator, retries included.
//!
//! Every resilient port goes through [`ValidatorClient::with_retry`]
//! (crate::ValidatorClient), so a trip is timed there once: from the first
//! attempt to the answer the port returns, every retry and backoff sleep
//! included. Each generated port method owns a static [`Port`], whose name is
//! the method's and whose metric handles are resolved on first use. Without the
//! `prometheus` feature a [`Trip`] is zero-sized and records nothing. The rules
//! every emitting crate follows are in `docs/observability.md`.

#[cfg(feature = "prometheus")]
use std::sync::OnceLock;
#[cfg(feature = "prometheus")]
use std::time::Instant;

#[cfg(feature = "prometheus")]
const REQUEST_SECONDS: &str = "zaino.source.request_seconds";
#[cfg(feature = "prometheus")]
const ATTEMPTS: &str = "zaino.source.attempts";

/// How a trip ended, in the source's own error vocabulary.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Outcome {
    /// The port answered.
    Ok,
    /// The validator's considered rejection.
    Domain,
    /// A failure retrying cannot fix (parse, auth, a non-retryable code).
    NonDomain,
    /// Retries spent: the validator is unreachable.
    Unavailable,
}

#[cfg(feature = "prometheus")]
impl Outcome {
    const ALL: [Outcome; 4] = [
        Outcome::Ok,
        Outcome::Domain,
        Outcome::NonDomain,
        Outcome::Unavailable,
    ];

    fn label(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Domain => "domain",
            Outcome::NonDomain => "non_domain",
            Outcome::Unavailable => "unavailable",
        }
    }

    fn slot(self) -> usize {
        match self {
            Outcome::Ok => 0,
            Outcome::Domain => 1,
            Outcome::NonDomain => 2,
            Outcome::Unavailable => 3,
        }
    }
}

/// One resilient port method: its `port` label, and its series once resolved.
pub(crate) struct Port {
    #[cfg_attr(not(feature = "prometheus"), allow(dead_code))]
    name: &'static str,
    #[cfg(feature = "prometheus")]
    series: OnceLock<PortSeries>,
}

#[cfg(feature = "prometheus")]
struct PortSeries {
    request: [metrics::Histogram; Outcome::ALL.len()],
    attempts: metrics::Histogram,
}

impl Port {
    /// The port method named `name`.
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            name,
            #[cfg(feature = "prometheus")]
            series: OnceLock::new(),
        }
    }

    /// Start timing one trip through this port.
    pub(crate) fn trip(&'static self) -> Trip {
        Trip {
            #[cfg(feature = "prometheus")]
            port: self,
            #[cfg(feature = "prometheus")]
            started: Instant::now(),
        }
    }

    #[cfg(feature = "prometheus")]
    fn series(&self) -> &PortSeries {
        self.series.get_or_init(|| PortSeries {
            request: Outcome::ALL.map(|outcome| {
                metrics::histogram!(
                    REQUEST_SECONDS,
                    "port" => self.name,
                    "outcome" => outcome.label()
                )
            }),
            attempts: metrics::histogram!(ATTEMPTS, "port" => self.name),
        })
    }
}

/// One trip in flight through a [`Port`].
pub(crate) struct Trip {
    #[cfg(feature = "prometheus")]
    port: &'static Port,
    #[cfg(feature = "prometheus")]
    started: Instant,
}

impl Trip {
    /// The trip ended with `outcome` after `attempts` attempts.
    #[cfg_attr(not(feature = "prometheus"), allow(unused_variables))]
    pub(crate) fn finish(self, outcome: Outcome, attempts: u32) {
        #[cfg(feature = "prometheus")]
        {
            let series = self.port.series();
            series.request[outcome.slot()].record(self.started.elapsed().as_secs_f64());
            series.attempts.record(f64::from(attempts));
        }
    }
}

/// `(metric, bucket edges)` for the exporter: a local ReadState read is tens
/// of microseconds, an RPC round trip milliseconds, a retry ladder seconds.
#[cfg(feature = "prometheus")]
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (
        REQUEST_SECONDS,
        &[
            50e-6, 100e-6, 250e-6, 500e-6, 1e-3, 2.5e-3, 5e-3, 10e-3, 25e-3, 100e-3, 1.0, 10.0,
            60.0,
        ],
    ),
    (ATTEMPTS, &[1.0, 2.0, 3.0, 5.0, 8.0, 13.0]),
];

/// `# HELP` registrations for every metric this crate emits.
#[cfg(feature = "prometheus")]
pub fn describe_metrics() {
    use metrics::{describe_histogram, Unit};

    describe_histogram!(
        REQUEST_SECONDS,
        Unit::Seconds,
        "One trip through a validator port, retries and backoff included, by port and outcome"
    );
    describe_histogram!(
        ATTEMPTS,
        "Attempts one trip through a validator port took, by port"
    );
}

#[cfg(all(test, feature = "prometheus"))]
mod tests {
    use std::time::Duration;

    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    use super::Port;
    use crate::error::{FailureMode, NonDomainError, QueryError, SourceError};
    use crate::validator_client::{RetryPolicy, ValidatorClient};

    /// What one trip recorded: `(outcome label, attempts)` from its two series.
    fn recorded(recorder: &DebuggingRecorder) -> Vec<(String, f64)> {
        let mut outcome = None;
        let mut attempts = None;
        for (key, _, _, value) in recorder.snapshotter().snapshot().into_vec() {
            let DebugValue::Histogram(samples) = value else {
                continue;
            };
            if samples.is_empty() {
                continue;
            }
            let key = key.key();
            if key.name() == super::REQUEST_SECONDS {
                outcome = key
                    .labels()
                    .find(|label| label.key() == "outcome")
                    .map(|label| label.value().to_owned());
            } else if key.name() == super::ATTEMPTS {
                attempts = samples.first().map(|sample| sample.into_inner());
            }
        }
        outcome.zip(attempts).into_iter().collect()
    }

    fn unreachable() -> QueryError<String, NonDomainError> {
        QueryError::NonDomain(NonDomainError::new(FailureMode::Connection, "refused"))
    }

    /// A trip records its outcome and attempt count once, at its end: a spent
    /// retry ladder is `unavailable` after every attempt, a considered rejection
    /// is `domain` after one.
    #[test]
    fn a_trip_records_its_outcome_and_attempts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a test runtime");
        let client = ValidatorClient::new(
            (),
            RetryPolicy {
                max_attempts: 3,
                initial_delay: Duration::ZERO,
                backoff_factor: 1.0,
                max_delay: Duration::ZERO,
            },
        );

        let spent = DebuggingRecorder::new();
        metrics::with_local_recorder(&spent, || {
            static PORT: Port = Port::new("test_spent");
            let result: Result<(), SourceError<String>> =
                runtime.block_on(client.with_retry(&PORT, || async { Err(unreachable()) }));
            assert!(matches!(result, Err(SourceError::Unavailable(_))));
        });
        assert_eq!(recorded(&spent), vec![("unavailable".to_owned(), 3.0)]);

        let rejected = DebuggingRecorder::new();
        metrics::with_local_recorder(&rejected, || {
            static PORT: Port = Port::new("test_rejected");
            let result: Result<(), SourceError<String>> =
                runtime.block_on(client.with_retry(&PORT, || async {
                    Err(QueryError::<String, NonDomainError>::Domain(
                        "no".to_owned(),
                    ))
                }));
            assert!(matches!(result, Err(SourceError::Domain(_))));
        });
        assert_eq!(recorded(&rejected), vec![("domain".to_owned(), 1.0)]);
    }
}
