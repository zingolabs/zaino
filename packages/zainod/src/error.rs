//! zainod daemon errors

use std::num::NonZeroUsize;
use std::path::PathBuf;

/// Configuring, booting or running the daemon (`TaskEnded` = a task ended cleanly before any
/// shutdown signal: never expected, a fault)
#[derive(Debug, thiserror::Error)]
pub(crate) enum IndexerError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("writing {}: {source}", path.display())]
    ConfigFile { path: PathBuf, source: std::io::Error },
    #[error("raising the open-file limit: {0}")]
    RaiseFdLimit(std::io::Error),
    #[error(
        "grpc.max_connections = {max_connections} needs {needed} open files ({reserved} kept for \
         indexes and the validator) and the limit is {limit}: raise the hard limit (systemd \
         LimitNOFILE=, docker --ulimit nofile=) or lower grpc.max_connections",
        reserved = crate::fd_limit::RESERVED
    )]
    FdLimit { max_connections: NonZeroUsize, needed: u64, limit: u64 },
    #[error(transparent)]
    ValidatorEndpoint(#[from] zaino_source::EndpointError),
    #[error(transparent)]
    OpenIndex(#[from] zaino_persistence::StoreError),
    #[error(transparent)]
    Nfs(#[from] zaino_nfs::NfsError),
    #[error(transparent)]
    Follow(#[from] zaino_sync::FollowError),
    #[error(transparent)]
    GrpcSnapshot(#[from] zaino_grpc_snapshot::GrpcSnapshotError),
    #[error(transparent)]
    Grpc(#[from] zaino_grpc::GrpcServeError),
    #[error(transparent)]
    Tls(#[from] zaino_grpc::TlsError),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
    #[error("metrics error: {0}")]
    Metrics(String),
    #[error("index snapshot: {0}")]
    Bootstrap(String),
    #[error("{task} task ended before shutdown")]
    TaskEnded { task: &'static str },
}
