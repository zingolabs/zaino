//! `CompactTxStreamer` service: the method registry ([`ROUTES`]) and the one dispatch over it
//!
//! - By hand, not tonic's generated trait (decoded messages only: stored gRPC-framed records
//!   would be decoded to be re-encoded, usage.md "Stored bytes on the wire")
//! - one global [`GrpcSnapshot`](zaino_grpc_snapshot::GrpcSnapshot) per request or stream (G1),
//!   pinned for its life
//! - no height named = its served tip, height named = `IndexesAtBlock::answers_through`
//! - `Unavailable` (nothing served, no chain, no holder) = `UNAVAILABLE`, its message
//! - a row whose index `RouteDeps::indexes` lacks = `UNIMPLEMENTED`, naming the index
//! - an unknown path = `UNIMPLEMENTED`

use std::{
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};

use futures::future::BoxFuture;
use http::{Request, Response};
use tonic::{body::Body, Status};
use zaino_chainview::ChainView;
use zaino_grpc_snapshot::{GrpcSnapshots, Unavailable};
use zaino_persistence::{IndexKind, PinnedView};
use zaino_source::ChainDataSource;
use zaino_traffic::TrafficBalancer;
use zcash_protocol::consensus::NetworkType;

use crate::admission::Class;
use crate::deadline::Bounds;
use crate::read_lanes::ReadLanes;
use crate::routes::{
    blocks, deprecated_nullifiers, info, mempool, transactions, transparent_address, tree_state,
};
use crate::wire;
use crate::GrpcLimits;

/// Every row's path prefix (`/{service}/{method}`)
const SERVICE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/";

/// One served method: its [`ROUTES`] row at `self as usize`, its handler in [`Routed::handle`]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    GetLatestBlock,
    GetBlock,
    GetBlockRange,
    GetBlockRangeNullifiers,
    GetTreeState,
    GetLatestTreeState,
    GetSubtreeRoots,
    GetAddressUtxos,
    GetAddressUtxosStream,
    GetTaddressBalance,
    GetTaddressBalanceStream,
    GetTaddressTransactions,
    GetTaddressTxids,
    SendTransaction,
    GetMempoolTx,
    GetMempoolStream,
    GetTransaction,
    GetLightdInfo,
    GetStandardFee,
}

/// One registry row: wire name (= the `method` metric label), admission pool, reply shape (its
/// latency metric), the index whose view its handler reads (`None` = reads none: always served)
#[derive(Clone, Copy, Debug)]
pub(crate) struct Route {
    pub(crate) method: Method,
    pub(crate) name: &'static str,
    pub(crate) class: Class,
    pub(crate) reply: Reply,
    pub(crate) index: Option<IndexKind>,
}

/// The proto's response side (`returns (stream …)` = `Stream`; a client-streaming request with
/// one reply = `One`)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    One,
    Stream,
}

const fn row(
    method: Method,
    name: &'static str,
    class: Class,
    reply: Reply,
    index: Option<IndexKind>,
) -> Route {
    Route { method, name, class, reply, index }
}

