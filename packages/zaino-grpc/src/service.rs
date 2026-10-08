//! `CompactTxStreamer` service: every method dispatched by path, over [`Routes`]
//!
//! - By hand, not tonic's generated trait (decoded messages only: stored gRPC-framed records
//!   would be decoded to be re-encoded; usage.md "Stored bytes on the wire")
//! - one global [`Snapshot`](zaino_snapshot::Snapshot) per request or stream (G1), pinned for
//!   its life: every index answers at heights `<=` its served tip (`GetLatestBlock` = that tip)
//! - `Unavailable` (nothing served, no chain, no holder) = `UNAVAILABLE`, its message
//! - a disabled `[index.*]` = its methods `UNIMPLEMENTED`, naming the index
//! - an unknown path = `UNIMPLEMENTED`

use std::{
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use http::{Request, Response};
use tonic::{body::Body, Status};
use zaino_chainview::ChainView;
use zaino_persistence::{IndexKind, MapRead, SequenceRead};
use zaino_snapshot::{Snapshots, Unavailable};
use zaino_source::ChainDataSource;
use zaino_traffic::TrafficBalancer;
use zcash_protocol::consensus::NetworkType;

use crate::deadline::Bounds;
use crate::limits::ReadLanes;
use crate::routes::{blocks, chain, transparent_address, tree_state};
use crate::wire::{self, frame, path, status_response, unary_response};
use crate::GrpcLimits;

/// What one `GrpcService` answers from
///
/// - `snapshots` = every read (chain, indexes, mempool, validator facts)
/// - `submit` = `SendTransaction` only; `validators` = `GetTransaction` + address tx bytes
/// - `network` = declared, never read off a validator (zebra on regtest reports `"test"`)
/// - `max_address_rows` = receives one transparent-address request may walk
pub struct Routes<S: ChainDataSource, V> {
    pub snapshots: Snapshots<V>,
    pub submit: Arc<ChainView<S>>,
    pub validators: TrafficBalancer<S>,
    pub network: NetworkType,
    pub max_address_rows: NonZeroUsize,
}

/// Routes + what every request shares, one `Arc` (a request clones one pointer)
///
/// - `reads` process-wide (every index read on the blocking pool under its lane's permit)
/// - `idle` = `GrpcLimits::stall_timeout` (nothing to send that long = cut, `deadline.rs`)
struct Wired<S: ChainDataSource, V> {
    routes: Routes<S, V>,
    reads: ReadLanes,
    tree_states: Arc<tree_state::Memos<V>>,
    idle: Duration,
}

/// Every `CompactTxStreamer` method, dispatched by path (the tower service each connection runs)
pub(crate) struct Dispatch<S: ChainDataSource, V>(Arc<Wired<S, V>>);

impl<S: ChainDataSource, V> Clone for Dispatch<S, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Dispatch<S, V> {
    pub(crate) fn new(routes: Routes<S, V>, limits: &GrpcLimits) -> Self {
        let (reads, idle) = (ReadLanes::new(limits), limits.stall_timeout);
        Self(Arc::new(Wired { routes, reads, tree_states: Arc::default(), idle }))
    }
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Wired<S, V> {
    /// One request, answered by the route its path names
    async fn answer<B>(&self, path: &str, body: B) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let routes = &self.routes;
        let reads = || self.reads.clone();
        let snap = routes.snapshots.load();
        let unavailable = |why: Unavailable| status_response(wire::unavailable(why));
        match path {
            path::GET_LATEST_BLOCK
            | path::GET_BLOCK
            | path::GET_BLOCK_RANGE
            | path::GET_BLOCK_RANGE_NULLIFIERS => {
                let at = match snap.served() {
                    Ok(at) => at,
                    Err(why) => return unavailable(why),
                };
                let Some(blocks) = at.views().compact_block() else {
                    return not_enabled(path, IndexKind::CompactBlock.name());
                };
                blocks::dispatch(at, blocks, path, body, reads()).await
            }
            path::GET_TREE_STATE | path::GET_LATEST_TREE_STATE | path::GET_SUBTREE_ROOTS => {
                // the NFS publish itself: memo identity (`served()` = its served tip)
                let Some(indexed) = snap.indexed() else {
                    return unavailable(Unavailable::NothingServed);
                };
                let Some(trees) = indexed.served().views().tree_state() else {
                    return not_enabled(path, IndexKind::TreeState.name());
                };
                let (indexed, memos) = (Arc::clone(indexed), Arc::clone(&self.tree_states));
                let answering = tree_state::Answering { indexed, trees, reads: reads(), memos };
                tree_state::dispatch(answering, path, body).await
            }
            path::GET_ADDRESS_UTXOS
            | path::GET_ADDRESS_UTXOS_STREAM
            | path::GET_TADDRESS_BALANCE
            | path::GET_TADDRESS_BALANCE_STREAM
            | path::GET_TADDRESS_TRANSACTIONS
            | path::GET_TADDRESS_TXIDS => {
                let at = match snap.served() {
                    Ok(at) => at,
                    Err(why) => return unavailable(why),
                };
                let Some(reader) = at.views().transparent_address() else {
                    return not_enabled(path, IndexKind::TransparentAddress.name());
                };
                let reader = reader.as_of(at.tip().height).with_max_rows(routes.max_address_rows);
                let index = transparent_address::Addresses { reader, network: at.params().network };
                match path {
                    path::GET_TADDRESS_TRANSACTIONS | path::GET_TADDRESS_TXIDS => {
                        let validators = routes.validators.clone();
                        transparent_address::transactions(index, validators, body, reads()).await
                    }
                    _ => transparent_address::dispatch(index, path, body, reads()).await,
                }
            }
            path::SEND_TRANSACTION => match chain::send(&routes.submit, body).await {
                Ok(record) => unary_response(record),
                Err(status) => status_response(status),
            },
            path::GET_MEMPOOL_TX => match chain::mempool_tx(&snap, body).await {
                Ok(records) => wire::streamed_response(records),
                Err(status) => status_response(status),
            },
            path::GET_MEMPOOL_STREAM => chain::mempool_stream(&snap),
            path::GET_TRANSACTION => match chain::transaction(&routes.validators, body).await {
                Ok(record) => unary_response(record),
                Err(status) => status_response(status),
            },
            path::GET_LIGHTD_INFO => match chain::lightd_info(&snap, routes.network) {
                Ok(info) => unary_response(frame(&info)),
                Err(status) => status_response(status),
            },
            unknown => status_response(Status::unimplemented(format!("{unknown}: no such method"))),
        }
    }
}

/// A method whose index this operator disabled
fn not_enabled(path: &str, index: &str) -> Response<Body> {
    let method = path.rsplit('/').next().unwrap_or(path);
    let why = format!("{method} needs the {index} index, which is not enabled");
    status_response(Status::unimplemented(why))
}

/// Never fails: every refusal is a gRPC status in the response
impl<S, V, ReqBody> tower::Service<Request<ReqBody>> for Dispatch<S, V>
where
    S: ChainDataSource,
    V: SequenceRead + MapRead,
    ReqBody: http_body::Body + Send + 'static,
    ReqBody::Data: Send,
    ReqBody::Error: std::fmt::Display,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let wired = Arc::clone(&self.0);
        let path = request.uri().path().to_owned();
        // a mempool subscription stays silent until the next block: never idles out
        let idle = (path != path::GET_MEMPOOL_STREAM).then_some(wired.idle);
        let bounds = Bounds::of(request.headers(), idle);
        Box::pin(async move { Ok(bounds.apply(wired.answer(&path, request.into_body())).await) })
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue, Response};
    use tonic::{body::Body, Status};
    use zaino_persistence::IndexKind;

    use super::Routes;
    use crate::testing::{
        dispatch, framed_request, indexed, request, routes, snapshot, snapshot_held,
    };
    use crate::wire::path;

    /// - Nothing published: every index method `UNAVAILABLE` (retry), `GetLightdInfo` without a
    ///   verified tip too; unknown path `UNIMPLEMENTED`
    /// - Compact-block-only snapshot: a disabled index = `UNIMPLEMENTED` naming it
    #[tokio::test]
    async fn a_disabled_index_or_unknown_path_is_unimplemented_and_nothing_served_says_retry() {
        use tonic::Code::{Unavailable, Unimplemented};
        use tower::Service as _;

        let mut booting = dispatch(routes());
        let unknown = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/Ping";
        let health = "/grpc.health.v1.Health/Check";
        let syncing = "the indexes are syncing: nothing served yet".to_owned();
        let cases = [
            (path::GET_LATEST_BLOCK, (Unavailable, syncing.clone())),
            (path::GET_TREE_STATE, (Unavailable, syncing.clone())),
            (path::GET_ADDRESS_UTXOS, (Unavailable, syncing)),
            (unknown, (Unimplemented, format!("{unknown}: no such method"))),
            (health, (Unimplemented, format!("{health}: no such method"))),
            (path::GET_LIGHTD_INFO, (Unavailable, "no verified header chain tip yet".to_owned())),
        ];
        for (path, want) in cases {
            let response = booting.call(request(path)).await.expect("infallible");
            let status = Status::from_header_map(response.headers()).expect("a status");
            assert_eq!((status.code(), status.message().to_owned()), want, "{path}");
        }

        let mut chain = zaino_primitives::testing::Chain::new();
        let tip = chain.extend(chain.genesis().hash, 1);
        let blocks = chain.path(tip.hash);
        let compact = indexed(IndexKind::CompactBlock, &blocks);
        let mut serving =
            dispatch(Routes { snapshots: snapshot(&blocks, vec![compact]), ..routes() });
        let off = |method: &str, index: &str| {
            (Unimplemented, format!("{method} needs the {index} index, which is not enabled"))
        };
        let cases = [
            (path::GET_TREE_STATE, off("GetTreeState", "tree_state")),
            (path::GET_LATEST_TREE_STATE, off("GetLatestTreeState", "tree_state")),
            (path::GET_SUBTREE_ROOTS, off("GetSubtreeRoots", "tree_state")),
            (path::GET_ADDRESS_UTXOS, off("GetAddressUtxos", "transparent_address")),
            (path::GET_TADDRESS_BALANCE, off("GetTaddressBalance", "transparent_address")),
            (
                path::GET_TADDRESS_TRANSACTIONS,
                off("GetTaddressTransactions", "transparent_address"),
            ),
        ];
        for (path, want) in cases {
            let response = serving.call(request(path)).await.expect("infallible");
            let status = Status::from_header_map(response.headers()).expect("a status");
            assert_eq!((status.code(), status.message().to_owned()), want, "{path}");
        }
    }

    /// G1, R12: one global snapshot, every RPC at one tip
    /// - Views hold 0..=3, snapshot serves 2 (root snapshot in bulk sync: views ahead of its tip);
    ///   validator 0 holds 2, one tx of ours relayed
    /// - `GetLatestBlock` = 2, `GetBlockRange` 0..=9 = 0..=2 then `OUT_OF_RANGE`,
    ///   `GetLatestTreeState` = 2's,
    ///   `GetLightdInfo.blockHeight` = 2 + the holder's branch
    /// - `GetTreeState` 3 = a miss (never past the served tip); block hash = its tree state's
    /// - mempool: `GetMempoolTx` gated open, `GetMempoolStream` opens on our relay, then ends (a
    ///   fixed snapshot never moves on)
    #[tokio::test]
    async fn every_rpc_agrees_on_one_snapshot() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_chainview::EndpointSet;
        use zaino_proto::frame::{split_frame, FRAME_HEADER};
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        let mut chain = zaino_primitives::testing::Chain::new();
        let tip = chain.extend(chain.genesis().hash, 3);
        let blocks = chain.path(tip.hash);
        let views =
            vec![indexed(IndexKind::CompactBlock, &blocks), indexed(IndexKind::TreeState, &blocks)];
        let raw = zaino_source::mock::fixture_transactions(2_000_000).remove(0);
        let txid = zaino_source::decode_transaction(&raw).expect("a mainnet tx").txid;
        let ours = [(txid, bytes::Bytes::from(raw.clone()))];
        let snapshots = snapshot_held(&blocks[..=2], views, EndpointSet::at([0]), &ours);
        let mut router = dispatch(Routes { snapshots, ..routes() });
        let mut call = |path: &'static str, message: Vec<u8>| {
            let response = router.call(framed_request(path, message.into()));
            async move {
                use http_body_util::BodyExt as _;
                let response = response.await.expect("router answers");
                let code = Status::from_header_map(response.headers()).map(|s| s.code());
                let body = response.into_body().collect().await.expect("body");
                let trailers = body.trailers().cloned().unwrap_or_else(HeaderMap::new);
                (code, body.to_bytes(), trailers)
            }
        };
        let at = |height| proto::BlockId { height, hash: Vec::new() };

        let (_, latest, _) = call(path::GET_LATEST_BLOCK, Vec::new()).await;
        let latest = proto::BlockId::decode(&latest[FRAME_HEADER..]).expect("a BlockID");
        let tip_hash = <[u8; 32]>::from(blocks[2].header().hash).to_vec();
        assert_eq!((latest.height, latest.hash), (2, tip_hash.clone()), "the snapshot's tip");

        let range = proto::BlockRange { start: Some(at(0)), end: Some(at(9)), pool_types: vec![] };
        let (_, streamed, trailers) = call(path::GET_BLOCK_RANGE, range.encode_to_vec()).await;
        let past = HeaderValue::from_static("11");
        assert_eq!(trailers.get("grpc-status"), Some(&past), "to the tip, then OUT_OF_RANGE");
        let mut rest = &streamed[..];
        let mut served = Vec::new();
        while !rest.is_empty() {
            let (message, tail) = split_frame(rest).expect("whole frame");
            let block = cf::CompactBlock::decode(message).expect("a CompactBlock");
            served.push((block.height, block.hash));
            rest = tail;
        }
        let expected: Vec<_> = (0..=2u64)
            .map(|height| {
                (height, <[u8; 32]>::from(blocks[height as usize].header().hash).to_vec())
            })
            .collect();
        assert_eq!(served, expected, "clamped at the snapshot tip, not the index's");

        let (_, state, _) = call(path::GET_LATEST_TREE_STATE, Vec::new()).await;
        let state = proto::TreeState::decode(&state[FRAME_HEADER..]).expect("a TreeState");
        let mut display = tip_hash;
        display.reverse();
        assert_eq!((state.height, state.hash), (2, hex::encode(display)), "same tip, same block");
        let (code, _, _) = call(path::GET_TREE_STATE, at(3).encode_to_vec()).await;
        assert_eq!(code, Some(tonic::Code::NotFound), "3 held, past the served tip");
        let (code, _, _) = call(path::GET_BLOCK, at(3).encode_to_vec()).await;
        assert_eq!(code, Some(tonic::Code::NotFound), "GetBlock agrees");

        let (_, info, _) = call(path::GET_LIGHTD_INFO, Vec::new()).await;
        let info = proto::LightdInfo::decode(&info[FRAME_HEADER..]).expect("a LightdInfo");
        let lightd = (info.block_height, info.estimated_height, info.consensus_branch_id.as_str());
        assert_eq!(lightd, (2, 2, "00000000"), "served height + the holder's view, one load");

        let every = vec![1, 2, 3, 4];
        let request = proto::GetMempoolTxRequest { pool_types: every, ..Default::default() };
        let request = request.encode_to_vec();
        let (_, listed, trailers) = call(path::GET_MEMPOOL_TX, request).await;
        let (compact, rest) = split_frame(&listed).expect("one CompactTx");
        let compact = cf::CompactTx::decode(compact).expect("a CompactTx");
        let ours = <[u8; 32]>::from(txid).to_vec();
        assert_eq!((compact.txid, rest.len()), (ours, 0), "held: our relay, compacted");
        assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let (_, streamed, trailers) = call(path::GET_MEMPOOL_STREAM, Vec::new()).await;
        let (opening, rest) = split_frame(&streamed).expect("the opening");
        let opening = proto::RawTransaction::decode(opening).expect("a RawTransaction");
        let opened = (opening.data.as_ref(), opening.height, rest.len());
        assert_eq!(opened, (&raw[..], 0, 0), "our relay, unmined, then the end");
        assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
    }

    /// Every index path on one router, each index enabled: each answers `OK` off its own index;
    /// `GetSubtreeRoots` past the end = an empty stream (pepper-sync's probe), never a status
    #[tokio::test]
    async fn every_index_claims_its_own_paths() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service as proto;

        let mut chain = zaino_primitives::testing::Chain::new();
        let tip = chain.extend(chain.genesis().hash, 1);
        let blocks = chain.path(tip.hash);
        let kinds = [IndexKind::CompactBlock, IndexKind::TreeState, IndexKind::TransparentAddress];
        let views = kinds.into_iter().map(|kind| indexed(kind, &blocks)).collect();
        let mut router = dispatch(Routes { snapshots: snapshot(&blocks, views), ..routes() });

        let address = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs".to_owned();
        let range = proto::BlockRange {
            start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
            end: Some(proto::BlockId { height: 1, hash: Vec::new() }),
            pool_types: Vec::new(),
        };
        let every = [
            (path::GET_LATEST_BLOCK, proto::ChainSpec::default().encode_to_vec()),
            (path::GET_BLOCK, proto::BlockId { height: 0, hash: Vec::new() }.encode_to_vec()),
            (path::GET_BLOCK_RANGE, range.encode_to_vec()),
            (path::GET_BLOCK_RANGE_NULLIFIERS, range.encode_to_vec()),
            (path::GET_TREE_STATE, proto::BlockId { height: 0, hash: Vec::new() }.encode_to_vec()),
            (path::GET_LATEST_TREE_STATE, Vec::new()),
            (
                path::GET_ADDRESS_UTXOS,
                proto::GetAddressUtxosArg {
                    addresses: vec![address.clone()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec(),
            ),
            (
                path::GET_TADDRESS_BALANCE,
                proto::AddressList { addresses: vec![address.clone()] }.encode_to_vec(),
            ),
            (
                path::GET_TADDRESS_BALANCE_STREAM,
                proto::Address { address: address.clone() }.encode_to_vec(),
            ),
        ];
        for (path, message) in every {
            let response =
                router.call(framed_request(path, message.into())).await.expect("router answers");
            let status = response.headers().get("grpc-status");
            let ok = [None, Some(&HeaderValue::from_static("0"))];
            assert!(ok.contains(&status), "{path}: {status:?}");
        }

        async fn drained(response: Response<Body>) -> (Vec<bytes::Bytes>, HeaderMap) {
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

        let response = router
            .call(framed_request(
                path::GET_SUBTREE_ROOTS,
                proto::GetSubtreeRootsArg {
                    start_index: 0,
                    shielded_protocol: proto::ShieldedProtocol::Orchard as i32,
                    max_entries: 0,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), None, "status rides in the trailers");
        let (chunks, trailing) = drained(response).await;
        assert!(chunks.is_empty(), "no subtree is complete yet");
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
    }
}
