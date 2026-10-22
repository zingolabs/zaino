//! `CompactTxStreamer` service: the method registry ([`Method`]) and the one dispatch over it
//!
//! - By hand, not tonic's generated trait (decoded messages only: stored gRPC-framed records
//!   would be decoded to be re-encoded, usage.md "Stored bytes on the wire")
//! - one global [`GrpcSnapshot`](zaino_grpc_snapshot::GrpcSnapshot) per request or stream (G1),
//!   pinned for its life
//! - no height named = its served tip, height named = `IndexesAtBlock::answers_through`
//! - `Unavailable` (nothing served, no chain, no holder) = `UNAVAILABLE`, its message
//! - a method whose index `RouteDeps::indexes` lacks = `UNIMPLEMENTED`, naming the index
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

/// Every method's path prefix (`/{service}/{method}`)
const SERVICE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/";

/// One served method (variant name = wire name = `method` metric label)
///
/// - new RPC = a variant, then its [`Method::route`] arm + its `Wired::handle` arm
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
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

/// One method's policy: admission pool, reply shape (its latency metric), the index whose view
/// its handler reads (`None` = reads none: always served)
#[derive(Clone, Copy, Debug)]
pub(crate) struct Route {
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

impl Method {
    /// The registry's policy columns: one arm per method
    pub(crate) const fn route(self) -> Route {
        use Class::{Subscription, Work};
        use IndexKind::{CompactBlock, TransparentAddress, TreeState};
        use Method::*;
        use Reply::{One, Stream};

        let (class, reply, index) = match self {
            // served tip itself
            GetLatestBlock => (Work, One, None),
            GetBlock => (Work, One, Some(CompactBlock)),
            GetBlockRange => (Work, Stream, Some(CompactBlock)),
            // TODO: REMOVE — deprecated alias of `GetBlockRange` (pepper-sync still calls it)
            GetBlockRangeNullifiers => (Work, Stream, Some(CompactBlock)),
            GetTreeState => (Work, One, Some(TreeState)),
            GetLatestTreeState => (Work, One, Some(TreeState)),
            GetSubtreeRoots => (Work, Stream, Some(TreeState)),
            GetAddressUtxos => (Work, One, Some(TransparentAddress)),
            GetAddressUtxosStream => (Work, Stream, Some(TransparentAddress)),
            GetTaddressBalance => (Work, One, Some(TransparentAddress)),
            GetTaddressBalanceStream => (Work, One, Some(TransparentAddress)),
            // index names the txs, a validator holds the bytes (`docs/architecture.md` §1)
            GetTaddressTransactions => (Work, Stream, Some(TransparentAddress)),
            // TODO: REMOVE — deprecated alias of `GetTaddressTransactions` (pepper-sync calls it)
            GetTaddressTxids => (Work, Stream, Some(TransparentAddress)),
            // one random entry per attempt, watched until it spreads (`docs/architecture.md` §6)
            SendTransaction => (Work, One, None),
            GetMempoolTx => (Work, Stream, None),
            // open until the served tip moves, silent between arrivals
            GetMempoolStream => (Subscription, Stream, None),
            GetTransaction => (Work, One, None),
            GetLightdInfo => (Work, One, None),
            GetStandardFee => (Work, One, None),
        };
        Route { class, reply, index }
    }

    /// Wire name (= the `method` metric label)
    pub(crate) fn name(self) -> &'static str {
        self.into()
    }

    /// `/{service}/{method}` → its method (`None` = no such variant)
    fn of(path: &str) -> Option<Self> {
        path.strip_prefix(SERVICE)?.parse().ok()
    }

    /// Once per request, by the outermost layer (`HealthRoute`): every inner layer reads it back
    pub(crate) fn resolve<B>(request: &mut Request<B>) {
        if let Some(method) = Self::of(request.uri().path()) {
            request.extensions_mut().insert(method);
        }
    }

    /// What [`Self::resolve`] named (`None` = no such method)
    pub(crate) fn of_request<B>(request: &Request<B>) -> Option<Self> {
        request.extensions().get::<Self>().copied()
    }

