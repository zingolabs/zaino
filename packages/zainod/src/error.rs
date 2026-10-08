//! zainod daemon errors

/// Configuring, booting or running the daemon (`TaskEnded` = a task ended cleanly before any
/// shutdown signal: never expected, a fault)
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    #[error("configuration error: {0}")]
    ConfigError(String),
    #[error(transparent)]
    ValidatorEndpoint(#[from] zaino_source::EndpointError),
    #[error(transparent)]
    OpenIndex(#[from] zaino_persistence::StoreError),
    #[error(transparent)]
    ChainView(#[from] zaino_chainview::ConfigError),
    #[error(transparent)]
    Nfs(#[from] zaino_nfs::NfsError),
    #[error(transparent)]
    Follow(#[from] zaino_sync::FollowError),
    #[error(transparent)]
    Snapshot(#[from] zaino_snapshot::SnapshotError),
    #[error(transparent)]
    Grpc(#[from] zaino_grpc::GrpcServeError),
    #[error(transparent)]
    Tls(#[from] zaino_grpc::TlsError),
    #[error(transparent)]
    TokioJoinError(#[from] tokio::task::JoinError),
    #[error("metrics error: {0}")]
    MetricsError(String),
    #[cfg(feature = "snapshot")]
    #[error("index snapshot: {0}")]
    Bootstrap(String),
    #[error("{task} task ended before shutdown")]
    TaskEnded { task: &'static str },
}