/// The registry: every served method (a new RPC = a [`Method`], its row, its `handle` arm)
pub(crate) const ROUTES: [Route; 19] = {
    use Class::{Subscription, Work};
    use IndexKind::{CompactBlock, TransparentAddress, TreeState};
    use Method::*;
    use Reply::{One, Stream};

    [
        // served tip itself
        row(GetLatestBlock, "GetLatestBlock", Work, One, None),
        row(GetBlock, "GetBlock", Work, One, Some(CompactBlock)),
        row(GetBlockRange, "GetBlockRange", Work, Stream, Some(CompactBlock)),
        // TODO: REMOVE — deprecated alias of `GetBlockRange` (pepper-sync still calls it)
        row(GetBlockRangeNullifiers, "GetBlockRangeNullifiers", Work, Stream, Some(CompactBlock)),
        row(GetTreeState, "GetTreeState", Work, One, Some(TreeState)),
        row(GetLatestTreeState, "GetLatestTreeState", Work, One, Some(TreeState)),
        row(GetSubtreeRoots, "GetSubtreeRoots", Work, Stream, Some(TreeState)),
        row(GetAddressUtxos, "GetAddressUtxos", Work, One, Some(TransparentAddress)),
        row(GetAddressUtxosStream, "GetAddressUtxosStream", Work, Stream, Some(TransparentAddress)),
        row(GetTaddressBalance, "GetTaddressBalance", Work, One, Some(TransparentAddress)),
        row(
            GetTaddressBalanceStream,
            "GetTaddressBalanceStream",
            Work,
            One,
            Some(TransparentAddress),
        ),
        // index names the txs, a validator holds the bytes (`docs/architecture.md` §1)
        row(
            GetTaddressTransactions,
            "GetTaddressTransactions",
            Work,
            Stream,
            Some(TransparentAddress),
        ),
        // TODO: REMOVE — deprecated alias of `GetTaddressTransactions` (pepper-sync calls it)
        row(GetTaddressTxids, "GetTaddressTxids", Work, Stream, Some(TransparentAddress)),
        // one random entry per attempt, watched until it spreads (`docs/architecture.md` §6)
        row(SendTransaction, "SendTransaction", Work, One, None),
        row(GetMempoolTx, "GetMempoolTx", Work, Stream, None),
        // open until the served tip moves, silent between arrivals
        row(GetMempoolStream, "GetMempoolStream", Subscription, Stream, None),
        row(GetTransaction, "GetTransaction", Work, One, None),
        row(GetLightdInfo, "GetLightdInfo", Work, One, None),
        row(GetStandardFee, "GetStandardFee", Work, One, None),
    ]
};

// row `at` = `Method` discriminant `at` (`Method::route` indexes by it)
const _: () = {
    let mut at = 0;
    while at < ROUTES.len() {
        assert!(ROUTES[at].method as usize == at, "ROUTES rows out of `Method` order");
        at += 1;
    }
};

impl Method {
    /// `/{service}/{method}` → its row's method (`None` = no row)
    fn of(path: &str) -> Option<Self> {
        let name = path.strip_prefix(SERVICE)?;
        ROUTES.iter().find(|route| route.name == name).map(|route| route.method)
    }

    /// Once per request, by the outermost layer (`HealthRoute`): every inner layer reads it back
    pub(crate) fn resolve<B>(request: &mut Request<B>) {
        if let Some(method) = Self::of(request.uri().path()) {
            request.extensions_mut().insert(method);
        }
    }

    /// What [`Self::resolve`] named (`None` = no row)
    pub(crate) fn of_request<B>(request: &Request<B>) -> Option<Self> {
        request.extensions().get::<Self>().copied()
    }

    pub(crate) const fn route(self) -> Route {
        ROUTES[self as usize]
    }

    #[cfg(test)]
    pub(crate) fn path(self) -> String {
        format!("{SERVICE}{}", self.route().name)
    }
}

impl Route {
    /// Open until the next block, silent between arrivals: never idles out, no latency to time
    pub(crate) fn is_subscription(self) -> bool {
        self.class == Class::Subscription
    }
}

/// What one `GrpcService` answers from
///
/// - `grpc_snapshots` = every read (chain, indexes, mempool, validator facts)
/// - `chain_view` = `SendTransaction` only (validate, then broadcast)
/// - `validators` = `GetTransaction` + address tx bytes
/// - `network` = declared (zainod's `network` key), never read off a validator
/// - `version` = the serving daemon's (`GetLightdInfo`), not this crate's
/// - `max_address_rows` = receives one transparent-address request may walk
/// - `indexes` = the configured ones: a `ROUTES` row needing another = `UNIMPLEMENTED`
pub struct RouteDeps<S: ChainDataSource, V> {
    pub grpc_snapshots: GrpcSnapshots<V>,
    pub chain_view: Arc<ChainView<S>>,
    pub validators: TrafficBalancer<S>,
    pub network: NetworkType,
    pub version: &'static str,
    pub max_address_rows: NonZeroUsize,
    pub indexes: Vec<IndexKind>,
}

/// Every method's answer before [`Routing::route`] (the listener serves health from bind on)
pub(crate) const STARTING: &str = "starting: not routed yet";

