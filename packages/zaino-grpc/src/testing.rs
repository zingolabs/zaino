//! Shared route-test infra: `Routes` over a `MockChain` validator, request builders, indexes
//! folded from genesis and served through one fixed snapshot

use std::sync::Arc;

use http::Request;
use http_body_util::Full;
use zaino_header_chain::VerifiedChain;
use zaino_index_tree_state::PoolActivations;
use zaino_nfs::{ChainParams, NfsHandle};
use zaino_persistence::{
    DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, Schema, Store,
};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_source::mock::MockChain;
use zaino_source::TrafficBalancer;

use crate::limits::ReadLanes;
use crate::service::{Dispatch, Routes};

/// Network every test index is built on (the transparent tests' addresses are mainnet ones)
pub(super) const MAINNET: zcash_protocol::consensus::NetworkType =
    zcash_protocol::consensus::NetworkType::Main;

/// `path` on a fresh in-memory filesystem, as `schema`'s store
pub(super) fn store(path: &str, schema: &Schema) -> DiskStore {
    let engine = DiskEngine::new(zaino_persistence::fs::SimFs::new());
    engine.open(std::path::Path::new(path), schema).expect("open")
}

/// `kind` folded through `blocks` from genesis by its own fold, committed (compact-block's fees
/// from value-balance's fold, as the NFS folds them)
pub(super) fn indexed(kind: IndexKind, blocks: &[Block]) -> (IndexKind, DiskView) {
    use zaino_index_compact_block as compact_block;
    use zaino_index_transparent_address as transparent_address;
    use zaino_index_tree_state as tree_state;
    use zaino_internal_block_hash_to_height as block_hash;
    use zaino_internal_value_balance as value_balance;

    let schema = match kind {
        IndexKind::CompactBlock => compact_block::schema(MAINNET),
        IndexKind::TreeState => tree_state::schema(MAINNET),
        IndexKind::TransparentAddress => transparent_address::schema(MAINNET),
        IndexKind::BlockHash => block_hash::schema(MAINNET),
        IndexKind::ValueBalance | IndexKind::HeaderChain => panic!("not a served index"),
    };
    let mut index = store(kind.name(), &schema);
    let mut fees = store("fees", &value_balance::schema(MAINNET));
    for block in blocks {
        let parent = index.staged();
        let changes = match kind {
            IndexKind::CompactBlock => {
                let paid = value_balance::ValueBalanceReader::new(fees.staged(), MAINNET);
                let (outputs, paid) = value_balance::fold(&paid, block).expect("prevouts held");
                fees.apply(outputs);
                let parent = compact_block::CompactBlockReader::new(parent, MAINNET);
                compact_block::fold(&parent, block, &paid).expect("small tree sizes")
            }
            IndexKind::TreeState => {
                let parent = tree_state::TreeStateReader::new(parent, MAINNET);
                tree_state::fold(&parent, block).expect("canonical commitments")
            }
            IndexKind::TransparentAddress => transparent_address::fold(
                &transparent_address::TransparentAddressReader::new(parent, MAINNET),
                block,
            ),
            _ => block_hash::fold(block, MAINNET),
        };
        index.apply(changes);
    }
    index.commit().expect("SimFs commit");
    (kind, index.view())
}

/// One snapshot for good: `views` served at `path`'s last block, every pool active from genesis
pub(super) fn snapshot(path: &[Block], views: Vec<(IndexKind, DiskView)>) -> NfsHandle<DiskView> {
    let tip = path.last().expect("a path holds genesis").header();
    let tip = BlockRef { hash: tip.hash, height: tip.height };
    let genesis = Height::GENESIS;
    let activations =
        PoolActivations { sapling: genesis, orchard: Some(genesis), ironwood: Some(genesis) };
    let params = ChainParams { network: MAINNET, activations };
    NfsHandle::fixed(Arc::new(VerifiedChain::regtest(path)), tip, params, views)
}

/// The always-on routes over `node` (its view's pollers back, unspawned); nothing served yet
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
        nfs: NfsHandle::unpublished(),
        max_address_rows: zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS,
    };
    (routes, pollers)
}

/// [`routes_over`] an empty, never-polled node, for tests that only serve indexes
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
