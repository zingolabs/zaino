//! gRPC / JsonRPC service implementations.

use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zaino_state::{IndexerSubscriber, LightWalletIndexer, ZcashIndexer};

pub mod grpc;
pub mod jsonrpc;

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
pub fn grpc_routes<Indexer: ZcashIndexer + LightWalletIndexer>(
    service_subscriber: IndexerSubscriber<Indexer>,
) -> tonic::service::Routes {
    tonic::service::Routes::new(CompactTxStreamerServer::new(GrpcClient {
        service_subscriber,
    }))
}