    #[cfg(test)]
    pub(crate) fn path(self) -> String {
        format!("{SERVICE}{}", self.name())
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
/// - `indexes` = the configured ones: a method needing another = `UNIMPLEMENTED`
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
/// - `routes` set once, after bind (empty = every method `UNAVAILABLE` [`STARTING`])
/// - `reads` process-wide (every index read on the blocking pool under its lane's permit)
struct Wired<S: ChainDataSource, V> {
    routes: OnceLock<RouteDeps<S, V>>,
    reads: ReadLanes,
    tree_states: Arc<tree_state::Memos<V>>,
    silent_timeout: Duration,
}

/// Every `CompactTxStreamer` method, dispatched by path (the tower service each connection runs;
/// [`Routing`] = its owner's handle on the same `Arc`)
pub(crate) struct Dispatch<S: ChainDataSource, V>(Arc<Wired<S, V>>);

impl<S: ChainDataSource, V> Clone for Dispatch<S, V> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<S: ChainDataSource, V: PinnedView> Dispatch<S, V> {
    pub(crate) fn new(limits: &GrpcLimits) -> Self {
        let (reads, silent_timeout) = (ReadLanes::new(limits), limits.silent_timeout);
        let routes = OnceLock::new();
        Self(Arc::new(Wired { routes, reads, tree_states: Arc::default(), silent_timeout }))
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
        assert!(self.0.routes.set(routes).is_ok(), "gRPC server routed twice");
    }
}

impl<S: ChainDataSource, V: PinnedView> Wired<S, V> {
    /// One request's response: every refusal folded into its status here, nowhere else
    async fn answer<B>(&self, method: Option<Method>, path: &str, body: B) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let answered = match (self.routes.get(), method) {
            (None, _) => Err(Status::unavailable(STARTING)),
            (Some(_), None) => Err(Status::unimplemented(format!("{path}: no such method"))),
            (Some(routes), Some(method)) => match method.route().index {
                Some(index) if !routes.indexes.contains(&index) => Err(disabled(method, index)),
                _ => self.handle(routes, method, body).await,
            },
        };
        answered.unwrap_or_else(Status::into_http)
    }

    /// The registry's handler column: one arm per [`Method`], off one `GrpcSnapshot`
    ///
    /// - body collected once (its method's cap), decoded here to the handler's message
    async fn handle<B>(
        &self,
        routes: &RouteDeps<S, V>,
        method: Method,
        body: B,
    ) -> Result<Response<Body>, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        use wire::{decode, decode_each};
        use Method::*;

        let request = wire::collect_request(body, method).await?;
        let reads = self.reads.clone();
        let grpc_snapshot = routes.grpc_snapshots.load();
        let served = || grpc_snapshot.served().map_err(wire::unavailable);
        let compact_blocks = || {
            let served = served()?;
            let reader = view(served.views().compact_block(), method)?;
            Ok::<_, Status>(blocks::CompactBlocks { served, reader })
        };
        let tree_states = || {
            // the NFS publish itself: memo identity (`served()` = its served tip)
            let nothing = || wire::unavailable(Unavailable::NothingServed);
            let indexed = grpc_snapshot.indexed().ok_or_else(nothing)?;
            let reader = view(indexed.served().views().tree_state(), method)?;
            let (indexed, memos) = (Arc::clone(indexed), Arc::clone(&self.tree_states));
            let reads = reads.clone();
            Ok::<_, Status>(tree_state::TreeStateReads { indexed, reader, reads, memos })
        };
        let address_index = || {
            let served = served()?;
            let reader = view(served.views().transparent_address(), method)?;
            let reader = reader.as_of(served.tip().height).with_max_rows(routes.max_address_rows);
            let network = served.params().network;
            Ok::<_, Status>(transparent_address::AddressIndex { reader, network })
        };

        match method {
            GetLatestBlock => Ok(blocks::get_latest_block(served()?)),
            GetBlock => blocks::get_block(compact_blocks()?, decode(&request)?, &reads).await,
            GetBlockRange => blocks::get_block_range(compact_blocks()?, decode(&request)?, reads),
            GetBlockRangeNullifiers => deprecated_nullifiers::get_block_range_nullifiers(
                compact_blocks()?,
                decode(&request)?,
                reads,
            ),
            GetTreeState => tree_state::get_tree_state(&tree_states()?, decode(&request)?).await,
            GetLatestTreeState => tree_state::get_latest_tree_state(&tree_states()?).await,
            GetSubtreeRoots => {
                tree_state::get_subtree_roots(&tree_states()?, decode(&request)?).await
            }
            GetAddressUtxos => {
                let index = address_index()?;
                transparent_address::get_address_utxos(index, decode(&request)?, &reads).await
            }
            GetAddressUtxosStream => {
                let index = address_index()?;
                transparent_address::get_address_utxos_stream(index, decode(&request)?, &reads)
                    .await
            }
            GetTaddressBalance => {
                let index = address_index()?;
                transparent_address::get_taddress_balance(index, decode(&request)?, &reads).await
            }
            GetTaddressBalanceStream => {
                let (index, request) = (address_index()?, decode_each(&request)?);
                transparent_address::get_taddress_balance_stream(index, request, &reads).await
            }
            GetTaddressTransactions | GetTaddressTxids => {
                transparent_address::get_taddress_transactions(
                    address_index()?,
                    routes.validators.clone(),
                    Arc::clone(&grpc_snapshot),
                    decode(&request)?,
                    &reads,
                )
                .await
            }
            SendTransaction => {
                transactions::send_transaction(&routes.chain_view, decode(&request)?).await
            }
            GetMempoolTx => mempool::get_mempool_tx(&grpc_snapshot, decode(&request)?),
            GetMempoolStream => mempool::get_mempool_stream(&grpc_snapshot),
            GetTransaction => {
                let validators = &routes.validators;
                transactions::get_transaction(validators, &grpc_snapshot, decode(&request)?).await
            }
            GetLightdInfo => info::get_lightd_info(&grpc_snapshot, routes.network, routes.version),
            GetStandardFee => info::get_standard_fee(&grpc_snapshot, routes.network),
        }
    }
}

