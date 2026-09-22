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
    /// Opening the Zebra ReadState database failed (Direct source mode).
    #[error("opening the validator ReadState database failed")]
    OpenReadState(#[source] zaino_source_zebra_readstate::OpenReadStateError),
    /// Building the validator JSON-RPC client failed (from the configured
    /// coordinates in Direct/Rpc source mode).
    #[error("building the validator JSON-RPC client failed")]
    RpcClient(#[source] zaino_rpc::RpcError),
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
