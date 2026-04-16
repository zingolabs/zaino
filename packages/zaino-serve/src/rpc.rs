//! gRPC / JsonRPC service implementations.

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
