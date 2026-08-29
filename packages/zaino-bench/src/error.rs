//! Error type for the benchmark harness.

use std::time::Duration;

/// Every way a benchmark run can fail.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BenchError {
    /// An argument combination the harness cannot act on.
    #[error("invalid arguments: {0}")]
    Args(String),

    /// The gRPC endpoint could not be reached or refused a call.
    #[error(transparent)]
    Grpc(#[from] crate::grpc_client::Error),

    /// The Prometheus scrape endpoint could not be reached.
    #[error("failed to scrape {url}: {source}")]
    Scrape {
        /// The endpoint that was scraped.
        url: String,
        /// The underlying transport failure.
        source: reqwest::Error,
    },

    /// zainod is serving `/metrics`, but without a metric the harness needs.
    /// Most often this means zainod was built without `--features prometheus`,
    /// or has not yet emitted its first sync sample.
    #[error(
        "metric `{0}` is absent from the scrape — is zainod built with \
         `--features prometheus`, and has its sync loop started?"
    )]
    MissingMetric(&'static str),

    /// `finalized_height` did not advance within the stall timeout.
    #[error("sync stalled: finalized height held at {height} for {}s", .timeout.as_secs())]
    SyncStalled {
        /// The height sync was stuck at.
        height: u64,
        /// How long it was stuck for.
        timeout: Duration,
    },

    /// Not one connection reached the server, in any round.
    #[error("no connection to {0} succeeded in any round")]
    AllConnectionsFailed(String),

    /// The chain served over gRPC does not link up.
    #[error("chain is invalid: {0} error(s) found")]
    InvalidChain(usize),

    /// Writing the CSV sample log failed.
    #[error("failed to write CSV to {path}: {source}")]
    Csv {
        /// The path that was being written.
        path: String,
        /// The underlying IO failure.
        source: std::io::Error,
    },
}