/// [`RouteDeps`] + what every request shares, one `Arc` (a request clones one pointer)
///
/// - `routed` set once, after bind (empty = every method `UNAVAILABLE` [`STARTING`])
/// - `reads` process-wide (every index read on the blocking pool under its lane's permit)
struct Wired<S: ChainDataSource, V> {
    routed: OnceLock<Routed<S, V>>,
    reads: ReadLanes,
    tree_states: Arc<tree_state::Memos<V>>,
    silent_timeout: Duration,
}

/// [`RouteDeps`] + each row's unconfigured index, fixed at [`Routing::route`] (`None` = served)
struct Routed<S: ChainDataSource, V> {
    routes: RouteDeps<S, V>,
    disabled_by: [Option<IndexKind>; ROUTES.len()],
}

/// Every `CompactTxStreamer` method, dispatched by path (the tower service each connection runs)
pub(crate) struct Dispatch<S: ChainDataSource, V>(Arc<Wired<S, V>>);

impl<S: ChainDataSource, V> Clone for Dispatch<S, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: ChainDataSource, V: PinnedView> Dispatch<S, V> {
    pub(crate) fn new(limits: &GrpcLimits) -> Self {
        let (reads, silent_timeout) = (ReadLanes::new(limits), limits.silent_timeout);
        let routed = OnceLock::new();
        Self(Arc::new(Wired { routed, reads, tree_states: Arc::default(), silent_timeout }))
    }

    pub(crate) fn routing(&self) -> Routing<S, V> {
        Routing(Arc::clone(&self.0))
    }
}

/// Sets a server's [`RouteDeps`] once they exist (a server binds + answers health before its owner
/// has a chain to route to)
pub struct Routing<S: ChainDataSource, V>(Arc<Wired<S, V>>);

impl<S: ChainDataSource, V> Clone for Routing<S, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: ChainDataSource, V> Routing<S, V> {
    /// Every method answered from `routes` from here on
    ///
    /// - once per server (twice = a bug, panics)
    pub fn route(&self, routes: RouteDeps<S, V>) {
        let disabled_by = ROUTES.map(|row| row.index.filter(|kind| !routes.indexes.contains(kind)));
        let routed = Routed { routes, disabled_by };
        assert!(self.0.routed.set(routed).is_ok(), "gRPC server routed twice");
    }
}

impl<S: ChainDataSource, V: PinnedView> Wired<S, V> {
    /// One request's response: every refusal folded into its status here, nowhere else
    async fn answer<B>(&self, method: Option<Method>, path: &str, body: B) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let answered = match (self.routed.get(), method) {
            (None, _) => Err(Status::unavailable(STARTING)),
            (Some(_), None) => Err(Status::unimplemented(format!("{path}: no such method"))),
            (Some(routed), Some(method)) => match routed.disabled_by[method as usize] {
                Some(index) => Err(disabled(method, index)),
                None => routed.handle(method, self, body).await,
            },
        };
        answered.unwrap_or_else(Status::into_http)
    }
}

impl<S: ChainDataSource, V: PinnedView> Routed<S, V> {
    /// The registry's handler column: one arm per [`Method`], off one `GrpcSnapshot`
    async fn handle<B>(
        &self,
        method: Method,
        wired: &Wired<S, V>,
        body: B,
    ) -> Result<Response<Body>, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        use Method::*;

        let (routes, reads) = (&self.routes, wired.reads.clone());
        let grpc_snapshot = routes.grpc_snapshots.load();
        let served = || grpc_snapshot.served().map_err(wire::unavailable);
        let compact_blocks = || {
            let served = served()?;
            let reader = view(served.views().compact_block(), method, IndexKind::CompactBlock)?;
            Ok::<_, Status>(blocks::CompactBlocks { served, reader })
        };
        let tree_states = || {
            // the NFS publish itself: memo identity (`served()` = its served tip)
            let nothing = || wire::unavailable(Unavailable::NothingServed);
            let indexed = grpc_snapshot.indexed().ok_or_else(nothing)?;
            let reader = view(indexed.served().views().tree_state(), method, IndexKind::TreeState)?;
            let (indexed, memos) = (Arc::clone(indexed), Arc::clone(&wired.tree_states));
            let reads = reads.clone();
            Ok::<_, Status>(tree_state::TreeStateReads { indexed, reader, reads, memos })
        };
        let address_index = || {
            let served = served()?;
            let index = served.views().transparent_address();
            let reader = view(index, method, IndexKind::TransparentAddress)?;
            let reader = reader.as_of(served.tip().height).with_max_rows(routes.max_address_rows);
            let network = served.params().network;
            Ok::<_, Status>(transparent_address::AddressIndex { reader, network })
        };

