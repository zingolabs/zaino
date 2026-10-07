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
//! - a disabled `[index.*]` = its methods `UNIMPLEMENTED`, naming the index
//! - an unknown path = `UNIMPLEMENTED`

use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use http::{Request, Response};
use tonic::{body::Body, Status};
use zaino_chainview::ChainView;
use zaino_persistence::{IndexKind, MapRead, SequenceRead};
use zaino_source::{ChainDataSource, TrafficBalancer};
use zcash_protocol::consensus::NetworkType;

use crate::limits::ReadLanes;
use crate::routes::{blocks, chain, transparent_address, tree_state};
use crate::wire::{frame, path, status_response, unary_response};

/// What one `GrpcService` answers from: the always-on routes, and each enabled `[index.*]`
///
/// - `None` = that index disabled: its methods `UNIMPLEMENTED`, naming the index
/// - `V` = the persistence engine's view every index reads through (zainod: `DiskView`)
pub struct Routes<S: ChainDataSource, V> {
    /// `SendTransaction`, `GetMempoolTx`, `GetMempoolStream`; `GetLightdInfo`'s chain half
    pub chain: Arc<ChainView<S>>,
    /// `GetTransaction`, and `GetTaddressTransactions`' bytes (the index names them)
    pub validators: TrafficBalancer<S>,
    /// Declared, never read off a validator (zebra on regtest reports `"test"`)
    pub network: NetworkType,
    /// Block methods + `GetLightdInfo.blockHeight` (always on: config refuses disabling it)
    pub compact_block: zaino_index_compact_block::CompactBlockService<V>,
    /// Locator for every `BlockID.hash`; `None` = by-hash requests `UNIMPLEMENTED`
    pub block_hash: Option<zaino_internal_block_hash_to_height::BlockHashService<V>>,
    pub tree_state: Option<zaino_index_tree_state::TreeStateService<V>>,
    pub transparent_address: Option<zaino_index_transparent_address::TransparentAddressService<V>>,
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
    /// One request, answered by the route its path names (handles cloned only for that route)
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
                let (service, locator) = (routes.compact_block.clone(), routes.block_hash.clone());
                blocks::dispatch(service, locator, path, body, reads()).await
            }
            path::GET_TREE_STATE | path::GET_LATEST_TREE_STATE | path::GET_SUBTREE_ROOTS => {
                let Some(service) = routes.tree_state.clone() else {
                    return not_enabled(path, IndexKind::TreeState.name());
                };
                let answering = tree_state::Answering {
                    service,
                    locator: routes.block_hash.clone(),
                    reads: reads(),
                    memos: Arc::clone(&self.tree_states),
                };
                tree_state::dispatch(answering, path, body).await
            }
            path::GET_ADDRESS_UTXOS
            | path::GET_ADDRESS_UTXOS_STREAM
            | path::GET_TADDRESS_BALANCE
            | path::GET_TADDRESS_BALANCE_STREAM => match routes.transparent_address.clone() {
                Some(service) => transparent_address::dispatch(service, path, body, reads()).await,
                None => not_enabled(path, IndexKind::TransparentAddress.name()),
            },
            path::GET_TADDRESS_TRANSACTIONS | path::GET_TADDRESS_TXIDS => {
                let Some(service) = routes.transparent_address.clone() else {
                    return not_enabled(path, IndexKind::TransparentAddress.name());
                };
                let validators = routes.validators.clone();
                transparent_address::transactions(service, validators, body, reads()).await
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
                match chain::lightd_info(&view, &routes.compact_block, routes.network) {
                    Ok(info) => unary_response(frame(&info)),
                    Err(status) => status_response(status),
                }
            }
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
    use zaino_index_transparent_address::TransparentAddressService;
    use zaino_sync::Served;

    use super::Routes;
    use crate::testing::{dispatch, framed_request, request, routes};
    use crate::wire::path;

    /// Only the always-on routes + an empty compact-block index: an index the config left off is
    /// `UNIMPLEMENTED` naming that index; an unknown path is `UNIMPLEMENTED`; the empty index
    /// and `GetLightdInfo` with no verified tip are `UNAVAILABLE` (retry), never a panic
    #[tokio::test]
    async fn a_disabled_index_or_unknown_path_is_unimplemented_and_names_why() {
        use tonic::Code::{Unavailable, Unimplemented};
        use tower::Service as _;

        let mut service = dispatch(routes());
        let off = |method: &str, index: &str| {
            (Unimplemented, format!("{method} needs the {index} index, which is not enabled"))
        };
        let unknown = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/Ping";
        let health = "/grpc.health.v1.Health/Check";
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
            (unknown, (Unimplemented, format!("{unknown}: no such method"))),
            (health, (Unimplemented, format!("{health}: no such method"))),
            (path::GET_LIGHTD_INFO, (Unavailable, "no verified header chain tip yet".to_owned())),
        ];
        for (path, want) in cases {
            let response = service.call(request(path)).await.expect("infallible");
            let status = Status::from_header_map(response.headers()).expect("a status");
            assert_eq!((status.code(), status.message().to_owned()), want, "{path}");
        }
        let response = service.call(request(path::GET_LATEST_BLOCK)).await.expect("infallible");
        let status = Status::from_header_map(response.headers()).expect("a status");
        assert_eq!(status.code(), Unavailable, "empty index = retry");
    }

    /// Three indexes on one router each answer their own paths and nobody else's, and an index
    /// that is still syncing with nothing committed answers `Unavailable` on every one of them
    #[tokio::test]
    async fn every_index_claims_its_own_paths_and_a_syncing_one_says_retry() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_compact_block::{testing, CompactBlockService};
        use zaino_index_transparent_address::TransparentAddressIndexWriter;
        use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateService};
        use zaino_proto::proto::service as proto;

        use crate::testing::store;

        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let batch = std::num::NonZeroUsize::MIN;

        let (compact_synced, compact_synced_rx) = tokio::sync::watch::channel(false);
        let compact_store = store("/compact-block", &zaino_index_compact_block::schema(net));
        let compact_block = CompactBlockService::new(Served::new(
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(testing::committed(
                compact_store,
                0,
            ))),
            compact_synced_rx,
        ));
        let tree_state_store = store("/tree-state", &zaino_index_tree_state::schema(net));
        let tree_state_index = TreeStateIndexWriter::new(tree_state_store, batch);
        // Start unsynced, flip below: the refusal and the answer come from one wiring.
        let (tree_state_synced, tree_state_synced_rx) = tokio::sync::watch::channel(false);
        let (transparent_synced, transparent_synced_rx) = tokio::sync::watch::channel(false);

        // each index's own boot view, behind a gate this test flips
        let boot_view = tree_state_index.published().served().pin_any();
        let tree_state = TreeStateService::new(
            Served::new(
                std::sync::Arc::new(arc_swap::ArcSwap::from_pointee((*boot_view).clone())),
                tree_state_synced_rx,
            ),
            net,
            zaino_index_tree_state::PoolActivations {
                sapling: zaino_primitives::types::Height::GENESIS,
                orchard: Some(zaino_primitives::types::Height::GENESIS),
                ironwood: Some(zaino_primitives::types::Height::GENESIS),
            },
        );
        let transparent_store =
            store("/transparent-address", &zaino_index_transparent_address::schema(net));
        let transparent_index = TransparentAddressIndexWriter::new(transparent_store, batch);
        let boot_view = transparent_index.published().served().pin_any();
        let transparent = TransparentAddressService::new(
            Served::new(
                std::sync::Arc::new(arc_swap::ArcSwap::from_pointee((*boot_view).clone())),
                transparent_synced_rx,
            ),
            net,
        );

        let mut router = dispatch(Routes {
            compact_block,
            tree_state: Some(tree_state),
            transparent_address: Some(transparent),
            ..routes()
        });

        // Every index path, all three indexes: UNAVAILABLE (14), never UNIMPLEMENTED/NOT_FOUND
        // - exhaustive: a path missing the gate leaks a partial index (only per-path catches it)
        let syncing = [
            (path::GET_LATEST_BLOCK, proto::ChainSpec::default().encode_to_vec()),
            (path::GET_BLOCK, proto::BlockId { height: 0, hash: Vec::new() }.encode_to_vec()),
            (
                path::GET_BLOCK_RANGE,
                proto::BlockRange {
                    start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                    end: Some(proto::BlockId { height: 1, hash: Vec::new() }),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                path::GET_BLOCK_RANGE_NULLIFIERS,
                proto::BlockRange {
                    start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                    end: Some(proto::BlockId { height: 1, hash: Vec::new() }),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                path::GET_SUBTREE_ROOTS,
                proto::GetSubtreeRootsArg {
                    start_index: 0,
                    shielded_protocol: proto::ShieldedProtocol::Orchard as i32,
                    max_entries: 0,
                }
                .encode_to_vec(),
            ),
            (
                path::GET_ADDRESS_UTXOS_STREAM,
                proto::GetAddressUtxosArg {
                    addresses: vec!["tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_owned()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec(),
            ),
            // Client-streaming, so the body is a run of framed `Address`; one is enough.
            (
                path::GET_TADDRESS_BALANCE_STREAM,
                proto::Address { address: "tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_owned() }
                    .encode_to_vec(),
            ),
            (path::GET_TREE_STATE, proto::BlockId { height: 0, hash: Vec::new() }.encode_to_vec()),
            (path::GET_LATEST_TREE_STATE, Vec::new()),
            (
                path::GET_ADDRESS_UTXOS,
                proto::GetAddressUtxosArg {
                    addresses: vec!["tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_owned()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec(),
            ),
            (
                path::GET_TADDRESS_BALANCE,
                proto::AddressList {
                    addresses: vec!["tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_owned()],
                }
                .encode_to_vec(),
            ),
            (
                path::GET_TADDRESS_TRANSACTIONS,
                proto::TransparentAddressBlockFilter {
                    address: "tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_owned(),
                    range: Some(proto::BlockRange {
                        start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                        end: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                        pool_types: Vec::new(),
                    }),
                }
                .encode_to_vec(),
            ),
        ];
        for (path, message) in syncing {
            let response =
                router.call(framed_request(path, message.into())).await.expect("router answers");
            // 14 = back off + retry (12 retires the method, 9 = client can fix it)
            let status = response.headers().get("grpc-status");
            assert_eq!(status, Some(&HeaderValue::from_static("14")), "{path}");
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

        // Caught up: the same router now answers rather than refusing, so the two states differ
        // only in the flag and not in how anything is wired.
        compact_synced.send_replace(true);
        tree_state_synced.send_replace(true);
        transparent_synced.send_replace(true);

        // `GetSubtreeRoots` past the end is pepper-sync's probe pass: an empty list, not a status.
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
        let status = response.headers().get("grpc-status");
        assert_eq!(status, None, "stream status rides in the trailers");
        let (chunks, trailing) = drained(response).await;
        assert!(chunks.is_empty(), "no subtree is complete yet");
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
    }
}