/// `method`'s index view ([`Method::route`]), absent = disabled (an NFS publish built without it)
fn view<T>(view: Option<T>, method: Method) -> Result<T, Status> {
    let index = method.route().index.expect("a method reading a view names its index");
    view.ok_or_else(|| disabled(method, index))
}

/// `method` needs `index`, which the operator left off
fn disabled(method: Method, index: IndexKind) -> Status {
    let (method, index) = (method.name(), index.name());
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
    use tonic::Code;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_persistence::IndexKind;
    use zaino_source::testing::{h, MockChain};

    use strum::VariantArray as _;

    use super::{Method, Reply};
    use crate::testing::{
        answer, decode_frames, dispatch, fixed_snapshots, framed_request, indexed, request_at,
        routes_over, routes_serving, MAINNET,
    };

    /// Any other path (another service, a typo) names none
    #[test]
    fn a_path_names_its_method_or_none() {
        for method in Method::VARIANTS {
            assert_eq!(Method::of(&method.path()), Some(*method), "{}", method.path());
        }
        let service = "/cash.z.wallet.sdk.rpc.CompactTxStreamer";
        let typo = format!("{service}/GetBlocks");
        for other in ["/grpc.health.v1.Health/GetBlock", "/", "", &typo] {
            assert_eq!(Method::of(other), None, "{other:?}");
        }
    }

    /// Each method's `Reply` = its proto response side, so the latency metric it feeds can't drift
    /// from the wire (`returns (stream …)` = `Stream`)
    #[test]
    fn every_method_replies_as_the_proto_declares() {
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
        let drifted: Vec<_> = Method::VARIANTS
            .iter()
            .map(|method| (method.name(), method.route().reply))
            .filter(|(name, reply)| declared.get(name) != Some(reply))
            .map(|(name, reply)| (name, reply, declared.get(name)))
            .collect();
        assert!(drifted.is_empty(), "methods vs service.proto (name, ours, proto's): {drifted:?}");
    }

    /// Routed with only tree_state configured, nothing served yet:
    ///
    /// - enabled method = `UNAVAILABLE` (retry)
    /// - disabled method = `UNIMPLEMENTED` naming its index, served or not
    /// - a method reading no index (`GetLatestBlock`, `GetLightdInfo`) = never disabled
    /// - an unknown path = `UNIMPLEMENTED`
    #[tokio::test]
    async fn config_enables_each_method_and_an_unknown_path_is_unimplemented() {
        use tower::Service as _;
        use Code::{Unavailable, Unimplemented};
        use Method::*;

        let chain = MockChain::regtest();
        let node = chain.validator(chain.genesis());
        let (routes, _unspawned, _) = routes_over(&node, MAINNET, &[IndexKind::TreeState]);
        let mut router = dispatch(routes);

        let syncing = || "the indexes are syncing: nothing served yet".to_owned();
        let off = |method: Method, index: &str| {
            format!("{} needs the {index} index, which is not enabled", method.name())
        };
        let unknown = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/Ping";
        let health = "/grpc.health.v1.Health/Check";
        #[rustfmt::skip]
        let cases = [
            (GetLatestBlock.path(),   Unavailable,   syncing()),
            (GetLightdInfo.path(),    Unavailable,   syncing()),
            (GetTreeState.path(),     Unavailable,   syncing()),
            (GetBlockRange.path(),    Unimplemented, off(GetBlockRange, "compact_block")),
            (GetBlock.path(),         Unimplemented, off(GetBlock, "compact_block")),
            (GetAddressUtxos.path(),  Unimplemented, off(GetAddressUtxos, "transparent_address")),
            (GetTaddressTxids.path(), Unimplemented, off(GetTaddressTxids, "transparent_address")),
            (unknown.to_owned(),      Unimplemented, format!("{unknown}: no such method")),
            (health.to_owned(),       Unimplemented, format!("{health}: no such method")),
        ];
        for (path, code, message) in cases {
            let response = router.call(request_at(&path, bytes::Bytes::new())).await;
            let (_, status) = answer(response.expect("infallible")).await;
            assert_eq!((status.code(), status.message()), (code, message.as_str()), "{path}");
        }
    }

    /// G1, zingolib #2835: views durable through 3, served tip 2 (bulk sync: views run ahead of
    /// the root publication's tip)
    #[tokio::test]
    async fn latest_rpcs_answer_the_served_tip_and_a_named_height_each_indexs_durable() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats::CompactBlock;
        use zaino_proto::proto::service::{BlockId, BlockRange, LightdInfo, TreeState};
        use Method::*;

        let mut chain = MockChain::regtest().network(MAINNET);
        let durable = chain.mine_empty(3);
        let served = chain.at(h(2));
        let indexes = [IndexKind::CompactBlock, IndexKind::TreeState];
        let views = indexes.map(|kind| indexed(kind, &chain, durable)).to_vec();
        let snapshots = fixed_snapshots(&chain, served, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &indexes, snapshots));
        let mut call = async |method: Method, message: Vec<u8>| {
            let response = router.call(framed_request(method, message.into())).await;
            let (chunks, status) = answer(response.expect("infallible")).await;
            (chunks.concat(), status)
        };
        let at = |height| BlockId { height, hash: Vec::new() };
        fn one<M: prost::Message + Default>(body: &[u8]) -> Option<M> {
            decode_frames(body).into_iter().next()
        }

        let (latest, _) = call(GetLatestBlock, Vec::new()).await;
        let expected = BlockId { height: 2, hash: served.hash.0.to_vec() };
        assert_eq!(one::<BlockId>(&latest), Some(expected), "GetLatestBlock: the served tip");
        let (state, _) = call(GetLatestTreeState, Vec::new()).await;
        let height = one::<TreeState>(&state).map(|state| state.height);
        assert_eq!(height, Some(2), "GetLatestTreeState: the served tip");
        let (info, _) = call(GetLightdInfo, Vec::new()).await;
        let height = one::<LightdInfo>(&info).map(|info| info.block_height);
        assert_eq!(height, Some(2), "GetLightdInfo.blockHeight: the served tip");

        let range = BlockRange { start: Some(at(0)), end: Some(at(9)), pool_types: Vec::new() };
        let (blocks, status) = call(GetBlockRange, range.encode_to_vec()).await;
        let heights: Vec<_> =
            decode_frames::<CompactBlock>(&blocks).iter().map(|b| b.height).collect();
        assert_eq!(heights, [0, 1, 2, 3], "GetBlockRange 0..=9: to the durable 3 ({status:?})");
        assert_eq!(status.code(), Code::OutOfRange, "GetBlockRange 0..=9 past the durable 3");

        let (block, status) = call(GetBlock, at(3).encode_to_vec()).await;
        let height = one::<CompactBlock>(&block).map(|block| block.height);
        assert_eq!(height, Some(3), "GetBlock 3: durable past the served tip ({status:?})");
        let (state, status) = call(GetTreeState, at(3).encode_to_vec()).await;
        let height = one::<TreeState>(&state).map(|state| state.height);
        assert_eq!(height, Some(3), "GetTreeState 3: durable past the served tip ({status:?})");
        for method in [GetTreeState, GetBlock] {
            let (_, status) = call(method, at(4).encode_to_vec()).await;
            assert_eq!(status.code(), Code::NotFound, "{method:?} 4, past every index: {status:?}");
        }
    }
}
