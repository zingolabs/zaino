//! Shared route-test infra: `Routes` over a `MockValidator`, request builders, indexes folded
//! from a `MockChain`'s genesis and served through one fixed snapshot

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::Full;
use tokio::sync::watch;
use tonic::body::Body;
use zaino_chainview::{ChainView, ChainViewSnapshot, EndpointSet, ObservationFold};
use zaino_header_chain::testing::HeaderViews;
use zaino_nfs::{ChainParams, Indexed};
use zaino_persistence::{DiskEngine, DiskView, IndexKind, PersistenceEngine, Schema, Store};
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{BlockRef, ReorgDepth, TransactionId, UpgradeSchedule};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_snapshot::{Publisher, Snapshots};
use zaino_source::testing::{MockValidator, ValidatorViews};
use zaino_traffic::{Limits, TrafficBalancer, TrafficDriver, Trusted};

use crate::service::{Dispatch, Method, Routes};

/// Network the routes declare (the transparent tests' addresses are mainnet ones)
pub(super) const MAINNET: zcash_protocol::consensus::NetworkType =
    zcash_protocol::consensus::NetworkType::Main;

/// `kind` folded through genesis ..= `tip` by its own fold on the chain's network, committed
/// (compact-block's fees = the builder's: value-balance's fold derives the same, in its tests)
pub(super) fn indexed(kind: IndexKind, chain: &MockChain, tip: BlockRef) -> (IndexKind, DiskView) {
    let (format, tables) = match kind {
        IndexKind::CompactBlock => {
            (zaino_index_compact_block::FORMAT, zaino_index_compact_block::TABLES)
        }
        IndexKind::TreeState => (zaino_index_tree_state::FORMAT, zaino_index_tree_state::TABLES),
        IndexKind::TransparentAddress => {
            (zaino_index_transparent_address::FORMAT, zaino_index_transparent_address::TABLES)
        }
        IndexKind::BlockHash => (
            zaino_internal_block_hash_to_height::FORMAT,
            zaino_internal_block_hash_to_height::TABLES,
        ),
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
                let parent = zaino_index_compact_block::CompactBlockReader::new(parent);
                let fees = chain.fees(block.header().hash);
                zaino_index_compact_block::fold(&parent, &block, &fees, &mut out)
                    .expect("small tree sizes");
            }
            IndexKind::TreeState => {
                let parent = zaino_index_tree_state::TreeStateReader::new(parent);
                zaino_index_tree_state::fold(&parent, &block, &mut out)
                    .expect("canonical commitments");
            }
            IndexKind::TransparentAddress => {
                let parent = zaino_index_transparent_address::TransparentAddressReader::new(parent);
                zaino_index_transparent_address::fold(&parent, &block, &mut out);
            }
            IndexKind::BlockHash => {
                let parent = zaino_internal_block_hash_to_height::BlockHashReader::new(parent);
                zaino_internal_block_hash_to_height::fold(&parent, &block, &mut out);
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
    let params = ChainParams::of(chain);
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
    let view = ChainViewSnapshot::fixed(verified, held_by, &["node:8232"], ours);
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

/// Routes over `node`, every index configured (the balancer's driver + the view's poll fold back,
/// unspawned); nothing verified, nothing served, never republished
pub(super) fn routes_over(
    node: &Arc<MockValidator>,
) -> (Routes<MockValidator, DiskView>, TrafficDriver<MockValidator>, ObservationFold<MockValidator>)
{
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let trusted = Trusted { source: Arc::clone(node), priority: 0, limits };
    let (validators, driver) = TrafficBalancer::new(vec![trusted], None);
    let schedule = UpgradeSchedule::compiled(MAINNET).expect("mainnet compiled in");
    let address = vec!["node:8232".to_owned()];
    let view = ChainView::new(address, validators.clone(), ReorgDepth::CONSENSUS, schedule);
    let view = view.expect("one endpoint");
    let fold = view.observation_fold();
    let routes = Routes {
        snapshots: Snapshots::fixed(None, view.subscriber().current()),
        submit: Arc::new(view),
        validators,
        network: MAINNET,
        version: "0.11.0-test",
        max_address_rows: zaino_index_transparent_address::DEFAULT_MAX_ADDRESS_ROWS,
        indexes: vec![
            IndexKind::CompactBlock,
            IndexKind::TreeState,
            IndexKind::TransparentAddress,
            IndexKind::BlockHash,
        ],
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
    routes_over(&Arc::new(chain.validator(chain.genesis()))).0
}

pub(super) fn dispatch(
    routes: Routes<MockValidator, DiskView>,
) -> Dispatch<MockValidator, DiskView> {
    let dispatch = Dispatch::new(&crate::GrpcLimits::default());
    dispatch.routing().route(routes);
    dispatch
}

/// `method`'s gRPC request, body = one framed message
pub(super) fn framed_request(method: Method, message: Bytes) -> Request<Full<Bytes>> {
    request_at(&method.path(), message)
}

/// gRPC request at any `path`, body = one framed message
pub(super) fn request_at(path: &str, message: Bytes) -> Request<Full<Bytes>> {
    let mut framed = Vec::with_capacity(FRAME_HEADER + message.len());
    frame_into(&mut framed, |out| out.extend_from_slice(&message));

    Request::builder()
        .uri(format!("http://localhost{path}"))
        .body(Full::new(Bytes::from(framed)))
        .expect("request")
}

/// Data frames, then the trailers a streaming body ends in
pub(super) async fn drained(response: Response<Body>) -> (Vec<Bytes>, HeaderMap) {
    use http_body_util::BodyExt as _;

    let mut body = std::pin::pin!(response.into_body());
    let mut chunks = Vec::new();
    let mut trailers = None;
    while let Some(frame) = body.frame().await {
        let frame = frame.expect("frame");
        assert!(trailers.is_none(), "trailers are the last frame");
        match frame.into_data() {
            Ok(chunk) => chunks.push(chunk),
            Err(frame) => trailers = frame.into_trailers().ok(),
        }
    }

    (chunks, trailers.expect("a streaming body ends in trailers"))
}

/// Each framed `M` of `framed`, in order
pub(super) fn decode_frames<M: prost::Message + Default>(framed: &[u8]) -> Vec<M> {
    let messages = crate::wire::messages(framed).expect("whole frames");
    messages.into_iter().map(|message| M::decode(message).expect("decodes")).collect()
}
