//! Shared route-test infra: `Routes` over a `MockChain` validator, request builders, indexes
//! folded from genesis and served through one fixed snapshot

use std::sync::Arc;

use http::Request;
use http_body_util::Full;
use zaino_chainview::{ChainView, ObservationFold};
use zaino_header_chain::VerifiedChain;
use zaino_index_tree_state::PoolActivations;
use zaino_nfs::{ChainParams, NfsHandle};
use zaino_persistence::{
    DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, Schema, Store,
};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_source::mock::MockChain;
use zaino_traffic::{Limits, TrafficBalancer, TrafficDriver, Trusted};

use crate::limits::ReadLanes;
use crate::service::{Dispatch, Routes};

/// Network every test index is built on (the transparent tests' addresses are mainnet ones)
pub(super) const MAINNET: zcash_protocol::consensus::NetworkType =
    zcash_protocol::consensus::NetworkType::Main;

/// Compact-block's store on [`MAINNET`]
pub(super) const COMPACT_BLOCK: Schema = Schema::new(
    IndexKind::CompactBlock,
    zaino_index_compact_block::FORMAT,
    MAINNET,
    zaino_index_compact_block::TABLES,
);

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

    let (format, tables) = match kind {
        IndexKind::CompactBlock => (compact_block::FORMAT, compact_block::TABLES),
        IndexKind::TreeState => (tree_state::FORMAT, tree_state::TABLES),
        IndexKind::TransparentAddress => (transparent_address::FORMAT, transparent_address::TABLES),
        IndexKind::BlockHash => (block_hash::FORMAT, block_hash::TABLES),
        IndexKind::ValueBalance | IndexKind::HeaderChain => panic!("not a served index"),
    };
    let mut index = store(kind.name(), &Schema::new(kind, format, MAINNET, tables));
    let fees =
        Schema::new(IndexKind::ValueBalance, value_balance::FORMAT, MAINNET, value_balance::TABLES);
    let mut fees = store("fees", &fees);
    for block in blocks {
        let parent = index.staged();
        let mut out = index.changes(block.at());
        match kind {
            IndexKind::CompactBlock => {
                let mut outputs = fees.changes(block.at());
                let paid = value_balance::ValueBalanceReader::new(fees.staged());
                let paid = value_balance::fold(&paid, block, &mut outputs).expect("prevouts held");
                fees.apply(outputs);
                let parent = compact_block::CompactBlockReader::new(parent);
                compact_block::fold(&parent, block, &paid, &mut out).expect("small tree sizes");
            }
            IndexKind::TreeState => {
                let parent = tree_state::TreeStateReader::new(parent);
                tree_state::fold(&parent, block, &mut out).expect("canonical commitments");
            }
            IndexKind::TransparentAddress => {
                let parent = transparent_address::TransparentAddressReader::new(parent);
                transparent_address::fold(&parent, block, &mut out);
            }
            IndexKind::BlockHash => {
                block_hash::fold(&block_hash::BlockHashReader::new(parent), block, &mut out);
            }
            IndexKind::ValueBalance | IndexKind::HeaderChain => unreachable!("refused above"),
        }
        index.apply(out);
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

/// The always-on routes over `node` (the balancer's driver + the view's poll fold back,
/// unspawned); nothing served yet
pub(super) fn routes_over(
    node: &Arc<MockChain>,
) -> (Routes<MockChain, DiskView>, TrafficDriver<MockChain>, ObservationFold<MockChain>) {
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = Trusted { source: Arc::clone(node), priority: 0, limits };
    let (validators, driver) = TrafficBalancer::new(vec![trusted], None);
    let depth = zaino_primitives::types::ReorgDepth::CONSENSUS;
    let view = ChainView::new(vec!["node:8232".to_owned()], validators.clone(), depth);
    let view = view.expect("one endpoint");
    let fold = view.observation_fold();
    let routes = Routes {
        chain: Arc::new(view),
        validators,
        network: MAINNET,
        nfs: NfsHandle::unpublished(),
        max_address_rows: zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS,
    };
    (routes, driver, fold)
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

/// gRPC request, body = one framed message
pub(super) fn framed_request(path: &str, message: bytes::Bytes) -> Request<Full<bytes::Bytes>> {
    let mut framed = Vec::with_capacity(FRAME_HEADER + message.len());
    frame_into(&mut framed, |out| out.extend_from_slice(&message));

    Request::builder()
        .uri(format!("http://localhost{path}"))
        .body(Full::new(bytes::Bytes::from(framed)))
        .expect("request")
}
