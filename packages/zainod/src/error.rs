//! Error types for the zainod daemon.

use zaino_sync::IndexFailed;

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
    /// The compact-block index could not resume — its stored tree sizes will not decode.
    #[error(transparent)]
    ResumeIndex(#[from] zaino_index_compact_block::IndexWriterError),
    /// The tree-state index's carries would not reseed off disk.
    #[error(transparent)]
    OpenTreeStateIndex(#[from] zaino_index_tree_state::IndexWriterError),
    /// An index with no errors of its own (block-hash, transparent-address) stopped.
    #[error(transparent)]
    Index(#[from] IndexFailed<zaino_persistence::StoreError>),
    #[error(transparent)]
    CompactBlockIndex(#[from] IndexFailed<zaino_index_compact_block::IndexWriterError>),
    #[error(transparent)]
    TreeStateIndex(#[from] IndexFailed<zaino_index_tree_state::IndexWriterError>),
    #[error(transparent)]
    OpenValueBalanceIndex(#[from] zaino_internal_value_balance::IndexWriterError),
    #[error(transparent)]
    ValueBalanceIndex(#[from] IndexFailed<zaino_internal_value_balance::IndexWriterError>),
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
    /// A background task panicked or was cancelled.
    #[error(transparent)]
    TokioJoinError(#[from] tokio::task::JoinError),
    /// Metrics endpoint error.
    #[error("metrics error: {0}")]
    MetricsError(String),
    /// A runtime task ended cleanly before any shutdown signal (never expected: a fault)
    #[error("{task} task ended before shutdown")]
    TaskEnded { task: &'static str },
}