        match method {
            GetLatestBlock => Ok(blocks::get_latest_block(served()?)),
            GetBlock => blocks::get_block(compact_blocks()?, body, &reads).await,
            GetBlockRange => blocks::get_block_range(compact_blocks()?, body, reads).await,
            GetBlockRangeNullifiers => {
                deprecated_nullifiers::get_block_range_nullifiers(compact_blocks()?, body, reads)
                    .await
            }
            GetTreeState => tree_state::get_tree_state(&tree_states()?, body).await,
            GetLatestTreeState => tree_state::get_latest_tree_state(&tree_states()?).await,
            GetSubtreeRoots => tree_state::get_subtree_roots(&tree_states()?, body).await,
            GetAddressUtxos => {
                transparent_address::get_address_utxos(address_index()?, body, &reads).await
            }
            GetAddressUtxosStream => {
                transparent_address::get_address_utxos_stream(address_index()?, body, &reads).await
            }
            GetTaddressBalance => {
                transparent_address::get_taddress_balance(address_index()?, body, &reads).await
            }
            GetTaddressBalanceStream => {
                transparent_address::get_taddress_balance_stream(address_index()?, body, &reads)
                    .await
            }
            GetTaddressTransactions | GetTaddressTxids => {
                transparent_address::get_taddress_transactions(
                    address_index()?,
                    routes.validators.clone(),
                    Arc::clone(&grpc_snapshot),
                    body,
                    &reads,
                )
                .await
            }
            SendTransaction => transactions::send_transaction(&routes.chain_view, body).await,
            GetMempoolTx => mempool::get_mempool_tx(&grpc_snapshot, body).await,
            GetMempoolStream => mempool::get_mempool_stream(&grpc_snapshot),
            GetTransaction => {
                transactions::get_transaction(&routes.validators, &grpc_snapshot, body).await
            }
            GetLightdInfo => info::get_lightd_info(&grpc_snapshot, routes.network, routes.version),
            GetStandardFee => info::get_standard_fee(&grpc_snapshot, routes.network),
        }
    }
}

/// A configured index's view, absent = disabled (an NFS publish built without it)
fn view<T>(view: Option<T>, method: Method, index: IndexKind) -> Result<T, Status> {
    view.ok_or_else(|| disabled(method, index))
}

/// `method` needs `index`, which the operator left off
fn disabled(method: Method, index: IndexKind) -> Status {
    let (method, index) = (method.route().name, index.name());
    Status::unimplemented(format!("{method} needs the {index} index, which is not enabled"))
}

