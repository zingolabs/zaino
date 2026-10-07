//! Shared route-test infra: `Routes` over a `MockChain` validator, request builders, an index
//! feeder

use std::{future::Future, sync::Arc};

use http::Request;
use http_body_util::Full;
use zaino_persistence::{DiskEngine, DiskStore, DiskView, PersistenceEngine, Schema};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_source::mock::MockChain;
use zaino_source::TrafficBalancer;
use zaino_sync::Served;

use crate::limits::ReadLanes;
use crate::service::{Dispatch, Routes};

/// Network the transparent-address tests build their index on (addresses are mainnet ones)
pub(super) const MAINNET: zcash_protocol::consensus::NetworkType =
    zcash_protocol::consensus::NetworkType::Main;

/// `path` on a fresh in-memory filesystem, as `schema`'s store
pub(super) fn store(path: &str, schema: &Schema) -> DiskStore {
    let engine = DiskEngine::new(zaino_persistence::fs::SimFs::new());
    engine.open(std::path::Path::new(path), schema).expect("open")
}

/// An empty compact-block index (no tip yet: its methods say retry)
fn empty_compact_block() -> zaino_index_compact_block::CompactBlockService<DiskView> {
    let store = store("/cb", &zaino_index_compact_block::schema(MAINNET));
    let view = zaino_index_compact_block::testing::committed(store, 0);
    zaino_index_compact_block::CompactBlockService::new(Served::fixed(view))
}

/// The always-on routes over `node` (its view's pollers back, unspawned); every index off
/// but an empty compact-block one
pub(super) fn routes_over(
    node: &Arc<MockChain>,
) -> (Routes<MockChain, DiskView>, Vec<zaino_chainview::EndpointPoller<MockChain>>) {
    let endpoint =
        zaino_chainview::Endpoint { address: "node:8232".to_owned(), source: Arc::clone(node) };
    let depth = zaino_primitives::types::ReorgDepth::CONSENSUS;
    let (view, pollers) =
        zaino_chainview::ChainView::new(vec![endpoint], depth).expect("one endpoint");
    let routes = Routes {
        chain: Arc::new(view),
        validators: TrafficBalancer::new(vec![Arc::clone(node)]),
        network: MAINNET,
        compact_block: empty_compact_block(),
        block_hash: None,
        tree_state: None,
        transparent_address: None,
    };
    (routes, pollers)
}

/// [`routes_over`] an empty, never-polled node, for tests that only enable indexes
pub(super) fn routes() -> Routes<MockChain, DiskView> {
    routes_over(&Arc::new(MockChain::new())).0
}

pub(super) fn dispatch(routes: Routes<MockChain, DiskView>) -> Dispatch<MockChain, DiskView> {
    Dispatch::new(routes, ReadLanes::new(&crate::GrpcLimits::default()))
}

pub(super) fn request(path: &str) -> Request<Full<bytes::Bytes>> {
    framed_request(path, bytes::Bytes::new())
}

/// A gRPC request whose body is one framed message.
pub(super) fn framed_request(path: &str, message: bytes::Bytes) -> Request<Full<bytes::Bytes>> {
    let mut framed = Vec::with_capacity(FRAME_HEADER + message.len());
    frame_into(&mut framed, |out| out.extend_from_slice(&message));

    Request::builder()
        .uri(format!("http://localhost{path}"))
        .body(Full::new(bytes::Bytes::from(framed)))
        .expect("request")
}

/// An index loop (`follow` = its `run` over the queue given) fed `blocks` as bulk, each a
/// final `Apply` then `Shutdown` as the producer sends them, awaited through `Shutdown`
pub(super) async fn indexed<F>(
    blocks: &[std::sync::Arc<zaino_primitives::types::Block>],
    name: &'static str,
    follow: impl FnOnce(zaino_sync::Subscription<zaino_primitives::types::Block>) -> F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let mut sink = zaino_sync::BlockSink::new("blocks");
    let queue = std::num::NonZeroUsize::new(1 << 20).expect("non-zero");
    let running = tokio::spawn(follow(sink.subscribe(name, queue)));
    for block in blocks {
        let (height, data) = (block.header().height, std::sync::Arc::clone(block));
        sink.send(zaino_sync::Step::Apply { height, finalized: true, data }).await;
    }
    sink.shutdown();
    running.await.expect("indexed through Shutdown");
}
