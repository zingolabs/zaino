//! gRPC / JsonRPC service implementations.

use tokio::sync::watch;
use zaino_proto::proto::indexed_tip::indexed_tip_service_server::IndexedTipServiceServer;
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zaino_state::{IndexedTipIndexer, IndexerSubscriber, LightWalletIndexer, ZcashIndexer};

pub mod grpc;
mod indexed_tip;
pub mod jsonrpc;

use indexed_tip::IndexedTipService;

#[derive(Clone)]
/// Zaino gRPC service.
pub struct GrpcClient<Indexer: ZcashIndexer + LightWalletIndexer> {
    /// Chain fetch service subscriber.
    pub service_subscriber: IndexerSubscriber<Indexer>,
}

#[derive(Clone)]
/// Zaino JSONRPC service.
pub struct JsonRpcClient<Indexer: ZcashIndexer + LightWalletIndexer> {
    /// Chain fetch service subscriber.
    pub service_subscriber: IndexerSubscriber<Indexer>,
}

/// Wraps an [`IndexerSubscriber`] in the generated `CompactTxStreamer`
/// gRPC service and produces type-erased [`tonic::service::Routes`].
///
/// Lives here (next to [`GrpcClient`]) so callers don't need a direct
/// dependency on `zaino-proto` to wire the gRPC dispatcher. The
/// transport-layer entrypoint
/// [`crate::server::grpc::TonicServer::spawn`] accepts the returned
/// [`tonic::service::Routes`] directly.
pub fn grpc_routes<Indexer: ZcashIndexer + LightWalletIndexer + IndexedTipIndexer + Clone>(
    service_subscriber: IndexerSubscriber<Indexer>,
    shutdown: watch::Receiver<()>,
) -> tonic::service::Routes {
    let indexed_tip_service = IndexedTipService::new(service_subscriber.inner_clone(), shutdown);
    tonic::service::Routes::new(CompactTxStreamerServer::new(GrpcClient {
        service_subscriber,
    }))
    .add_service(IndexedTipServiceServer::new(indexed_tip_service))
}