/// Never fails: every refusal is a gRPC status in the response
impl<S, V, ReqBody> tower::Service<Request<ReqBody>> for Dispatch<S, V>
where
    S: ChainDataSource,
    V: PinnedView,
    ReqBody: http_body::Body + Send + 'static,
    ReqBody::Data: Send,
    ReqBody::Error: std::fmt::Display,
{
    type Response = Response<Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let wired = Arc::clone(&self.0);
        let path = request.uri().path().to_owned();
        let method = Method::of_request(&request);
        let bounded = method.is_none_or(|method| !method.route().is_subscription());
        let bounds = Bounds::of(request.headers(), bounded.then_some(wired.silent_timeout));
        Box::pin(
            async move { Ok(bounds.apply(wired.answer(method, &path, request.into_body())).await) },
        )
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use tonic::Status;
    use zaino_persistence::IndexKind;
    use zaino_primitives::testing::{h, MockChain};

    use super::{Method, Reply, RouteDeps, ROUTES};
    use crate::testing::{
        decode_frames, dispatch, drained, framed_request, grpc_snapshots, grpc_snapshots_held,
        indexed, request_at, routes, MAINNET,
    };

    /// - each row's path names its own method
    /// - any other path (another service, a typo) names none
    #[test]
    fn a_path_names_its_row_or_no_method() {
        let resolved: Vec<_> = ROUTES.iter().map(|row| Method::of(&row.method.path())).collect();
        assert_eq!(resolved, ROUTES.map(|row| Some(row.method)));
        let service = "/cash.z.wallet.sdk.rpc.CompactTxStreamer";
        let others = ["/grpc.health.v1.Health/GetBlock", "/", "", &format!("{service}/GetBlocks")];
        assert_eq!(others.map(Method::of), [None; 4]);
    }

    /// Each row's `Reply` = its proto response side, so the latency metric a row feeds can't drift
    /// from the wire (`returns (stream …)` = `Stream`)
    #[test]
    fn every_row_replies_as_the_proto_declares() {
        let proto = include_str!("../../zaino-proto/lightwallet-protocol/walletrpc/service.proto");
        let declared: std::collections::HashMap<&str, Reply> = proto
            .lines()
            .filter_map(|line| {
                let (name, rest) = line.trim().strip_prefix("rpc ")?.split_once('(')?;
                let returns = rest.split_once("returns")?.1.trim_start();
                let reply =
                    if returns.starts_with("(stream ") { Reply::Stream } else { Reply::One };
                Some((name.trim(), reply))
            })
            .collect();
        let drifted: Vec<_> = ROUTES
            .iter()
            .filter(|row| declared.get(row.name) != Some(&row.reply))
            .map(|row| (row.name, row.reply, declared.get(row.name)))
            .collect();
        assert!(drifted.is_empty(), "rows vs service.proto (name, ours, proto's): {drifted:?}");
    }

    /// Routed with only tree_state configured, nothing served yet:
    ///
    /// - enabled method = `UNAVAILABLE` (retry)
    /// - disabled method = `UNIMPLEMENTED` naming its index, served or not
    /// - a method reading no index (`GetLatestBlock`, `GetLightdInfo`) = never disabled
    /// - an unknown path = `UNIMPLEMENTED`
    #[tokio::test]
    async fn config_enables_each_method_and_an_unknown_path_is_unimplemented() {
        use tonic::Code::{Unavailable, Unimplemented};
        use tower::Service as _;
        use Method::*;

        let mut router = dispatch(RouteDeps { indexes: vec![IndexKind::TreeState], ..routes() });
        let syncing = || (Unavailable, "the indexes are syncing: nothing served yet".to_owned());
        let off = |method: Method, index: &str| {
            let method = method.route().name;
            (Unimplemented, format!("{method} needs the {index} index, which is not enabled"))
        };
        let unknown = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/Ping";
        let health = "/grpc.health.v1.Health/Check";
        let cases = [
            (GetLatestBlock.path(), syncing()),
            (GetLightdInfo.path(), syncing()),
            (GetTreeState.path(), syncing()),
            (GetBlockRange.path(), off(GetBlockRange, "compact_block")),
            (GetBlock.path(), off(GetBlock, "compact_block")),
            (GetAddressUtxos.path(), off(GetAddressUtxos, "transparent_address")),
            (GetTaddressTxids.path(), off(GetTaddressTxids, "transparent_address")),
            (unknown.to_owned(), (Unimplemented, format!("{unknown}: no such method"))),
            (health.to_owned(), (Unimplemented, format!("{health}: no such method"))),
        ];

        let mut answered = Vec::new();
        for (path, _) in &cases {
            let response = router.call(request_at(path, bytes::Bytes::new())).await;
            let status = Status::from_header_map(response.expect("infallible").headers());
            let status = status.expect("a status");
            answered.push((status.code(), status.message().to_owned()));
        }
        assert_eq!(answered, cases.map(|(_, expected)| expected));
    }

    /// G1, zingolib #2835: no height named = served tip, height named = that index's durable
    ///
    /// - Views hold 0..=3, served tip 2 (root publication in bulk sync: views ahead of its tip)
    /// - validator 0 holds 2, one tx of ours relayed
    /// - `GetLatestBlock`, `GetLatestTreeState`, `GetLightdInfo.blockHeight` = 2
    /// - `GetBlockRange` 0..=9 = 0..=3 then `OUT_OF_RANGE`
    /// - `GetTreeState` / `GetBlock` 3 answered (block hash = its tree state's), 4 = a miss
    /// - mempool: `GetMempoolTx` gated open, `GetMempoolStream` opens on our relay, then ends (a
    ///   fixed `GrpcSnapshot` never moves on)
    #[tokio::test]
    async fn latest_rpcs_share_the_served_tip_and_each_index_answers_its_own_durable() {
        use http::HeaderMap;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_chainview::TrustedValidatorSet;
        use zaino_proto::frame::FRAME_HEADER;
        use zaino_proto::proto::compact_formats;
        use zaino_proto::proto::service;
        use Method::*;

        let mut chain = MockChain::regtest().network(MAINNET);
        let tip = chain.mine_empty(3);
        let views = vec![
            indexed(IndexKind::CompactBlock, &chain, tip),
            indexed(IndexKind::TreeState, &chain, tip),
        ];
        let raw = zaino_source::testing::fixtures::transactions(2_000_000).remove(0);
        let txid = zaino_source::decode_transaction(&raw).expect("a mainnet tx").txid;
        let ours = [(txid, bytes::Bytes::from(raw.clone()))];
        let two = chain.at(h(2));
        let grpc_snapshots =
            grpc_snapshots_held(&chain, two, views, TrustedValidatorSet::at([0]), &ours);
        let mut router = dispatch(RouteDeps { grpc_snapshots, ..routes() });
        let mut call = |method: Method, message: Vec<u8>| {
            let response = router.call(framed_request(method, message.into()));
            async move {
                use http_body_util::BodyExt as _;
                let response = response.await.expect("router answers");
                // refusal = Trailers-Only (headers), answer = status in its trailers
                let headed = Status::from_header_map(response.headers()).map(|s| s.code());
                let body = response.into_body().collect().await.expect("body");
                let trailers = body.trailers().cloned().unwrap_or_else(HeaderMap::new);
                let code = headed.or_else(|| Status::from_header_map(&trailers).map(|s| s.code()));
                (code, body.to_bytes(), trailers)
            }
        };
        let at = |height| service::BlockId { height, hash: Vec::new() };

        let (_, latest, _) = call(GetLatestBlock, Vec::new()).await;
        let latest = service::BlockId::decode(&latest[FRAME_HEADER..]).expect("a BlockID");
        let tip_hash = two.hash.0.to_vec();
        assert_eq!((latest.height, latest.hash), (2, tip_hash.clone()), "the served tip");

        let range =
            service::BlockRange { start: Some(at(0)), end: Some(at(9)), pool_types: vec![] };
        let (_, streamed, trailers) = call(GetBlockRange, range.encode_to_vec()).await;
        let past = HeaderValue::from_static("11");
        assert_eq!(trailers.get("grpc-status"), Some(&past), "to the index tip, then OUT_OF_RANGE");
        let served: Vec<_> = decode_frames::<compact_formats::CompactBlock>(&streamed)
            .into_iter()
            .map(|block| (block.height, block.hash))
            .collect();
        let expected: Vec<_> =
            (0..=3u32).map(|at| (u64::from(at), chain.at(h(at)).hash.0.to_vec())).collect();
        assert_eq!(
            served, expected,
            "through the compact-block index's durable, not the served tip"
        );

        let (_, state, _) = call(GetLatestTreeState, Vec::new()).await;
        let state = service::TreeState::decode(&state[FRAME_HEADER..]).expect("a TreeState");
        let mut display = tip_hash;
        display.reverse();
        assert_eq!(
            (state.height, state.hash),
            (2, const_hex::encode(display)),
            "same tip, same block"
        );
        let three = chain.at(h(3)).hash;
        let mut three_display = three.0.to_vec();
        three_display.reverse();
        let (code, state, _) = call(GetTreeState, at(3).encode_to_vec()).await;
        let state = service::TreeState::decode(&state[FRAME_HEADER..]).expect("3 answered");
        let answered = (code, state.height, state.hash);
        let ok = Some(tonic::Code::Ok);
        assert_eq!(answered, (ok, 3, const_hex::encode(three_display)), "3 durable, past the tip");
        let (code, block, _) = call(GetBlock, at(3).encode_to_vec()).await;
        let block =
            compact_formats::CompactBlock::decode(&block[FRAME_HEADER..]).expect("3 answered");
        let three = three.0.to_vec();
        assert_eq!((code, block.height, block.hash), (ok, 3, three), "GetBlock agrees");
        for method in [GetTreeState, GetBlock] {
            let (code, _, _) = call(method, at(4).encode_to_vec()).await;
            assert_eq!(code, Some(tonic::Code::NotFound), "{method:?}: 4 past every index");
        }

        let (_, info, _) = call(GetLightdInfo, Vec::new()).await;
        let info = service::LightdInfo::decode(&info[FRAME_HEADER..]).expect("a LightdInfo");
        let lightd = (info.block_height, info.estimated_height, info.consensus_branch_id.as_str());
        assert_eq!(lightd, (2, 2, "37a5165b"), "served height, best, the schedule's branch at 2");

        let every = vec![1, 2, 3, 4];
        let request = service::GetMempoolTxRequest { pool_types: every, ..Default::default() };
        let (_, listed, trailers) = call(GetMempoolTx, request.encode_to_vec()).await;
        let listed: Vec<_> = decode_frames::<compact_formats::CompactTx>(&listed)
            .into_iter()
            .map(|tx| tx.txid)
            .collect();
        let ours = txid.as_ref().to_vec();
        assert_eq!(listed, [ours], "held: our relay, compacted");
        assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let (_, streamed, trailers) = call(GetMempoolStream, Vec::new()).await;
        let opened = decode_frames::<service::RawTransaction>(&streamed);
        let unmined = service::RawTransaction { data: raw.into(), height: 0 };
        assert_eq!(opened, [unmined], "our relay, unmined, then the end");
        assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
    }

    /// Every index method on one router, each index enabled:
    ///
    /// - each answers `OK` off its own index
    /// - `GetSubtreeRoots` past the end = an empty stream (pepper-sync's probe), never a status
    #[tokio::test]
    async fn every_index_answers_its_own_methods() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service;
        use Method::*;

        let mut chain = MockChain::regtest().network(MAINNET);
        let tip = chain.mine_empty(1);
        let kinds = [IndexKind::CompactBlock, IndexKind::TreeState, IndexKind::TransparentAddress];
        let views = kinds.into_iter().map(|kind| indexed(kind, &chain, tip)).collect();
        let mut router =
            dispatch(RouteDeps { grpc_snapshots: grpc_snapshots(&chain, tip, views), ..routes() });

        let address = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs".to_owned();
        let range = service::BlockRange {
            start: Some(service::BlockId { height: 0, hash: Vec::new() }),
            end: Some(service::BlockId { height: 1, hash: Vec::new() }),
            pool_types: Vec::new(),
        };
        let utxos = service::GetAddressUtxosArg {
            addresses: vec![address.clone()],
            start_height: 0,
            max_entries: 0,
        };
        let every = [
            (GetLatestBlock, service::ChainSpec::default().encode_to_vec()),
            (GetBlock, service::BlockId { height: 0, hash: Vec::new() }.encode_to_vec()),
            (GetBlockRange, range.encode_to_vec()),
            (GetBlockRangeNullifiers, range.encode_to_vec()),
            // 1 = Sapling's activation (below it: no tree state)
            (GetTreeState, service::BlockId { height: 1, hash: Vec::new() }.encode_to_vec()),
            (GetLatestTreeState, Vec::new()),
            (GetAddressUtxos, utxos.encode_to_vec()),
            (
                GetTaddressBalance,
                service::AddressList { addresses: vec![address.clone()] }.encode_to_vec(),
            ),
            (GetTaddressBalanceStream, service::Address { address }.encode_to_vec()),
        ];
        for (method, message) in every {
            let response =
                router.call(framed_request(method, message.into())).await.expect("router answers");
            let (_, trailers) = drained(response).await;
            let status = trailers.get("grpc-status");
            assert_eq!(status, Some(&HeaderValue::from_static("0")), "{method:?}");
        }

        let roots = service::GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: service::ShieldedProtocol::Orchard as i32,
            max_entries: 0,
        };
        let response = router
            .call(framed_request(GetSubtreeRoots, roots.encode_to_vec().into()))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), None, "status rides in the trailers");
        let (chunks, trailing) = drained(response).await;
        assert!(chunks.is_empty(), "no subtree is complete yet");
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
    }
}
