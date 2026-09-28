//! Error types for the zainod daemon.

/// Errors from configuring, booting, or running the Zaino daemon.
///
/// Each variant keeps its cause typed (`#[from]`/`#[source]`). What the
/// runtime's assembly can fail with is its own [`DeployError`](zaino_runtime::DeployError).
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Configuration is missing, malformed, or invalid.
    #[error("configuration error: {0}")]
    ConfigError(String),
    /// Opening the Zebra ReadState database failed (Direct source mode).
    #[error("opening the validator ReadState database failed")]
    OpenReadState(#[source] zaino_source_zebra_readstate::OpenReadStateError),
    /// Building the validator JSON-RPC client failed (from the configured
    /// coordinates in Direct/Rpc source mode).
    #[error("building the validator JSON-RPC client failed")]
    RpcClient(#[source] zaino_rpc::RpcError),
    /// The validator's tip poller could not take its first reading, so no
    /// consumer could follow the chain.
    #[error("starting the validator tip poller failed")]
    TipPolling(#[source] zaino_source::QueryError<zaino_source::GetChainTipError>),
    /// The runtime could not assemble the selected deployment.
    #[error(transparent)]
    Deploy(#[from] zaino_runtime::DeployError),
    /// A background task panicked or was cancelled.
    #[error(transparent)]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Metrics endpoint error.
    #[cfg(feature = "prometheus")]
    #[error("metrics error: {0}")]
    MetricsError(String),
    /// A fatal runtime escalation — the caller's run loop restarts the daemon.
    #[error("restart zaino")]
    Restart,
}
