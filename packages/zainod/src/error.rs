//! Error types for the zainod daemon.

/// Errors from configuring, booting, or running the Zaino daemon.
///
/// Each variant keeps its cause typed (`#[from]`/`#[source]`); only the
/// component-boot boundary is boxed, since `OrchestraBuilder::boot` is generic
/// over each component's error type and one enum cannot name them all.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Configuration is missing, malformed, or invalid.
    #[error("configuration error: {0}")]
    ConfigError(String),
    /// The validator's JSON-RPC endpoint could not be resolved, authenticated, or reached.
    #[error(transparent)]
    ValidatorProbe(#[from] zaino_rpc::ProbeError),
    /// Seeding the validator tip subscription failed (first tip read).
    #[error("reading the validator tip to seed the tip subscription failed")]
    TipPolling(#[source] zaino_source::QueryError<zaino_source::GetChainTipError>),
    /// The non-finalised chain-head could not anchor against the validator.
    #[error("the chain-head could not anchor against the validator")]
    ChainHeadInit(#[source] zaino_chain_head_service::ChainHeadInitError),
    /// Opening the LMDB index store failed.
    #[error(transparent)]
    OpenStore(#[from] zaino_persistence::OpenError),
    /// Building or running the sync stack (backend, provisioner, engine) failed.
    #[error(transparent)]
    Sync(#[from] zaino_indexer::IndexerError),
    /// The validator was unreachable when the runtime gated on it at boot.
    #[error(transparent)]
    ValidatorUnreachable(#[from] zaino_runtime::ValidatorUnreachable),
    /// A runtime component failed to boot.
    #[error("component failed to boot")]
    Boot(#[source] Box<dyn std::error::Error + Send + Sync>),
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
