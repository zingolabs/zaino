//! Shared route-test infra: `RouteDeps` over a `MockValidator`, request builders, indexes folded
//! from a `MockChain`'s genesis and served through one fixed `GrpcSnapshot`

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use http::{HeaderMap, Request, Response};
use http_body_util::Full;
use tokio::sync::watch;
use tonic::body::Body;
use zaino_chainview::{ChainView, ChainViewSnapshot, ObservationMerge, TrustedValidatorSet};
use zaino_grpc_snapshot::{GrpcSnapshots, Publisher};
use zaino_header_chain::testing::HeaderViews;
use zaino_nfs::{ChainParams, NfsPublication};
use zaino_persistence::{DiskEngine, DiskView, IndexKind, PersistenceEngine, Store};
use zaino_primitives::protocol::compiled_schedule;
use zaino_primitives::testing::MockChain;
use zaino_primitives::types::{BlockRef, FinalityDepth};
use zaino_proto::frame::{frame_into, FRAME_HEADER};
use zaino_source::testing::{MockValidator, ValidatorViews};
use zaino_traffic::{Limits, TrafficBalancer, TrafficDriver, TrustedValidator};
use zcash_protocol::TxId;

use crate::service::{Dispatch, Method, RouteDeps};

/// Network the routes declare (the transparent tests' addresses are mainnet ones)
pub(super) const MAINNET: zcash_protocol::consensus::NetworkType =
    zcash_protocol::consensus::NetworkType::Main;

/// `kind` folded through genesis ..= `tip` by its own fold on the chain's network, committed
/// (compact-block's fees = the builder's: utxo-set's fold derives the same, in its tests)
pub(super) fn indexed(kind: IndexKind, chain: &MockChain, tip: BlockRef) -> (IndexKind, DiskView) {
    assert_ne!(kind, IndexKind::UtxoSet, "not a served index");
    let schema = zaino_nfs::schema(kind, chain.schedule().network);
    let engine = DiskEngine::new(
        zaino_persistence::fs::SimFs::new(),
        zaino_persistence::LsmConfig::default(),
    );
    let mut index =
        engine.open(std::path::Path::new(kind.name()), &schema, NonZeroUsize::MAX).expect("open");
    for block in chain.blocks(tip) {
        let parent = index.applied();
        let mut out = index.changes(block.block_ref());
        match kind {
            IndexKind::CompactBlock => {
                let parent = zaino_index_compact_block::CompactBlockReader::new(parent);
                let fees = chain.fees(block.header().hash);
                zaino_index_compact_block::fold(&parent, &block, &fees, &mut out);
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
            IndexKind::TxidLocation => {
                let parent = zaino_internal_txid_location::TxidLocationReader::new(parent);
                zaino_internal_txid_location::fold(&parent, &block, &mut out);
            }
            IndexKind::UtxoSet => unreachable!("asserted above"),
        }
        index.apply(out);
    }
    index.commit().expect("SimFs commit");
    (kind, index.durable())
}

/// `views` served at `tip` (the NFS root): genesis ..= `tip` verified, the chain's own params
pub(super) fn indexed_at(
    chain: &MockChain,
    tip: BlockRef,
    views: Vec<(IndexKind, DiskView)>,
) -> NfsPublication<DiskView> {
    let params = ChainParams::of(chain);
    NfsPublication::fixed(Arc::new(chain.snapshot(tip)), tip, params, views)
}

/// Fixed `GrpcSnapshot`: [`indexed_at`], `held_by` holding `tip`, `ours` our relays (servable)
pub(super) fn grpc_snapshots_held(
    chain: &MockChain,
    tip: BlockRef,
    views: Vec<(IndexKind, DiskView)>,
    held_by: TrustedValidatorSet,
    ours: &[(TxId, bytes::Bytes)],
) -> GrpcSnapshots<DiskView> {
    let verified = Some(Arc::new(chain.snapshot(tip)));
    let view = ChainViewSnapshot::fixed(verified, held_by, &["node:8232"], ours);
    GrpcSnapshots::fixed(Some(Arc::new(indexed_at(chain, tip, views))), Arc::new(view))
}

/// [`grpc_snapshots_held`] by no validator, empty mempool (index methods only)
pub(super) fn grpc_snapshots(
    chain: &MockChain,
    tip: BlockRef,
    views: Vec<(IndexKind, DiskView)>,
) -> GrpcSnapshots<DiskView> {
    grpc_snapshots_held(chain, tip, views, TrustedValidatorSet::default(), &[])
}

/// `RouteDeps` over `node`, every index configured (the balancer's driver + the view's poll merge
/// back, unspawned)
///
/// - nothing verified, nothing served, never republished
pub(super) fn routes_over(
    node: &Arc<MockValidator>,
) -> (
    RouteDeps<MockValidator, DiskView>,
    TrafficDriver<MockValidator>,
    ObservationMerge<MockValidator>,
) {
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let address = "node:8232".to_owned();
    let trusted = TrustedValidator { address, source: Arc::clone(node), priority: 0, limits };
    let (validators, driver) = TrafficBalancer::new(vec![trusted], None);
    let schedule = compiled_schedule(MAINNET).expect("mainnet compiled in");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let view = ChainView::new(validators.clone(), FinalityDepth::CONSENSUS, schedule, shutdown);
    let merge = view.observation_merge();
    let routes = RouteDeps {
        grpc_snapshots: GrpcSnapshots::fixed(None, view.subscriber().current()),
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
    (routes, driver, merge)
}

/// Stands in for the NFS's publication watch
pub(super) type NfsPublishes = watch::Sender<Option<Arc<NfsPublication<DiskView>>>>;

/// `routes` read a live publisher over their chain view and the returned NFS watch (the
/// publisher back, unspawned)
pub(super) fn published(
    routes: &mut RouteDeps<MockValidator, DiskView>,
) -> (Publisher<DiskView>, NfsPublishes) {
    let (nfs, indexed) = watch::channel(None);
    let publisher = Publisher::new(indexed, routes.submit.subscriber(), FinalityDepth::CONSENSUS);
    routes.grpc_snapshots = publisher.handle();
    (publisher, nfs)
}

/// [`routes_over`] a never-polled node at a bare genesis, for tests that only serve indexes
pub(super) fn routes() -> RouteDeps<MockValidator, DiskView> {
    let chain = MockChain::regtest();
    routes_over(&Arc::new(chain.validator(chain.genesis()))).0
}

pub(super) fn dispatch(
    routes: RouteDeps<MockValidator, DiskView>,
) -> Dispatch<MockValidator, DiskView> {
    let dispatch = Dispatch::new(&crate::GrpcLimits::default());
    dispatch.routing().route(routes);
    dispatch
}

/// `method`'s gRPC request, body = one framed message
pub(super) fn framed_request(method: Method, message: Bytes) -> Request<Full<Bytes>> {
    request_at(&method.path(), message)
}

/// gRPC request at any `path`, body = one framed message, its method resolved as the serve stack
/// resolves it
pub(super) fn request_at(path: &str, message: Bytes) -> Request<Full<Bytes>> {
    let mut framed = Vec::with_capacity(FRAME_HEADER + message.len());
    frame_into(&mut framed, |out| out.extend_from_slice(&message));

    let mut request = Request::builder()
        .uri(format!("http://localhost{path}"))
        .body(Full::new(Bytes::from(framed)))
        .expect("request");
    Method::resolve(&mut request);
    request
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

/// Up to 10 poll rounds (paused clock: instant) until `done`
pub(super) async fn rounds(what: &str, done: impl Fn() -> bool) {
    for _ in 0..10 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    panic!("never {what}");
}
