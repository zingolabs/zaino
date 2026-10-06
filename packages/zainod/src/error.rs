//! Error types for the zainod daemon.

/// Errors from configuring, booting, or running the Zaino daemon.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Configuration is missing, malformed, or invalid.
    #[error("configuration error: {0}")]
    ConfigError(String),
    /// The validator's JSON-RPC endpoint could not be resolved, authenticated, or reached.
    #[error(transparent)]
    ValidatorProbe(#[from] zaino_source::ProbeError),
    /// Opening an index directory failed.
    #[error(transparent)]
    OpenIndex(#[from] zaino_persistence::StoreError),
    /// The tree-state index's carries would not reseed off disk.
    #[error(transparent)]
    OpenTreeStateIndex(#[from] zaino_index_tree_state::IndexWriterError),
    #[error(transparent)]
    OpenValueBalanceIndex(#[from] zaino_internal_value_balance::IndexWriterError),
    /// The validator's upgrade schedule (tree-state pool activations) was unreadable at boot.
    #[error("validator upgrade schedule: {0}")]
    UpgradeSchedule(#[from] zaino_source::QueryError<zaino_source::GetBlockchainInfoError>),
    /// The configured validator set is empty or beyond the endpoint-set bound.
    #[error(transparent)]
    ChainView(#[from] zaino_chainview::ConfigError),
    /// A chainview endpoint was ejected (failure budget spent, or no mempool).
    #[error(transparent)]
    ChainViewEndpoint(#[from] zaino_chainview::EndpointPollError),
    #[error(transparent)]
    Produce(#[from] zaino_sync::ProduceError),
    /// Binding or running the gRPC server failed.
    #[error(transparent)]
    Grpc(#[from] zaino_grpc::GrpcServeError),
    /// The `[serve.tls]` certificate pair failed to load.
    #[error(transparent)]
    Tls(#[from] zaino_grpc::TlsError),
    /// A background task panicked or was cancelled.
    #[error(transparent)]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Metrics endpoint error.
    #[error("metrics error: {0}")]
    MetricsError(String),
    /// Bootstrapping empty indexes from the configured snapshot failed.
    #[cfg(feature = "snapshot")]
    #[error("index snapshot: {0}")]
    Snapshot(String),
    /// A runtime task ended cleanly before any shutdown signal (never expected: a fault)
    #[error("{task} task ended before shutdown")]
    TaskEnded { task: &'static str },
}
