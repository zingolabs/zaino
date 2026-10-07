//! The `CompactTxStreamer` service: every method dispatched by path, over [`Routes`]
//!
//! Why by hand rather than tonic's generated trait: that trait takes and returns decoded
//! messages, so a compact-block record would be decoded only for tonic to encode it again. Those
//! records are *already* gRPC-framed (`[0x00][len][message]`), which is exactly the shape of an
//! HTTP/2 response body — a unary response is one record and a server-streaming response is the
//! records concatenated, so the bytes go out as the body and no `CompactBlock` is constructed.
//! The other routes answer with domain values, so their dispatch builds the proto message and
//! frames it ([`crate::wire`]).
//!
//! - one [`Snapshot`] per index request or stream, pinned for its life: every index answers at
//!   heights `<=` its tip (`GetLatestBlock` = that tip)
//! - no snapshot yet (a booting NFS) = every index method `UNAVAILABLE`
//! - a disabled `[index.*]` = its methods `UNIMPLEMENTED`, naming the index
//! - an unknown path = `UNIMPLEMENTED`

use std::{
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use http::{Request, Response};
use tonic::{body::Body, Status};
use zaino_chainview::ChainView;
use zaino_nfs::{NfsHandle, Snapshot};
use zaino_persistence::{IndexKind, MapRead, SequenceRead};
use zaino_source::{ChainDataSource, TrafficBalancer};
use zcash_protocol::consensus::NetworkType;

use crate::limits::ReadLanes;
use crate::routes::{blocks, chain, transparent_address, tree_state};
use crate::wire::{frame, path, status_response, unary_response};

/// What one `GrpcService` answers from: the chain view, the validators, the NFS's snapshots
///
/// - `network` = declared, never read off a validator (zebra on regtest reports `"test"`)
/// - `max_address_rows` = receives one transparent-address request may walk
pub struct Routes<S: ChainDataSource, V> {
    pub chain: Arc<ChainView<S>>,
    pub validators: TrafficBalancer<S>,
    pub network: NetworkType,
    pub nfs: NfsHandle<V>,
    pub max_address_rows: NonZeroUsize,
}

/// The routes + what every request shares, behind one `Arc` (a request clones one pointer)
struct Wired<S: ChainDataSource, V> {
    routes: Routes<S, V>,
    /// Process-wide: every index read runs on the blocking pool under a permit of its lane
    reads: ReadLanes,
    tree_states: Arc<tree_state::Memos<V>>,
}

/// Every `CompactTxStreamer` method, dispatched by path (the tower service each connection runs)
pub(crate) struct Dispatch<S: ChainDataSource, V>(Arc<Wired<S, V>>);

impl<S: ChainDataSource, V> Clone for Dispatch<S, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Dispatch<S, V> {
    pub(crate) fn new(routes: Routes<S, V>, reads: ReadLanes) -> Self {
        Self(Arc::new(Wired { routes, reads, tree_states: Arc::default() }))
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
        match path {
            path::GET_LATEST_BLOCK
            | path::GET_BLOCK
            | path::GET_BLOCK_RANGE
            | path::GET_BLOCK_RANGE_NULLIFIERS => {
                let snap = match self.snapshot() {
                    Ok(snap) => snap,
                    Err(syncing) => return status_response(syncing),
                };
                let Some(blocks) = snap.views().compact_block() else {
                    return not_enabled(path, IndexKind::CompactBlock.name());
                };
                blocks::dispatch(&snap, blocks, path, body, reads()).await
            }
            path::GET_TREE_STATE | path::GET_LATEST_TREE_STATE | path::GET_SUBTREE_ROOTS => {
                let snap = match self.snapshot() {
                    Ok(snap) => snap,
                    Err(syncing) => return status_response(syncing),
                };
                let Some(trees) = snap.views().tree_state() else {
                    return not_enabled(path, IndexKind::TreeState.name());
                };
                let memos = Arc::clone(&self.tree_states);
                let answering = tree_state::Answering { snap, trees, reads: reads(), memos };
                tree_state::dispatch(answering, path, body).await
            }
            path::GET_ADDRESS_UTXOS
            | path::GET_ADDRESS_UTXOS_STREAM
            | path::GET_TADDRESS_BALANCE
            | path::GET_TADDRESS_BALANCE_STREAM
            | path::GET_TADDRESS_TRANSACTIONS
            | path::GET_TADDRESS_TXIDS => {
                let snap = match self.snapshot() {
                    Ok(snap) => snap,
                    Err(syncing) => return status_response(syncing),
                };
                let Some(reader) = snap.views().transparent_address() else {
                    return not_enabled(path, IndexKind::TransparentAddress.name());
                };
                let reader = reader.as_of(snap.tip().height).with_max_rows(routes.max_address_rows);
                let index =
                    transparent_address::Addresses { reader, network: snap.params().network };
                match path {
                    path::GET_TADDRESS_TRANSACTIONS | path::GET_TADDRESS_TXIDS => {
                        let validators = routes.validators.clone();
                        transparent_address::transactions(index, validators, body, reads()).await
                    }
                    _ => transparent_address::dispatch(index, path, body, reads()).await,
                }
            }
            path::SEND_TRANSACTION | path::GET_MEMPOOL_TX | path::GET_MEMPOOL_STREAM => {
                chain::dispatch(&routes.chain, path, body).await
            }
            path::GET_TRANSACTION => match chain::transaction(&routes.validators, body).await {
                Ok(record) => unary_response(record),
                Err(status) => status_response(status),
            },
            path::GET_LIGHTD_INFO => {
                let view = routes.chain.subscriber();
                let served = routes.nfs.snapshot().map(|snap| snap.tip().height);
                match chain::lightd_info(&view, served, routes.network) {
                    Ok(info) => unary_response(frame(&info)),
                    Err(status) => status_response(status),
                }
            }
            unknown => status_response(Status::unimplemented(format!("{unknown}: no such method"))),
        }
    }

    /// The current snapshot (`UNAVAILABLE` before the first: retry, the indexes are opening)
    fn snapshot(&self) -> Result<Arc<Snapshot<V>>, Status> {
        let snap = self.routes.nfs.snapshot();
        snap.ok_or_else(|| Status::unavailable("the indexes are syncing: nothing served yet"))
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
        Box::pin(async move {
            let path = request.uri().path().to_owned();
            Ok(wired.answer(&path, request.into_body()).await)
        })
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue, Response};
    use tonic::{body::Body, Status};
    use zaino_nfs::NfsHandle;
    use zaino_persistence::IndexKind;

    use super::Routes;
    use crate::testing::{dispatch, framed_request, indexed, request, routes, snapshot};
    use crate::wire::path;

    /// Nothing published yet: every index method `UNAVAILABLE` (retry), `GetLightdInfo` with no
    /// verified tip too; an unknown path `UNIMPLEMENTED`. A snapshot with only compact-block: an
    /// index the config left off = `UNIMPLEMENTED` naming that index
    #[tokio::test]
    async fn a_disabled_index_or_unknown_path_is_unimplemented_and_nothing_served_says_retry() {
        use tonic::Code::{Unavailable, Unimplemented};
        use tower::Service as _;

        let mut booting = dispatch(Routes { nfs: NfsHandle::unpublished(), ..routes() });
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
        let mut serving = dispatch(Routes { nfs: snapshot(&blocks, vec![compact]), ..routes() });
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

    /// R12: one snapshot answers every RPC at one tip. Compact-block and tree-state hold 0..=3,
    /// the snapshot serves 2 (a root snapshot during bulk sync: views ahead of its tip):
    /// `GetLatestBlock` = 2, `GetBlockRange` 0..=9 stops at 2, `GetLatestTreeState` = 2's,
    /// `GetTreeState` 3 = a miss (never past the served tip), each block's hash = its tree state's
    #[tokio::test]
    async fn get_latest_block_get_block_range_and_get_tree_state_agree_on_the_snapshot_tip() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::frame::{split_frame, FRAME_HEADER};
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        let mut chain = zaino_primitives::testing::Chain::new();
        let tip = chain.extend(chain.genesis().hash, 3);
        let blocks = chain.path(tip.hash);
        let views =
            vec![indexed(IndexKind::CompactBlock, &blocks), indexed(IndexKind::TreeState, &blocks)];
        let mut router = dispatch(Routes { nfs: snapshot(&blocks[..=2], views), ..routes() });
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
        assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
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
        let mut router = dispatch(Routes { nfs: snapshot(&blocks, views), ..routes() });

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

        // Data frames in order, then the trailers.
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
