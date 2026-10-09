//! Shared route-test infra: `Routes` over a `MockValidator`, request builders, indexes folded
//! from a `MockChain`'s genesis and served through one fixed snapshot

use std::{num::NonZeroUsize, sync::Arc};

use http::Request;
use http_body_util::Full;
use tokio::sync::watch;
use zaino_chainview::{ChainView, ChainViewSnapshot, EndpointSet, ObservationFold};
use zaino_header_chain::testing::HeaderViews;
use zaino_nfs::{ChainParams, Indexed};
use zaino_persistence::{DiskEngine, DiskView, IndexKind, PersistenceEngine, Schema, Store};
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{BlockRef, ReorgDepth, TransactionId};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_snapshot::{Publisher, Snapshots};
use zaino_source::testing::MockValidator;
use zaino_traffic::{Limits, TrafficBalancer, TrafficDriver, Trusted};

use crate::service::{Dispatch, Routes};

/// Network the routes declare (the transparent tests' addresses are mainnet ones)
pub(super) const MAINNET: zcash_protocol::consensus::NetworkType =
    zcash_protocol::consensus::NetworkType::Main;

/// `kind` folded through genesis ..= `tip` by its own fold on the chain's network, committed
/// (compact-block's fees = the builder's: value-balance's fold derives the same, in its tests)
pub(super) fn indexed(kind: IndexKind, chain: &MockChain, tip: BlockRef) -> (IndexKind, DiskView) {
    use zaino_index_compact_block as compact_block;
    use zaino_index_transparent_address as transparent_address;
    use zaino_index_tree_state as tree_state;
    use zaino_internal_block_hash_to_height as block_hash;

    let (format, tables) = match kind {
        IndexKind::CompactBlock => (compact_block::FORMAT, compact_block::TABLES),
        IndexKind::TreeState => (tree_state::FORMAT, tree_state::TABLES),
        IndexKind::TransparentAddress => (transparent_address::FORMAT, transparent_address::TABLES),
        IndexKind::BlockHash => (block_hash::FORMAT, block_hash::TABLES),
        IndexKind::ValueBalance => panic!("not a served index"),
    };
    let schema = Schema::new(kind, format, chain.schedule().network, tables);
    let engine = DiskEngine::new(
        zaino_persistence::fs::SimFs::new(),
        zaino_persistence::LsmConfig::default(),
    );
    let mut index =
        engine.open(std::path::Path::new(kind.name()), &schema, NonZeroUsize::MAX).expect("open");
    for block in chain.blocks(tip) {
        let parent = index.staged();
        let mut out = index.changes(block.at());
        match kind {
            IndexKind::CompactBlock => {
                let parent = compact_block::CompactBlockReader::new(parent);
                let fees = chain.fees(block.header().hash);
                compact_block::fold(&parent, &block, &fees, &mut out).expect("small tree sizes");
            }
            IndexKind::TreeState => {
                let parent = tree_state::TreeStateReader::new(parent);
                tree_state::fold(&parent, &block, &mut out).expect("canonical commitments");
            }
            IndexKind::TransparentAddress => {
                let parent = transparent_address::TransparentAddressReader::new(parent);
                transparent_address::fold(&parent, &block, &mut out);
            }
            IndexKind::BlockHash => {
                block_hash::fold(&block_hash::BlockHashReader::new(parent), &block, &mut out);
            }
            IndexKind::ValueBalance => unreachable!("refused above"),
        }
        index.apply(out);
    }
    index.commit().expect("SimFs commit");
    (kind, index.committed())
}

/// `views` served at `tip` (the NFS root): genesis ..= `tip` verified, the chain's own params
pub(super) fn indexed_at(
    chain: &MockChain,
    tip: BlockRef,
    views: Vec<(IndexKind, DiskView)>,
) -> Indexed<DiskView> {
    let params = ChainParams::of(chain, tip);
    Indexed::fixed(Arc::new(chain.verified(tip)), tip, params, views)
}

/// One snapshot for good: [`indexed_at`], `held_by` holding `tip`, `ours` our relays (servable)
pub(super) fn snapshot_held(
    chain: &MockChain,
    tip: BlockRef,
    views: Vec<(IndexKind, DiskView)>,
    held_by: EndpointSet,
    ours: &[(TransactionId, bytes::Bytes)],
) -> Snapshots<DiskView> {
    let verified = Some(Arc::new(chain.verified(tip)));
    let view = ChainViewSnapshot::fixed(verified, held_by, &["node:8232"], ours, &[]);
    Snapshots::fixed(Some(Arc::new(indexed_at(chain, tip, views))), Arc::new(view))
}

/// [`snapshot_held`] by no validator, empty mempool (index methods only)
pub(super) fn snapshot(
    chain: &MockChain,
    tip: BlockRef,
    views: Vec<(IndexKind, DiskView)>,
) -> Snapshots<DiskView> {
    snapshot_held(chain, tip, views, EndpointSet::default(), &[])
}

/// The always-on routes over `node` (the balancer's driver + the view's poll fold back,
/// unspawned); nothing verified, nothing served, never republished
pub(super) fn routes_over(
    node: &Arc<MockValidator>,
) -> (Routes<MockValidator, DiskView>, TrafficDriver<MockValidator>, ObservationFold<MockValidator>)
{
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = Trusted { source: Arc::clone(node), priority: 0, limits };
    let (validators, driver) = TrafficBalancer::new(vec![trusted], None);
    let view =
        ChainView::new(vec!["node:8232".to_owned()], validators.clone(), ReorgDepth::CONSENSUS);
    let view = view.expect("one endpoint");
    let fold = view.observation_fold();
    let routes = Routes {
        snapshots: Snapshots::fixed(None, view.subscriber().current()),
        submit: Arc::new(view),
        validators,
        network: MAINNET,
        max_address_rows: zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS,
    };
    (routes, driver, fold)
}

/// Stands in for the NFS's publication watch
pub(super) type NfsPublishes = watch::Sender<Option<Arc<Indexed<DiskView>>>>;

/// `routes` read a live publisher over their chain view and the returned NFS watch (the
/// publisher back, unspawned)
pub(super) fn published(
    routes: &mut Routes<MockValidator, DiskView>,
) -> (Publisher<DiskView>, NfsPublishes) {
    let (nfs, indexed) = watch::channel(None);
    let publisher = Publisher::new(indexed, routes.submit.subscriber(), ReorgDepth::CONSENSUS);
    routes.snapshots = publisher.handle();
    (publisher, nfs)
}

/// [`routes_over`] a never-polled node at a bare genesis, for tests that only serve indexes
pub(super) fn routes() -> Routes<MockValidator, DiskView> {
    let chain = MockChain::regtest();
    routes_over(&Arc::new(MockValidator::following(&chain, chain.genesis()))).0
}

pub(super) fn dispatch(
    routes: Routes<MockValidator, DiskView>,
) -> Dispatch<MockValidator, DiskView> {
    Dispatch::new(routes, &crate::GrpcLimits::default())
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
