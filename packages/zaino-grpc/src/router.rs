//! The root router: one gRPC service, methods claimed by whichever indexes are enabled.
//!
//! `CompactTxStreamer` is a single, fixed service name, so its methods cannot be split across
//! two tonic services — the second would never be routed to. The router is therefore the named
//! service, and it dispatches by method path: a path an enabled index claims is answered here,
//! and everything else falls through to the generated server unchanged.
//!
//! Why intercept at all, rather than implement the generated trait: that trait's methods take
//! and return decoded messages, so a compact-block record would be decoded only for tonic to
//! encode it again. Those records are *already* gRPC-framed (`[0x00][len][message]`), which is
//! exactly the shape of an HTTP/2 response body — a unary response is one record and a
//! server-streaming response is the records concatenated, so the router writes the bytes out as
//! the body and never constructs a `CompactBlock`. The tree-state and transparent-address
//! indexes answer with domain values instead, so their dispatch builds the proto message and
//! frames it here.
//!
//! Disabling an index is a config gate here and a feature gate in `Cargo.toml`: its routes stop
//! being claimed, and its crate stops being compiled in.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use http::{HeaderMap, HeaderValue, Request, Response};
use http_body_util::Full;
use tonic::{body::Body, server::NamedService, Status};
use zaino_index_compact_block::Pools;
use zaino_primitives::types::Height;

use crate::ReadLanes;

/// gRPC method paths this router can claim, as `/{service}/{method}`.
///
/// Grouped and gated by claiming index (index compiled out → path dead, not merely unwired).
pub(crate) mod path {
    pub const GET_LATEST_BLOCK: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock";
    pub const GET_BLOCK: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlock";
    pub const GET_BLOCK_RANGE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange";
    /// TODO: REMOVE THIS — deprecated alias of `GET_BLOCK_RANGE` (pepper-sync still calls it)
    pub const GET_BLOCK_RANGE_NULLIFIERS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRangeNullifiers";

    pub const GET_TREE_STATE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTreeState";
    pub const GET_LATEST_TREE_STATE: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestTreeState";
    pub const GET_SUBTREE_ROOTS: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetSubtreeRoots";

    pub const GET_ADDRESS_UTXOS: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetAddressUtxos";
    pub const GET_ADDRESS_UTXOS_STREAM: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetAddressUtxosStream";
    pub const GET_TADDRESS_BALANCE: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressBalance";
    pub const GET_TADDRESS_BALANCE_STREAM: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressBalanceStream";

    /// Needs both halves of the boundary: the index names the transactions, the validator holds
    /// the bytes. See `docs/design/boundaries.md`.
    pub const GET_TADDRESS_TRANSACTIONS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressTransactions";
    /// TODO: REMOVE THIS — deprecated alias of `GET_TADDRESS_TRANSACTIONS` (pepper-sync still
    /// calls it)
    pub const GET_TADDRESS_TXIDS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressTxids";

    /// Relayed to **every** validator in the view, not one, and marked as ours — which is what
    /// lets a wallet see its own transaction before it reaches quorum.
    pub const SEND_TRANSACTION: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction";
    pub const GET_MEMPOOL_TX: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetMempoolTx";
    pub const GET_MEMPOOL_STREAM: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetMempoolStream";
}

/// gRPC framing: compression flag + big-endian length.
const FRAME_HEADER: usize = 5;

/// Client height → [`Height`] (past the protocol ceiling = the client's error, never a service's)
fn height(raw: u64, field: &str) -> Result<Height, Status> {
    u32::try_from(raw).ok().and_then(|raw| Height::try_from(raw).ok()).ok_or_else(|| {
        Status::invalid_argument(format!("{field} {raw} is above the protocol height ceiling"))
    })
}

/// `BlockID.hash` → `(height, hash)` via the block-hash index
///
/// - height only: the answering index confirms it holds `hash` there (independent publications)
fn locate(
    locator: Option<&zaino_internal_block_hash_to_height::BlockHashService>,
    raw: &[u8],
    method: &str,
) -> Result<(Height, [u8; 32]), Status> {
    use zaino_internal_block_hash_to_height::ServeError;

    let hash: [u8; 32] =
        raw.try_into().map_err(|_| Status::invalid_argument("block hash must be 32 bytes"))?;
    let locator = locator.ok_or_else(|| {
        Status::unimplemented(format!(
            "{method} by hash resolves through the block-hash index, which is off"
        ))
    })?;
    match locator.locate(&hash) {
        Ok(height) => Ok((height, hash)),
        // `Unavailable`: clears on its own (retry with backoff)
        Err(error @ ServeError::Syncing) => Err(Status::unavailable(error.to_string())),
        Err(error @ ServeError::HashNotFound) => Err(Status::not_found(error.to_string())),
    }
}

/// Client `poolTypes` → [`Pools`] (empty = shielded default; unknown or `POOL_TYPE_INVALID` refused)
fn pools(raw: &[i32]) -> Result<Pools, Status> {
    use zaino_proto::proto::service::PoolType;

    if raw.is_empty() {
        return Ok(Pools::default());
    }

    let mut pools = Pools { sapling: false, orchard: false, ironwood: false, transparent: false };
    for &value in raw {
        match PoolType::try_from(value) {
            Ok(PoolType::Transparent) => pools.transparent = true,
            Ok(PoolType::Sapling) => pools.sapling = true,
            Ok(PoolType::Orchard) => pools.orchard = true,
            Ok(PoolType::Ironwood) => pools.ironwood = true,
            Ok(PoolType::Invalid) | Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "poolTypes value {value} is not a pool \
                     (TRANSPARENT=1, SAPLING=2, ORCHARD=3, IRONWOOD=4)"
                )))
            }
        }
    }

    Ok(pools)
}

/// `from ..= to`, refused when reversed (services take ordered ranges)
fn ordered(from: Height, to: Height) -> Result<(Height, Height), Status> {
    match from <= to {
        true => Ok((from, to)),
        false => Err(Status::invalid_argument(format!("range start {from} is above end {to}"))),
    }
}

/// Encodes one message into its wire frame, `[0x00][len:be32][message]` (infallible: a `Vec`
/// grows, and every reply is far under 4 GiB)
fn frame<M: prost::Message>(message: &M) -> bytes::Bytes {
    frame_all(std::slice::from_ref(message))
}

/// `messages` framed back to back into one buffer (one allocation, one DATA chunk)
fn frame_all<M: prost::Message>(messages: &[M]) -> bytes::Bytes {
    let total = messages.iter().map(|message| FRAME_HEADER + message.encoded_len()).sum();
    let mut framed = Vec::with_capacity(total);
    for message in messages {
        let len = u32::try_from(message.encoded_len()).expect("reply < 4 GiB");
        framed.push(0);
        framed.extend_from_slice(&len.to_be_bytes());
        message.encode_raw(&mut framed);
    }
    bytes::Bytes::from(framed)
}

/// Request body caps (gRPC framing included); over one = `RESOURCE_EXHAUSTED`, as tonic's own
/// decode limit answers
///
/// - claimed paths collect their own bodies, so tonic's 4 MiB default never applies to them
mod request_limit {
    /// Every claimed request but a transaction: ids, ranges, and address lists (~1.5k
    /// t-addresses; clients send tens)
    pub(super) const MESSAGE: usize = 64 * 1024;

    /// `SendTransaction`: a transaction fits in a block, plus framing and the height field
    pub(super) const TRANSACTION: usize = zaino_primitives::protocol::MAX_BLOCK_BYTES + 1024;

    /// A whole body arrives within this (a peer trickling one holds its stream permit)
    pub(super) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
}

/// A unary gRPC response whose body is one already-framed record.
///
/// `grpc-status` rides in the headers rather than the trailers. That is legal only because the
/// body is complete when the headers are written, and it keeps the whole answer to a single
/// write — which is the point of storing records in wire shape. A streaming body cannot do
/// this; see [`streamed_response`].
fn unary_response(record: bytes::Bytes) -> Response<Body> {
    let mut response = Response::new(Body::new(Full::new(record)));

    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
    response.headers_mut().insert("grpc-status", HeaderValue::from_static("0"));

    response
}

/// Final `grpc-status` trailers, as tonic's own encoder emits them.
///
/// A non-header-safe message is dropped rather than the whole trailer: `grpc-status` MUST be
/// present or the client reports a truncated stream.
fn trailers(status: &Status) -> HeaderMap {
    let mut map = HeaderMap::new();

    if status.add_header(&mut map).is_err() {
        map.clear();
        let _ = Status::new(status.code(), "").add_header(&mut map);
    }

    map
}

/// A server-streaming response over an already-materialised run of framed records.
///
/// `grpc-status` goes in the trailers, never the headers: a client reading one in the headers
/// treats the response as complete before the body arrives.
fn streamed_response(records: Vec<bytes::Bytes>) -> Response<Body> {
    use futures::StreamExt as _;
    use http_body::Frame;
    use http_body_util::StreamBody;

    let data = futures::stream::iter(records.into_iter().map(|record| Ok(Frame::data(record))));
    let trailing = futures::stream::once(async {
        Ok::<_, Status>(Frame::trailers(trailers(&Status::ok(""))))
    });

    let mut response = Response::new(Body::new(StreamBody::new(data.chain(trailing))));
    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

    response
}

/// The whole request body, refused past `limit` bytes (checked as it arrives, never buffered
/// beyond) or past [`request_limit::DEADLINE`]
async fn collect_limited<B>(body: B, limit: usize) -> Result<bytes::Bytes, Status>
where
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    use http_body_util::{BodyExt as _, LengthLimitError, Limited};

    let body = body.map_err(|error| Status::internal(format!("reading request body: {error}")));
    let collected =
        tokio::time::timeout(request_limit::DEADLINE, Limited::new(body, limit).collect())
            .await
            .map_err(|_| {
                Status::deadline_exceeded(format!(
                    "request body not complete within {:?}",
                    request_limit::DEADLINE
                ))
            })?;
    match collected {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.is::<LengthLimitError>() => Err(Status::resource_exhausted(format!(
            "request body is over this method's {limit}-byte limit"
        ))),
        Err(error) => match error.downcast::<Status>() {
            Ok(status) => Err(*status),
            Err(other) => Err(Status::internal(format!("reading request body: {other}"))),
        },
    }
}

/// Decodes a unary request message from a gRPC request body (≤ [`request_limit::MESSAGE`])
async fn decode_request<M, B>(body: B) -> Result<M, Status>
where
    M: prost::Message + Default,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    decode_request_within(body, request_limit::MESSAGE).await
}

/// Decodes a unary request message from a gRPC request body of at most `limit` bytes.
///
/// The body is one framed message; the frame header is stripped and the rest decoded.
async fn decode_request_within<M, B>(body: B, limit: usize) -> Result<M, Status>
where
    M: prost::Message + Default,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let collected = collect_limited(body, limit).await?;

    let message = collected
        .get(FRAME_HEADER..)
        .ok_or_else(|| Status::invalid_argument("request body is shorter than its frame"))?;

    M::decode(message).map_err(|error| Status::invalid_argument(error.to_string()))
}

/// Decodes every framed message in a client-streaming request body.
///
/// Collected rather than streamed: the one claimed client-streaming method
/// (`GetTaddressBalanceStream`) answers a single total, so nothing can be emitted before the
/// last address arrives anyway.
async fn decode_request_stream<M, B>(body: B) -> Result<Vec<M>, Status>
where
    M: prost::Message + Default,
    B: http_body::Body,
    B::Error: fmt::Display,
{
    let collected = collect_limited(body, request_limit::MESSAGE).await?;

    let mut messages = Vec::new();
    let mut at = 0usize;
    while at < collected.len() {
        let header: [u8; 4] = collected
            .get(at + 1..at + FRAME_HEADER)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| Status::invalid_argument("request body ends mid-frame"))?;
        let len = u32::from_be_bytes(header) as usize;

        let message = collected
            .get(at + FRAME_HEADER..at + FRAME_HEADER + len)
            .ok_or_else(|| Status::invalid_argument("frame is shorter than its length"))?;
        messages.push(M::decode(message).map_err(|e| Status::invalid_argument(e.to_string()))?);

        at += FRAME_HEADER + len;
    }

    Ok(messages)
}

fn status_response(status: Status) -> Response<Body> {
    status.into_http()
}

/// The root gRPC service.
///
/// `inner` is the generated `CompactTxStreamer` server, which answers everything the enabled
/// indexes do not claim.
#[derive(Clone)]
pub struct Router<Inner> {
    inner: Inner,
    compact_block: Option<zaino_index_compact_block::CompactBlockService>,
    /// Locator for every `BlockID.hash`; unwired = by-hash requests `Unimplemented`
    block_hash: Option<zaino_internal_block_hash_to_height::BlockHashService>,
    tree_state: Option<zaino_index_tree_state::TreeStateService>,
    transparent_address: Option<zaino_index_transparent_address::TransparentAddressService>,
    raw_transactions: std::sync::Arc<dyn crate::validator::FetchRawTransaction>,
    /// Process-wide: every index read runs on the blocking pool under a permit of its lane
    reads: ReadLanes,
    /// The multi-validator view: reads, the relay, and the renderer. Wired together or not.
    chainview: Option<ChainViewHandles>,
    /// Process-wide, like the views they render
    mempool_snapshots: std::sync::Arc<chainview::SnapshotFrames>,
    tree_states: std::sync::Arc<tree_state::Memos>,
}

/// What the chainview-backed methods need, kept together because none works without the rest.
#[derive(Clone)]
pub struct ChainViewHandles {
    pub view: zaino_chainview::ChainViewSubscriber,
    pub relay: std::sync::Arc<dyn crate::validator::Relay>,
    pub compact: std::sync::Arc<dyn crate::validator::ProjectCompact>,
}

/// The index a path belongs to, with the handle that answers it.
///
/// Resolved per request so every index claims its own paths independently — one that is
/// compiled in but unwired leaves its paths with `inner` rather than shadowing the rest.
enum Claimed {
    /// Block-hash index, when wired = the locator a `BlockID.hash` resolves through (same below)
    CompactBlock(
        zaino_index_compact_block::CompactBlockService,
        ReadLanes,
        Option<zaino_internal_block_hash_to_height::BlockHashService>,
    ),
    TreeState(
        zaino_index_tree_state::TreeStateService,
        ReadLanes,
        Option<zaino_internal_block_hash_to_height::BlockHashService>,
        std::sync::Arc<tree_state::Memos>,
    ),
    TransparentAddress(zaino_index_transparent_address::TransparentAddressService, ReadLanes),
    /// Reads plus the relay: `SendTransaction` fans out, the mempool methods read.
    ChainView(ChainViewHandles, std::sync::Arc<chainview::SnapshotFrames>),
    /// Both halves of the boundary: the index names the transactions, the validator holds them.
    TransparentTransactions(
        zaino_index_transparent_address::TransparentAddressService,
        ReadLanes,
        std::sync::Arc<dyn crate::validator::FetchRawTransaction>,
    ),
}

impl<Inner> Router<Inner> {
    /// A router that claims nothing: every method falls through to `inner`
    ///
    /// - `raw_transactions` = the bytes half of `GetTaddressTransactions` (the index names the
    ///   transactions, the validator holds them)
    pub fn new(
        inner: Inner,
        raw_transactions: std::sync::Arc<dyn crate::validator::FetchRawTransaction>,
        reads: ReadLanes,
    ) -> Self {
        Self {
            inner,
            compact_block: None,
            block_hash: None,
            tree_state: None,
            transparent_address: None,
            raw_transactions,
            reads,
            chainview: None,
            mempool_snapshots: std::sync::Arc::default(),
            tree_states: std::sync::Arc::default(),
        }
    }

    /// Claims the compact-block methods for `service`.
    ///
    /// Omitting this leaves those paths with `inner`, which is what an operator who disabled
    /// the index gets. Same for the two below.
    pub fn with_compact_block(
        mut self,
        service: zaino_index_compact_block::CompactBlockService,
    ) -> Self {
        self.compact_block = Some(service);
        self
    }

    /// Resolves `BlockID.hash` for `GetBlock` and `GetTreeState` (claims no path of its own)
    pub fn with_block_hash(
        mut self,
        service: zaino_internal_block_hash_to_height::BlockHashService,
    ) -> Self {
        self.block_hash = Some(service);
        self
    }

    /// Claims `GetTreeState`, `GetLatestTreeState` and `GetSubtreeRoots` for `service`.
    pub fn with_tree_state(mut self, service: zaino_index_tree_state::TreeStateService) -> Self {
        self.tree_state = Some(service);
        self
    }

    /// Claims the `GetAddressUtxos*` and `GetTaddressBalance*` methods for `service`.
    pub fn with_transparent_address(
        mut self,
        service: zaino_index_transparent_address::TransparentAddressService,
    ) -> Self {
        self.transparent_address = Some(service);
        self
    }

    /// Lets the multi-validator view answer `SendTransaction` and the mempool methods.
    ///
    /// All three together: reads from the subscriber, the relay fans a broadcast out, and the
    /// renderer turns raw bytes into compact ones. None of the methods works without the rest.
    pub fn with_chainview(mut self, handles: ChainViewHandles) -> Self {
        self.chainview = Some(handles);
        self
    }

    /// The wired index claiming `path`, if any.
    ///
    /// - path first, handle second (unclaimed path costs no service clone)
    /// - path matched but index unwired → falls out to `inner`, never to the next index
    fn claimed(&self, path: &str) -> Option<Claimed> {
        if matches!(
            path,
            path::GET_LATEST_BLOCK
                | path::GET_BLOCK
                | path::GET_BLOCK_RANGE
                | path::GET_BLOCK_RANGE_NULLIFIERS
        ) {
            return self.compact_block.clone().map(|service| {
                Claimed::CompactBlock(service, self.reads.clone(), self.block_hash.clone())
            });
        }

        if matches!(
            path,
            path::GET_TREE_STATE | path::GET_LATEST_TREE_STATE | path::GET_SUBTREE_ROOTS
        ) {
            return self.tree_state.clone().map(|service| {
                Claimed::TreeState(
                    service,
                    self.reads.clone(),
                    self.block_hash.clone(),
                    self.tree_states.clone(),
                )
            });
        }

        if matches!(
            path,
            path::GET_ADDRESS_UTXOS
                | path::GET_ADDRESS_UTXOS_STREAM
                | path::GET_TADDRESS_BALANCE
                | path::GET_TADDRESS_BALANCE_STREAM
        ) {
            return self
                .transparent_address
                .clone()
                .map(|service| Claimed::TransparentAddress(service, self.reads.clone()));
        }

        if matches!(path, path::GET_TADDRESS_TRANSACTIONS | path::GET_TADDRESS_TXIDS) {
            return self.transparent_address.clone().map(|service| {
                Claimed::TransparentTransactions(
                    service,
                    self.reads.clone(),
                    self.raw_transactions.clone(),
                )
            });
        }

        if matches!(path, path::SEND_TRANSACTION | path::GET_MEMPOOL_TX | path::GET_MEMPOOL_STREAM)
        {
            return self
                .chainview
                .clone()
                .map(|handles| Claimed::ChainView(handles, self.mempool_snapshots.clone()));
        }

        let _ = path;
        None
    }

    /// Which indexes are wired — a service handle itself tells a reader nothing.
    fn wired(&self) -> Vec<&'static str> {
        // Every push feature-gated: no index compiled in = nothing written.
        #[allow(unused_mut)]
        let mut wired = Vec::new();

        if self.compact_block.is_some() {
            wired.push("compact_block");
        }
        if self.block_hash.is_some() {
            wired.push("block_hash");
        }
        if self.tree_state.is_some() {
            wired.push("tree_state");
        }
        if self.transparent_address.is_some() {
            wired.push("transparent_address");
        }

        wired
    }
}

/// Hand-written: `TransparentAddressService` is not `Debug`.
impl<Inner: fmt::Debug> fmt::Debug for Router<Inner> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router").field("inner", &self.inner).field("wired", &self.wired()).finish()
    }
}

impl<Inner: NamedService> NamedService for Router<Inner> {
    /// The router *is* the `CompactTxStreamer`; claiming a different name would leave the
    /// intercepted paths unrouted.
    const NAME: &'static str = Inner::NAME;
}

impl<Inner, ReqBody> tower::Service<Request<ReqBody>> for Router<Inner>
where
    Inner: tower::Service<Request<ReqBody>, Response = Response<Body>> + Clone + Send + 'static,
    Inner::Future: Send + 'static,
    ReqBody: http_body::Body + Send + 'static,
    ReqBody::Data: Send,
    ReqBody::Error: std::fmt::Display,
{
    type Response = Response<Body>;
    type Error = Inner::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let Some(claimed) = self.claimed(request.uri().path()) else {
            // `Clone` then `std::mem::replace` is tower's readiness contract: the clone that
            // was polled ready is the one that must be called.
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(inner.call(request));
        };

        // Path and body are re-read per arm so a build with every index disabled leaves no
        // unused binding behind (`Claimed` is then uninhabited and this match is empty).
        match claimed {
            Claimed::CompactBlock(service, reads, locator) => Box::pin(async move {
                let path = request.uri().path().to_owned();
                Ok(compact_block::dispatch(service, locator, &path, request.into_body(), reads)
                    .await)
            }),
            Claimed::TreeState(service, reads, locator, memos) => Box::pin(async move {
                let path = request.uri().path().to_owned();
                let answering = tree_state::Answering { service, locator, reads, memos };
                Ok(tree_state::dispatch(answering, &path, request.into_body()).await)
            }),
            Claimed::TransparentAddress(service, reads) => Box::pin(async move {
                let path = request.uri().path().to_owned();
                Ok(transparent_address::dispatch(service, &path, request.into_body(), reads).await)
            }),
            Claimed::ChainView(handles, snapshots) => Box::pin(async move {
                let path = request.uri().path().to_owned();
                Ok(chainview::dispatch(&handles, &snapshots, &path, request.into_body()).await)
            }),
            Claimed::TransparentTransactions(service, reads, raw) => Box::pin(async move {
                Ok(transparent_address::transactions(service, raw, request.into_body(), reads)
                    .await)
            }),
        }
    }
}

mod compact_block {
    use bytes::Bytes;
    use http_body::Frame;
    use http_body_util::StreamBody;
    use zaino_index_compact_block::{CompactBlockService, Pools, RangeCursor, ServeError};
    use zaino_internal_block_hash_to_height::BlockHashService;

    use crate::limits::Lane;
    use crate::ReadLanes;
    use zaino_proto::proto::service as proto;

    use super::{
        path, status_response, trailers, unary_response, Body, HeaderValue, Response, Status,
    };

    /// Maps an index error onto a gRPC status, keeping the kinds distinct: a miss is not a bad
    /// request, and corruption is not either.
    pub(super) fn to_status(error: ServeError) -> Status {
        match &error {
            // `Unavailable`, not `FailedPrecondition`: clears on its own (retry with backoff)
            ServeError::Syncing | ServeError::Empty => Status::unavailable(error.to_string()),
            ServeError::NotFound { .. } | ServeError::HashNotFound => {
                Status::not_found(error.to_string())
            }
            ServeError::RangeTooLarge { .. } => Status::invalid_argument(error.to_string()),
            ServeError::Malformed { .. } => Status::internal(error.to_string()),
        }
    }

    /// Dispatches a claimed compact-block path.
    pub(super) async fn dispatch<B>(
        service: CompactBlockService,
        locator: Option<BlockHashService>,
        path: &str,
        body: B,
        reads: ReadLanes,
    ) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let answer = match path {
            path::GET_LATEST_BLOCK => latest(&service).map(unary_response),
            path::GET_BLOCK => block(service, locator, body, &reads).await.map(unary_response),
            path::GET_BLOCK_RANGE => {
                range(&service, body).await.map(|cursor| range_response(cursor, reads, Ok))
            }
            path::GET_BLOCK_RANGE_NULLIFIERS => deprecated_nullifiers::range(&service, body)
                .await
                .map(|cursor| range_response(cursor, reads, deprecated_nullifiers::reproject)),
            _ => Err(Status::unimplemented("not a compact-block method")),
        };

        match answer {
            Ok(response) => response,
            Err(status) => status_response(status),
        }
    }

    /// A server-streaming response fed from the cursor, chunk by chunk.
    ///
    /// `grpc-status` goes in the trailers, never the headers: the headers are written before
    /// the range is walked, so the final status is not yet known (and a client reading one in
    /// the headers would treat the response as already complete).
    ///
    /// `reproject` = per-chunk rewrite (`Ok` for `GetBlockRange`, so its records stay slices)
    fn range_response(
        cursor: RangeCursor,
        reads: ReadLanes,
        reproject: fn(Bytes) -> Result<Bytes, Status>,
    ) -> Response<Body> {
        let frames = futures::stream::unfold(Some((cursor, reads)), move |state| async move {
            let (mut cursor, reads) = state?;

            // The blocking pool is for steps that block. A refill faults in a cold mmap window,
            // measured at ~11.7 ms (docs/design/persistence-architecture.md), which would stall
            // every other task on that worker. Every other step is already in memory, and
            // spawning for those would cost a task per *block* on a projected range — putting a
            // bounded pool in front of the highest-volume RPC before the disk is even reached.
            let (cursor, chunk) = if cursor.next_touches_disk() {
                // Range lane, per disk step (not per request): a stream reading the nonfinalised
                // tier never queues, and no range queues a point read or a scan.
                let _permit = reads.acquire(Lane::Range).await;

                match tokio::task::spawn_blocking(move || {
                    let chunk = cursor.next_chunk();
                    (cursor, chunk)
                })
                .await
                {
                    Ok(stepped) => stepped,
                    Err(error) => match error.try_into_panic() {
                        // a walk panic = a broken invariant: re-raised (zainod aborts on it)
                        Ok(panic) => std::panic::resume_unwind(panic),
                        // runtime shutting down: nothing left to serve
                        Err(_) => return None,
                    },
                }
            } else {
                let chunk = cursor.next_chunk();
                (cursor, chunk)
            };

            let last = match chunk.map(|read| read.map_err(to_status).and_then(reproject)) {
                Some(Ok(chunk)) => return Some((Ok(Frame::data(chunk)), Some((cursor, reads)))),
                Some(Err(status)) => trailers(&status),
                None => trailers(&Status::ok("")),
            };

            Some((Ok::<_, Status>(Frame::trailers(last)), None))
        });

        let mut response = Response::new(Body::new(StreamBody::new(frames)));
        response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

        response
    }

    /// `GetLatestBlock` answers a `BlockID`, not a block — the tip's height and hash.
    ///
    /// It is the one claimed method whose reply is not a stored record, so it is the one that
    /// encodes rather than copies. The message is two fields, so that costs nothing.
    ///
    /// - inline: the tip was resolved when the view was published (no page read, no hop)
    fn latest(service: &CompactBlockService) -> Result<Bytes, Status> {
        let (height, hash) = service.latest_id().map_err(to_status)?;
        Ok(super::frame(&proto::BlockId { height: height.into(), hash: hash.to_vec() }))
    }

    /// `GetBlock` answers one whole block, every pool included.
    ///
    /// TODO: Deprecate this asymmetry, pending ZIP updates to the light client protocol.
    /// `GetBlock` returns every pool while `GetBlockRange` defaults to shielded-only, so the
    /// same wallet asking for one block and for a range of one block gets two different
    /// answers. `BlockID` has no `poolTypes`, so there is no way to ask for less. Matching
    /// lightwalletd is the only reason to keep it.
    async fn block<B>(
        service: CompactBlockService,
        locator: Option<BlockHashService>,
        body: B,
        reads: &ReadLanes,
    ) -> Result<Bytes, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let id: proto::BlockId = super::decode_request(body).await?;

        // by height at the tip (pepper-sync's reorg check): RAM, answered inline
        if id.hash.is_empty() {
            let height = super::height(id.height, "height")?;
            if let Some(record) = service.resident_block(height).map_err(to_status)? {
                return Ok(record);
            }
        }

        reads
            .read(Lane::Point, move || {
                // A hash, when given, wins: it names one block across a reorg, a height does not.
                if !id.hash.is_empty() {
                    let (height, hash) = super::locate(locator.as_ref(), &id.hash, "GetBlock")?;
                    return service.block_at_hash(height, &hash).map_err(to_status);
                }
                service.block(super::height(id.height, "height")?).map_err(to_status)
            })
            .await?
    }

    /// `GetBlockRange`: the wallet-sync path, and the one that has to be cheap.
    async fn range<B>(service: &CompactBlockService, body: B) -> Result<RangeCursor, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let request: proto::BlockRange = super::decode_request(body).await?;

        open_range(service, &request, super::pools(&request.pool_types)?)
    }

    fn open_range(
        service: &CompactBlockService,
        request: &proto::BlockRange,
        pools: Pools,
    ) -> Result<RangeCursor, Status> {
        let from = request
            .start
            .as_ref()
            .map(|id| id.height)
            .ok_or_else(|| Status::invalid_argument("range has no start"))?;
        let to = request
            .end
            .as_ref()
            .map(|id| id.height)
            .ok_or_else(|| Status::invalid_argument("range has no end"))?;
        let (from, to) =
            super::ordered(super::height(from, "range start")?, super::height(to, "range end")?)?;

        service.range(from, to, pools).map_err(to_status)
    }

    // =============================================================================================
    // !!! TODO: REMOVE THIS MODULE. DEPRECATED `GetBlockRangeNullifiers`.
    // !!! - Served only because pepper-sync still calls it; not part of Zaino's supported surface
    // !!! - Delete with its path, its `GrpcService` stub and its proto rpc once pepper-sync
    // !!!   requests `GetBlockRange` with `poolTypes`
    // =============================================================================================
    pub(super) mod deprecated_nullifiers {
        use bytes::{Buf as _, Bytes};
        use prost::Message as _;
        use zaino_index_compact_block::{CompactBlockService, RangeCursor};
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        use super::super::{pools, Status, FRAME_HEADER};
        use super::open_range;

        /// Proto: MUST ignore a `TRANSPARENT` member (dropped before projection → `[TRANSPARENT]`
        /// alone = empty = shielded default, not "no pools")
        pub(super) async fn range<B>(
            service: &CompactBlockService,
            body: B,
        ) -> Result<RangeCursor, Status>
        where
            B: http_body::Body,
            B::Error: std::fmt::Display,
        {
            let mut request: proto::BlockRange = super::super::decode_request(body).await?;
            request.pool_types.retain(|pool| *pool != proto::PoolType::Transparent as i32);

            open_range(service, &request, pools(&request.pool_types)?)
        }

        /// Framed records → framed nullifier-only records, one frame per block
        pub(super) fn reproject(mut chunk: Bytes) -> Result<Bytes, Status> {
            let malformed = || Status::internal("stored range chunk is not whole frames");
            let mut out = Vec::with_capacity(chunk.len());

            while chunk.has_remaining() {
                let header = chunk.get(..FRAME_HEADER).ok_or_else(malformed)?;
                let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
                let body = chunk.get(FRAME_HEADER..FRAME_HEADER + len).ok_or_else(malformed)?;

                let block = nullifiers_only(
                    cf::CompactBlock::decode(body).map_err(|e| Status::internal(e.to_string()))?,
                );
                out.push(0);
                out.extend_from_slice(&(block.encoded_len() as u32).to_be_bytes());
                block.encode(&mut out).map_err(|e| Status::internal(e.to_string()))?;

                chunk.advance(FRAME_HEADER + len);
            }

            Ok(Bytes::from(out))
        }

        /// Spends whole (a spend = its nullifier record), actions cut to `nullifier`, rest emptied
        pub(super) fn nullifiers_only(mut block: cf::CompactBlock) -> cf::CompactBlock {
            let cut = |actions: Vec<cf::CompactOrchardAction>| {
                actions
                    .into_iter()
                    .map(|action| cf::CompactOrchardAction {
                        nullifier: action.nullifier,
                        ..Default::default()
                    })
                    .collect()
            };

            for tx in &mut block.vtx {
                tx.outputs.clear();
                tx.vin.clear();
                tx.vout.clear();
                tx.actions = cut(std::mem::take(&mut tx.actions));
                tx.ironwood_actions = cut(std::mem::take(&mut tx.ironwood_actions));
            }
            // proto: commitment tree sizes not included
            block.chain_metadata = Some(cf::ChainMetadata::default());

            block
        }
    }
}

mod tree_state {
    use std::sync::Arc;

    use bytes::Bytes;
    use zaino_index_tree_state::{ReadView, ServeError, TreeStateService};
    use zaino_internal_block_hash_to_height::BlockHashService;
    use zaino_primitives::types::{BlockHash, Height, ShieldedPool, SubtreeRoot, Treestate};
    use zaino_proto::proto::service as proto;
    use zcash_protocol::consensus::NetworkType;

    use super::{path, status_response, streamed_response, unary_response, Body, Response, Status};
    use crate::limits::Lane;
    use crate::memo::PerView;
    use crate::ReadLanes;

    /// Framed answers per publication: the nonfinalised heights every synced wallet asks, the
    /// tip, and each pool's whole root list (sliced per request)
    #[derive(Default)]
    pub(super) struct Memos {
        states: PerView<ReadView, State, Result<Bytes, Status>>,
        roots: PerView<ReadView, ShieldedPool, Result<Arc<FramedRoots>, Status>>,
    }

    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum State {
        Latest,
        At(Height),
    }

    /// Everything one tree-state request is answered with
    pub(super) struct Answering {
        pub(super) service: TreeStateService,
        pub(super) locator: Option<BlockHashService>,
        pub(super) reads: ReadLanes,
        pub(super) memos: Arc<Memos>,
    }

    /// Framed on first ask per publication, on the point lane (single flight); then inline
    async fn once<K, T>(
        memo: fn(&Memos) -> &PerView<ReadView, K, T>,
        answering: &Answering,
        view: Arc<ReadView>,
        key: K,
        compute: impl FnOnce(&ReadView) -> T + Send + 'static,
    ) -> Result<T, Status>
    where
        K: Eq + std::hash::Hash + Send + 'static,
        T: Clone + Send + 'static,
    {
        if let Some(hit) = memo(&answering.memos).cached(&view, &key) {
            return Ok(hit);
        }
        let memos = Arc::clone(&answering.memos);
        answering
            .reads
            .read(Lane::Point, move || memo(&memos).get_or_compute(&view, key, || compute(&view)))
            .await
    }

    /// One pool's completed roots, framed back to back
    ///
    /// - `ends[i]` = offset just past root `i` (a request = one slice, one DATA chunk)
    pub(super) struct FramedRoots {
        framed: Bytes,
        ends: Vec<usize>,
    }

    impl FramedRoots {
        fn of(roots: &[SubtreeRoot]) -> Self {
            let mut framed = Vec::new();
            let mut ends = Vec::with_capacity(roots.len());
            for one in roots {
                framed.extend_from_slice(&root(one));
                ends.push(framed.len());
            }
            Self { framed: Bytes::from(framed), ends }
        }

        /// Roots `[start, start + max)`, `max == 0` = to the end (`start ≥ count` = none)
        fn slice(&self, start: u16, max: u16) -> Bytes {
            let count = self.ends.len();
            let first = usize::from(start).min(count);
            let last = match max {
                0 => count,
                max => count.min(first + usize::from(max)),
            };
            let offset = |roots: usize| roots.checked_sub(1).map_or(0, |at| self.ends[at]);
            self.framed.slice(offset(first)..offset(last))
        }
    }

    /// Maps an index error onto a gRPC status.
    ///
    /// `Syncing` = `Unavailable` (clears on its own, so back off and retry; `Unimplemented`
    /// would retire the method and `FailedPrecondition` implies the client can fix it).
    pub(super) fn to_status(error: ServeError) -> Status {
        match &error {
            ServeError::Syncing | ServeError::Empty => Status::unavailable(error.to_string()),
            ServeError::NotFound { .. } => Status::not_found(error.to_string()),
            ServeError::Inconsistent { .. } => Status::internal(error.to_string()),
        }
    }

    /// Dispatches a claimed tree-state path.
    pub(super) async fn dispatch<B>(answering: Answering, path: &str, body: B) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let answer = match path {
            path::GET_TREE_STATE => treestate(answering, body).await.map(unary_response),
            path::GET_LATEST_TREE_STATE => latest(&answering).await.map(unary_response),
            path::GET_SUBTREE_ROOTS => subtree_roots(&answering, body).await.map(|chunk| {
                streamed_response((!chunk.is_empty()).then_some(chunk).into_iter().collect())
            }),
            _ => Err(Status::unimplemented("not a tree-state method")),
        };

        match answer {
            Ok(response) => response,
            Err(status) => status_response(status),
        }
    }

    async fn latest(answering: &Answering) -> Result<Bytes, Status> {
        let view = answering.service.pin().map_err(to_status)?;
        let network = answering.service.network();
        once(
            |memos| &memos.states,
            answering,
            view,
            State::Latest,
            move |view| view.latest().map(|state| reply(&state, network)).map_err(to_status),
        )
        .await?
    }

    /// A hash, when given, wins (it names one block across a reorg): the block-hash index locates
    /// its height, this index answers there only if it holds that same block
    ///
    /// - by height, nonfinalised (the synced wallets' tip asks): once per publication
    async fn treestate<B>(answering: Answering, body: B) -> Result<Bytes, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let id: proto::BlockId = super::decode_request(body).await?;

        if id.hash.is_empty() {
            let height = super::height(id.height, "height")?;
            if let Ok(view) = answering.service.pin() {
                if view.is_nonfinalized(height) {
                    let network = answering.service.network();
                    return once(
                        |memos| &memos.states,
                        &answering,
                        view,
                        State::At(height),
                        move |view| {
                            view.treestate(height)
                                .map(|state| reply(&state, network))
                                .map_err(to_status)
                        },
                    )
                    .await?;
                }
            }
        }

        let Answering { service, locator, reads, .. } = answering;
        reads.read(Lane::Point, move || treestate_of(&service, locator.as_ref(), &id)).await?
    }

    fn treestate_of(
        service: &TreeStateService,
        locator: Option<&BlockHashService>,
        id: &proto::BlockId,
    ) -> Result<Bytes, Status> {
        if id.hash.is_empty() {
            let state =
                service.treestate(super::height(id.height, "height")?).map_err(to_status)?;
            return Ok(reply(&state, service.network()));
        }

        let (height, hash) = super::locate(locator, &id.hash, "GetTreeState")?;
        let state = service.treestate(height).map_err(to_status)?;
        if state.block_hash != BlockHash::from(hash) {
            return Err(Status::not_found(format!(
                "block {} is not the one this index holds at {height} (reorg)",
                BlockHash::from(hash)
            )));
        }
        Ok(reply(&state, service.network()))
    }

    /// Domain treestate → wire: trees hex, hash in display order.
    /// lightwalletd's spelling of the chain, which is not `NetworkType`'s own.
    ///
    /// The proto documents `"main"` / `"test"`; `regtest` is lightwalletd's de-facto third.
    fn network_name(network: NetworkType) -> &'static str {
        match network {
            NetworkType::Main => "main",
            NetworkType::Test => "test",
            NetworkType::Regtest => "regtest",
        }
    }

    fn reply(state: &Treestate, network: NetworkType) -> Bytes {
        super::frame(&proto::TreeState {
            network: network_name(network).to_owned(),
            height: u64::from(state.height),
            hash: state.block_hash.to_string(),
            time: state.time,
            sapling_tree: hex::encode(state.sapling.as_bytes()),
            orchard_tree: hex::encode(state.orchard.as_bytes()),
            ironwood_tree: hex::encode(state.ironwood.as_bytes()),
        })
    }

    /// Every root of the pool framed once per publication; a request = one slice of it
    async fn subtree_roots<B>(answering: &Answering, body: B) -> Result<Bytes, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let request: proto::GetSubtreeRootsArg = super::decode_request(body).await?;

        let pool = match proto::ShieldedProtocol::try_from(request.shielded_protocol) {
            Ok(proto::ShieldedProtocol::Sapling) => ShieldedPool::Sapling,
            Ok(proto::ShieldedProtocol::Orchard) => ShieldedPool::Orchard,
            Ok(proto::ShieldedProtocol::Ironwood) => ShieldedPool::Ironwood,
            Err(_) => {
                return Err(Status::invalid_argument(format!(
                    "unknown shieldedProtocol {}",
                    request.shielded_protocol
                )))
            }
        };

        // Depth-32 tree = ≤ 2^16 subtrees, so anything wider names none.
        let ceiling = |field: &str| {
            Status::invalid_argument(format!("{field} is above the 2^16 subtree ceiling"))
        };
        let start = u16::try_from(request.start_index).map_err(|_| ceiling("startIndex"))?;
        let max = u16::try_from(request.max_entries).map_err(|_| ceiling("maxEntries"))?;

        let view = answering.service.pin().map_err(to_status)?;
        let framed = once(
            |memos| &memos.roots,
            answering,
            view,
            pool,
            move |view| {
                let every = view.subtree_roots(pool, 0, 0).map_err(to_status)?;
                Ok(Arc::new(FramedRoots::of(&every)))
            },
        )
        .await??;
        Ok(framed.slice(start, max))
    }

    /// `completingBlockHash` in display order (lightwalletd reverses the internal-order hash)
    fn root(root: &SubtreeRoot) -> Bytes {
        let mut hash = <[u8; 32]>::from(root.completing.hash);
        hash.reverse();
        super::frame(&proto::SubtreeRoot {
            root_hash: <[u8; 32]>::from(root.root).to_vec(),
            completing_block_hash: hash.to_vec(),
            completing_block_height: u64::from(root.completing.height),
        })
    }

    #[cfg(test)]
    mod tests {
        use prost::Message as _;
        use zaino_primitives::types::{BlockRef, SubtreeRoot};
        use zaino_proto::proto::service as proto;

        /// Any `[start, start + max)` of the pre-framed list = exactly those roots, framed as one
        /// per record; past the end = empty (pepper-sync's probe), never a panic
        #[test]
        fn a_root_request_is_one_slice_of_the_framed_list() {
            let roots: Vec<SubtreeRoot> = (0..5u8)
                .map(|seed| SubtreeRoot {
                    root: [seed; 32].into(),
                    completing: BlockRef {
                        hash: [seed; 32].into(),
                        height: (u32::from(seed) * 10).try_into().expect("height"),
                    },
                })
                .collect();
            let framed = super::FramedRoots::of(&roots);
            let heights = |mut chunk: bytes::Bytes| {
                let mut found = Vec::new();
                while !chunk.is_empty() {
                    let len = u32::from_be_bytes(chunk[1..5].try_into().expect("header")) as usize;
                    let root = proto::SubtreeRoot::decode(&chunk[5..5 + len]).expect("decodes");
                    found.push(root.completing_block_height);
                    chunk = chunk.slice(5 + len..);
                }
                found
            };

            let cases: [((u16, u16), &[u64]); 6] = [
                ((0, 0), &[0, 10, 20, 30, 40]),
                ((2, 0), &[20, 30, 40]),
                ((1, 2), &[10, 20]),
                ((3, 9), &[30, 40]),
                ((5, 0), &[]),
                ((9, 1), &[]),
            ];
            for ((start, max), expected) in cases {
                assert_eq!(heights(framed.slice(start, max)), expected, "start {start} max {max}");
            }
        }
    }
}

/// `SendTransaction` and the mempool methods, over the multi-validator view.
mod chainview {
    use http::HeaderValue;
    use http_body::Frame;
    use http_body_util::StreamBody;
    use zaino_chainview::{BroadcastError, ChainViewSnapshot, ChainViewSubscriber, MempoolTail};
    use zaino_proto::proto::service as proto;

    use super::{
        decode_request, frame, path, status_response, trailers, Body, Pools, Response, Status,
    };
    use crate::validator::Relay;

    pub(super) async fn dispatch<B>(
        handles: &super::ChainViewHandles,
        snapshots: &SnapshotFrames,
        path: &str,
        body: B,
    ) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        match path {
            path::SEND_TRANSACTION => match send(handles.relay.as_ref(), body).await {
                Ok(record) => super::unary_response(record),
                Err(status) => status_response(status),
            },
            path::GET_MEMPOOL_STREAM => stream(&handles.view, snapshots),
            path::GET_MEMPOOL_TX => match compact(handles, body).await {
                Ok(records) => super::streamed_response(records),
                Err(status) => status_response(status),
            },
            _ => status_response(Status::unimplemented("not a chainview method")),
        }
    }

    /// `GetMempoolTx`: the servable mempool minus what the client already holds, compacted.
    ///
    /// Materialised rather than streamed lazily: the projection is CPU over bytes already in
    /// memory, so there is no round trip to defer, and the pinned view must not be held across
    /// awaits for the life of a stream.
    async fn compact<B>(
        handles: &super::ChainViewHandles,
        body: B,
    ) -> Result<Vec<bytes::Bytes>, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let request: proto::GetMempoolTxRequest = decode_request(body).await?;
        let pools = super::pools(&request.pool_types)?;

        let pinned = handles.view.current();
        let mempool = pinned.mempool().map_err(|below| Status::unavailable(below.to_string()))?;

        mempool
            .excluding(&request.exclude_txid_suffixes)
            .into_iter()
            .enumerate()
            .map(|(slot, entry)| {
                let mut tx = handles.compact.project(slot as u64, &entry.raw, entry.fee)?;
                prune(&mut tx, pools);
                Ok(frame(&tx))
            })
            .collect()
    }

    /// Drops every pool the request did not ask for (here, not in the projection: the port
    /// renders bytes, the request shapes the answer)
    fn prune(tx: &mut zaino_proto::proto::compact_formats::CompactTx, pools: Pools) {
        if !pools.transparent {
            tx.vin.clear();
            tx.vout.clear();
        }
        if !pools.sapling {
            tx.spends.clear();
            tx.outputs.clear();
        }
        if !pools.orchard {
            tx.actions.clear();
        }
        if !pools.ironwood {
            tx.ironwood_actions.clear();
        }
    }

    /// Relayed to every endpoint; a rejection is a domain answer, not a transport error.
    ///
    /// A wallet must tell "the network said no" from "the network is unreachable", so only the
    /// latter is a status.
    async fn send<B>(relay: &dyn Relay, body: B) -> Result<bytes::Bytes, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let raw: proto::RawTransaction =
            super::decode_request_within(body, super::request_limit::TRANSACTION).await?;

        let reply = match relay.relay(raw.data.to_vec()).await {
            Ok(txid) => proto::SendResponse {
                error_code: 0,
                error_message: <[u8; 32]>::from(txid)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            },
            Err(BroadcastError::Rejected(rejection)) => {
                proto::SendResponse { error_code: -1, error_message: rejection.to_string() }
            }
            Err(unreachable @ BroadcastError::Unreachable { .. }) => {
                return Err(Status::unavailable(unreachable.to_string()))
            }
        };

        Ok(frame(&reply))
    }

    /// A tail's opening snapshot, framed once per published view
    ///
    /// - every wallet reconnects on each block, onto the same published view: one render (a
    ///   memcpy of the mempool), then shared by refcount (one DATA chunk each)
    pub(super) type SnapshotFrames = crate::memo::PerView<ChainViewSnapshot, (), bytes::Bytes>;

    fn opening(snapshots: &SnapshotFrames, tail: &MempoolTail) -> bytes::Bytes {
        snapshots.get_or_compute(tail.anchor(), (), || {
            let entries: Vec<proto::RawTransaction> =
                tail.snapshot().entries().map(|entry| raw_transaction(entry.raw)).collect();
            super::frame_all(&entries)
        })
    }

    /// Unmined by construction; the wire spells that `height: 0`
    fn raw_transaction(data: bytes::Bytes) -> proto::RawTransaction {
        proto::RawTransaction { data, height: 0 }
    }

    /// `GetMempoolStream`: the servable mempool as one chunk, then each arrival, closing on a
    /// mined block (below quorum = `UNAVAILABLE`, never a silent stream)
    fn stream(view: &ChainViewSubscriber, snapshots: &SnapshotFrames) -> Response<Body> {
        let tail = match view.tail() {
            Ok(tail) => tail,
            Err(below) => return status_response(Status::unavailable(below.to_string())),
        };
        let snapshot = opening(snapshots, &tail);
        let opening = (!snapshot.is_empty()).then(|| Ok::<_, Status>(Frame::data(snapshot)));

        // The tail rides in the unfold state rather than being captured: it is borrowed mutably
        // across an await, which a `FnMut` closure cannot hold.
        let arrivals = futures::stream::unfold(Some(tail), move |state| async move {
            let mut tail = state?;

            let Some(entry) = tail.next().await else {
                return Some((Ok(Frame::trailers(trailers(&Status::ok("")))), None));
            };

            let record = frame(&raw_transaction(entry.raw));
            Some((Ok::<_, Status>(Frame::data(record)), Some(tail)))
        });
        let frames = futures::StreamExt::chain(futures::stream::iter(opening), arrivals);

        let mut response = Response::new(Body::new(StreamBody::new(frames)));
        response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

        response
    }

    #[cfg(test)]
    mod tests {
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service::PoolType;

        /// A full transaction, pruned once per request shape; a value naming no pool is refused.
        ///
        /// Empty is the one case that is not "all": the wire pins it to the legacy shielded set,
        /// so transparent must be withheld from a client that named no pool at all.
        #[test]
        fn pruning_keeps_the_named_pools_empty_means_shielded_only_and_unknown_is_refused() {
            let full = cf::CompactTx {
                index: 7,
                txid: vec![0xab; 32],
                fee: 0,
                spends: vec![cf::CompactSaplingSpend { nf: vec![1; 32] }],
                outputs: vec![cf::CompactSaplingOutput::default()],
                actions: vec![cf::CompactOrchardAction::default()],
                ironwood_actions: vec![cf::CompactOrchardAction::default()],
                vin: vec![cf::CompactTxIn::default()],
                vout: vec![cf::TxOut::default()],
            };

            let pruned = |pools: &[PoolType]| {
                let mut tx = full.clone();
                let pools: Vec<i32> = pools.iter().map(|pool| *pool as i32).collect();
                super::prune(&mut tx, crate::router::pools(&pools).expect("known pools"));
                tx
            };

            for (raw, value) in [(vec![PoolType::Invalid as i32], 0), (vec![2, 99, 3], 99)] {
                let refused = crate::router::pools(&raw).expect_err("names no pool");
                let expected = format!(
                    "poolTypes value {value} is not a pool \
                     (TRANSPARENT=1, SAPLING=2, ORCHARD=3, IRONWOOD=4)"
                );
                let got = (refused.code(), refused.message());
                assert_eq!(got, (tonic::Code::InvalidArgument, expected.as_str()), "{raw:?}");
            }

            let shielded = cf::CompactTx { vin: vec![], vout: vec![], ..full.clone() };
            let transparent = cf::CompactTx {
                spends: vec![],
                outputs: vec![],
                actions: vec![],
                ironwood_actions: vec![],
                ..full.clone()
            };
            let subset =
                cf::CompactTx { actions: vec![], vin: vec![], vout: vec![], ..full.clone() };
            let (sapling_ironwood, every) = (
                [PoolType::Sapling, PoolType::Ironwood],
                [PoolType::Transparent, PoolType::Sapling, PoolType::Orchard, PoolType::Ironwood],
            );
            assert_eq!(pruned(&[]), shielded, "none named = shielded only");
            assert_eq!(pruned(&[PoolType::Transparent]), transparent, "transparent alone");
            assert_eq!(pruned(&sapling_ironwood), subset, "subset = exactly its members");
            assert_eq!(pruned(&every), full, "every pool = no-op");
        }
    }
}

mod transparent_address {
    use bytes::Bytes;
    use zaino_index_transparent_address::{AddressUtxo, ServeError, TransparentAddressService};
    use zaino_primitives::types::Zatoshis;
    use zaino_proto::proto::service as proto;
    use zcash_address::{ConversionError, ZcashAddress};
    use zcash_protocol::consensus::NetworkType;
    use zcash_script::script::Evaluable;
    use zcash_transparent::address::TransparentAddress;

    use http::HeaderValue;
    use http_body::Frame;
    use http_body_util::StreamBody;

    use super::{
        decode_request, frame, path, status_response, streamed_response, trailers, unary_response,
        Body, Response, Status,
    };
    use crate::limits::Lane;
    use crate::ReadLanes;

    /// `Syncing` = `Unavailable` (clears on its own; `Unimplemented` would retire the method)
    pub(super) fn to_status(error: ServeError) -> Status {
        match &error {
            ServeError::Syncing => Status::unavailable(error.to_string()),
            ServeError::SupplyExceeded => Status::internal(error.to_string()),
            ServeError::TooManyRows { .. } => Status::resource_exhausted(error.to_string()),
        }
    }

    /// Dispatches a claimed transparent-address path.
    pub(super) async fn dispatch<B>(
        service: TransparentAddressService,
        path: &str,
        body: B,
        reads: ReadLanes,
    ) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let answer = match path {
            path::GET_ADDRESS_UTXOS => utxos(service, body, &reads)
                .await
                .map(|address_utxos| {
                    super::frame(&proto::GetAddressUtxosReplyList { address_utxos })
                })
                .map(unary_response),
            path::GET_ADDRESS_UTXOS_STREAM => utxos(service, body, &reads)
                .await
                .map(|replies| replies.iter().map(super::frame).collect())
                .map(streamed_response),
            path::GET_TADDRESS_BALANCE => {
                balance_of(service, body, &reads).await.map(unary_response)
            }
            path::GET_TADDRESS_BALANCE_STREAM => {
                streamed_balance_of(service, body, &reads).await.map(unary_response)
            }
            _ => Err(Status::unimplemented("not a transparent-address method")),
        };

        match answer {
            Ok(response) => response,
            Err(status) => status_response(status),
        }
    }

    /// `GetTaddressTransactions` — the index names them, the validator supplies them.
    ///
    /// Fetches **lazily**, one per poll: a busy address over a wide range could name thousands
    /// of transactions, and a client that stops reading stops the round trips rather than
    /// having paid for all of them up front. HTTP/2 flow control does the pacing.
    pub(super) async fn transactions<B>(
        service: TransparentAddressService,
        raw: std::sync::Arc<dyn crate::validator::FetchRawTransaction>,
        body: B,
        reads: ReadLanes,
    ) -> Response<Body>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let found = match found_transactions(service, body, &reads).await {
            Ok(found) => found,
            Err(status) => return status_response(status),
        };

        // `None` state ends the stream, so the trailer frame is always last and always sent.
        let frames = futures::stream::unfold(Some(found.into_iter()), move |state| {
            let raw = std::sync::Arc::clone(&raw);
            async move {
                let mut rest = state?;

                let Some(txid) = rest.next() else {
                    return Some((Ok(Frame::trailers(trailers(&Status::ok("")))), None));
                };

                match raw.fetch(txid).await.map(|tx| frame(&tx)) {
                    Ok(record) => Some((Ok::<_, Status>(Frame::data(record)), Some(rest))),
                    Err(status) => Some((Ok(Frame::trailers(trailers(&status))), None)),
                }
            }
        });

        let mut response = Response::new(Body::new(StreamBody::new(frames)));
        response
            .headers_mut()
            .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));

        response
    }

    /// The txids the index says touched the address, in height order.
    async fn found_transactions<B>(
        service: TransparentAddressService,
        body: B,
        reads: &ReadLanes,
    ) -> Result<Vec<zaino_primitives::types::TransactionId>, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let request: proto::TransparentAddressBlockFilter = decode_request(body).await?;
        let address = transparent_address(&request.address, service.network())?;
        let range = request.range.ok_or_else(|| Status::invalid_argument("range is required"))?;

        let height = |end: Option<proto::BlockId>, field: &str| {
            end.ok_or_else(|| Status::invalid_argument(format!("range.{field} is required")))
                .and_then(|at| super::height(at.height, field))
        };
        let (from, to) = super::ordered(height(range.start, "start")?, height(range.end, "end")?)?;

        let found =
            reads.read(Lane::Scan, move || service.transactions(&address, from, to)).await?;

        Ok(found.map_err(to_status)?.into_iter().map(|found| found.txid).collect())
    }

    /// Only `network`'s encodings (index keys = `[kind][hash160]`: a foreign encoding would
    /// answer from this network's rows)
    fn transparent_address(
        encoded: &str,
        network: NetworkType,
    ) -> Result<TransparentAddress, Status> {
        let invalid =
            |reason: String| Status::invalid_argument(format!("address {encoded}: {reason}"));

        encoded
            .parse::<ZcashAddress>()
            .map_err(|error| invalid(error.to_string()))?
            .convert_if_network::<TransparentAddress>(network)
            .map_err(|error| match error {
                ConversionError::IncorrectNetwork { expected, actual } => invalid(format!(
                    "a {} address, and this index is {}",
                    network_name(actual),
                    network_name(expected)
                )),
                _ => invalid("not a transparent address".to_owned()),
            })
    }

    fn network_name(network: NetworkType) -> &'static str {
        match network {
            NetworkType::Main => "mainnet",
            NetworkType::Test => "testnet",
            NetworkType::Regtest => "regtest",
        }
    }

    /// `GetAddressUtxos` / `GetAddressUtxosStream` — the same walk, two response shapes.
    async fn utxos<B>(
        service: TransparentAddressService,
        body: B,
        reads: &ReadLanes,
    ) -> Result<Vec<proto::GetAddressUtxosReply>, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let request: proto::GetAddressUtxosArg = super::decode_request(body).await?;
        let from = super::height(request.start_height, "startHeight")?;
        let addresses = request
            .addresses
            .iter()
            .map(|encoded| transparent_address(encoded, service.network()))
            .collect::<Result<Vec<_>, _>>()?;

        let (addresses, utxos) = reads
            .read(Lane::Scan, move || {
                let utxos = service.utxos_of(&addresses, from);
                (addresses, utxos)
            })
            .await?;
        let mut found = Vec::new();
        for ((encoded, address), utxos) in
            request.addresses.iter().zip(&addresses).zip(utxos.map_err(to_status)?)
        {
            for utxo in &utxos {
                found.push(reply(encoded, address, utxo)?);
            }
        }

        // `GetAddressUtxosArg` documents results as height-ordered, which a per-address walk is
        // only within one address.
        found.sort_by(|left, right| {
            (left.height, &left.txid, left.index).cmp(&(right.height, &right.txid, right.index))
        });
        if request.max_entries > 0 {
            found.truncate(request.max_entries as usize);
        }

        Ok(found)
    }

    fn reply(
        encoded: &str,
        address: &TransparentAddress,
        utxo: &AddressUtxo,
    ) -> Result<proto::GetAddressUtxosReply, Status> {
        Ok(proto::GetAddressUtxosReply {
            address: encoded.to_owned(),
            txid: <[u8; 32]>::from(utxo.txid).to_vec(),
            index: i32::try_from(utxo.vout)
                .map_err(|_| Status::internal("stored vout is above the protocol ceiling"))?,
            script: address.script().to_bytes(),
            value_zat: utxo.value.as_i64(),
            height: u64::from(utxo.height),
        })
    }

    async fn balance_of<B>(
        service: TransparentAddressService,
        body: B,
        reads: &ReadLanes,
    ) -> Result<Bytes, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let list: proto::AddressList = super::decode_request(body).await?;
        balance(service, &list.addresses, reads).await
    }

    /// `GetTaddressBalanceStream` is client-streaming: the body is one framed `Address` each,
    /// and the reply is still the single total.
    async fn streamed_balance_of<B>(
        service: TransparentAddressService,
        body: B,
        reads: &ReadLanes,
    ) -> Result<Bytes, Status>
    where
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let streamed: Vec<proto::Address> = super::decode_request_stream(body).await?;
        let addresses: Vec<String> = streamed.into_iter().map(|one| one.address).collect();

        balance(service, &addresses, reads).await
    }

    /// Deduped on the parsed address (a repeated address counts once)
    /// - distinct addresses' balances sum within the supply, so overflow = index corruption
    async fn balance(
        service: TransparentAddressService,
        addresses: &[String],
        reads: &ReadLanes,
    ) -> Result<Bytes, Status> {
        let distinct: Vec<TransparentAddress> = addresses
            .iter()
            .map(|encoded| transparent_address(encoded, service.network()))
            .collect::<Result<std::collections::BTreeSet<_>, _>>()?
            .into_iter()
            .collect();
        let balances = reads.read(Lane::Scan, move || service.balances(&distinct)).await?;

        let total = Zatoshis::sum_balances(balances.map_err(to_status)?.into_iter())
            .expect("distinct addresses' balances exceed the money supply: index corrupt");

        Ok(super::frame(&proto::Balance { value_zat: total.as_i64() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_index_transparent_address::TransparentAddressService;
    use zaino_sync::Served;

    /// Network the transparent-address tests build their index on (addresses are mainnet ones)
    const MAINNET: zcash_protocol::consensus::NetworkType =
        zcash_protocol::consensus::NetworkType::Main;

    /// An inner service that records whether it was reached, standing in for the generated
    /// `CompactTxStreamer` server.
    #[derive(Clone, Default)]
    struct SpyInner {
        reached: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl NamedService for SpyInner {
        const NAME: &'static str = "cash.z.wallet.sdk.rpc.CompactTxStreamer";
    }

    impl tower::Service<Request<Full<bytes::Bytes>>> for SpyInner {
        type Response = Response<Body>;
        type Error = std::convert::Infallible;
        type Future =
            Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: Request<Full<bytes::Bytes>>) -> Self::Future {
            self.reached.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(status_response(Status::unimplemented("inner"))) })
        }
    }

    /// Validator bytes port a test must never reach (a call = routed where it must not be)
    struct NoValidator;

    impl crate::validator::FetchRawTransaction for NoValidator {
        fn fetch(
            &self,
            txid: zaino_primitives::types::TransactionId,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<zaino_proto::proto::service::RawTransaction, Status>>
                    + Send
                    + '_,
            >,
        > {
            panic!("validator asked for {txid:?}")
        }
    }

    fn unwired<Inner>(inner: Inner) -> Router<Inner> {
        Router::new(
            inner,
            std::sync::Arc::new(NoValidator),
            ReadLanes::new(&crate::GrpcLimits::default()),
        )
    }

    fn request(path: &str) -> Request<Full<bytes::Bytes>> {
        framed_request(path, bytes::Bytes::new())
    }

    /// A gRPC request whose body is one framed message.
    fn framed_request(path: &str, message: bytes::Bytes) -> Request<Full<bytes::Bytes>> {
        let mut framed = Vec::with_capacity(FRAME_HEADER + message.len());
        framed.push(0);
        framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
        framed.extend_from_slice(&message);

        Request::builder()
            .uri(format!("http://localhost{path}"))
            .body(Full::new(bytes::Bytes::from(framed)))
            .expect("request")
    }

    /// The router claims a method only when an index is wired for it; everything else reaches
    /// the inner service untouched.
    #[tokio::test]
    async fn unclaimed_methods_fall_through_to_the_inner_service() {
        use tower::Service as _;

        let spy = SpyInner::default();
        let reached = spy.reached.clone();
        let mut router = unwired(spy);

        // No index wired → even a claimable path belongs to `inner`.
        let paths = [
            path::GET_BLOCK,
            path::GET_BLOCK_RANGE,
            path::GET_TREE_STATE,
            path::GET_SUBTREE_ROOTS,
            path::GET_ADDRESS_UTXOS,
            path::GET_TADDRESS_BALANCE,
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction",
        ];
        for path in paths {
            assert!(router.claimed(path).is_none(), "nothing is claimed yet: {path}");
            let _ = router.call(request(path)).await.expect("inner answers");
        }
        let hits = reached.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(hits, paths.len(), "every path reached the inner service");

        // The router keeps the service name, so the intercepted paths stay routable.
        let name = <Router<SpyInner> as NamedService>::NAME;
        assert_eq!(name, "cash.z.wallet.sdk.rpc.CompactTxStreamer");
    }

    #[tokio::test]
    async fn a_wired_index_claims_its_methods_and_leaves_the_rest() {
        use tower::Service as _;
        use zaino_index_compact_block::{CompactBlockService, CompactBlockStore};

        let store = CompactBlockStore::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/cb"),
            zcash_protocol::consensus::NetworkType::Regtest,
        )
        .expect("open");
        let spy = SpyInner::default();
        let reached = spy.reached.clone();

        let mut router = unwired(spy)
            .with_compact_block(CompactBlockService::new(Served::fixed(store.reader().pin())));

        for path in [path::GET_LATEST_BLOCK, path::GET_BLOCK, path::GET_BLOCK_RANGE] {
            assert!(router.claimed(path).is_some(), "index claims {path}");
        }

        // Methods no index backs still belong to the inner service.
        let others = [
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTreeState",
        ];
        for path in others {
            assert!(router.claimed(path).is_none(), "not claimed: {path}");
            let _ = router.call(request(path)).await.expect("inner answers");
        }
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), 2);

        // An empty store has no tip, and that is a status rather than a panic.
        let response = router.call(request(path::GET_LATEST_BLOCK)).await.expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("14")), "unavailable while empty");
    }

    /// `GetBlock` by hash: the block-hash index locates, the compact index answers only where it
    /// holds that same block; no locator wired = `Unimplemented`
    #[tokio::test]
    async fn get_block_by_hash_answers_only_where_the_compact_index_holds_that_block() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_compact_block::{
            encode_compact_block, testing, CompactBlockService, CompactBlockStore,
        };
        use zaino_internal_block_hash_to_height::{BlockHashService, BlockHashStore};
        use zaino_proto::proto::service as proto;

        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let mut compact = CompactBlockStore::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/cb"),
            net,
        )
        .expect("open");
        for height in 0..3u32 {
            let (block, balances, sizes) = testing::block(height);
            let record = encode_compact_block(&block, &balances, &sizes);
            compact
                .append(block.header().height, block.header().hash.into(), &record)
                .expect("append");
        }
        compact.commit(testing::block(2).2).expect("commit");
        // Locator's chain = the compact index's through 1, then a fork at 2
        let mut hashes = BlockHashStore::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/bh"),
            net,
        )
        .expect("open");
        let height = |n: u32| Height::try_from(n).expect("height");
        hashes
            .commit(&[(height(0), [0u8; 32]), (height(1), [1; 32]), (height(2), [0xcd; 32])])
            .expect("commit");

        let service = CompactBlockService::new(Served::fixed(compact.reader().pin()));
        let unlocated = unwired(SpyInner::default()).with_compact_block(service.clone());
        let mut router = unlocated
            .clone()
            .with_block_hash(BlockHashService::new(Served::fixed(hashes.reader().pin())));
        let request = |hash: Vec<u8>| {
            framed_request(
                path::GET_BLOCK,
                proto::BlockId { height: 0, hash }.encode_to_vec().into(),
            )
        };
        let code = |response: &Response<Body>| {
            Status::from_header_map(response.headers())
                .map(|status| status.code())
                .unwrap_or(tonic::Code::Ok)
        };

        let response = router.call(request(vec![1; 32])).await.expect("answers");
        assert_eq!(code(&response), tonic::Code::Ok);
        let by_height = service.block(Height::try_from(1).expect("height")).expect("by height");
        let body = response.into_body().collect().await.expect("body").to_bytes();
        assert_eq!(body, by_height, "same stored record as by height");

        let mut codes = Vec::new();
        for (mut router, hash) in [
            (router.clone(), vec![0xee; 32]),
            (router.clone(), vec![0xcd; 32]),
            (router.clone(), vec![1; 31]),
            (unlocated, vec![1; 32]),
        ] {
            codes.push(code(&router.call(request(hash)).await.expect("answers")));
        }
        use tonic::Code::{InvalidArgument, NotFound, Unimplemented};
        let expected = [NotFound, NotFound, InvalidArgument, Unimplemented];
        assert_eq!(codes, expected, "unknown hash, other block at 2, short hash, no locator");
    }

    /// A populated index answers real requests, and the body is the stored bytes.
    #[tokio::test]
    async fn get_block_and_get_block_range_answer_from_stored_records() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_compact_block::{
            encode_compact_block, testing, CompactBlockService, CompactBlockStore,
        };
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        let mut store = CompactBlockStore::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/cb"),
            zcash_protocol::consensus::NetworkType::Regtest,
        )
        .expect("open");
        for height in 0..6u32 {
            let (block, balances, sizes) = testing::block(height);
            let record = encode_compact_block(&block, &balances, &sizes);
            store
                .append(block.header().height, block.header().hash.into(), &record)
                .expect("append");
        }
        store.commit(testing::block(5).2).expect("commit");

        let service = CompactBlockService::new(Served::fixed(store.reader().pin()));
        let mut router = unwired(SpyInner::default()).with_compact_block(service);

        async fn body_of(response: Response<Body>) -> bytes::Bytes {
            response.into_body().collect().await.expect("body").to_bytes()
        }

        // GetBlock: one framed record, decodable as a CompactBlock.
        let id = proto::BlockId { height: 3, hash: Vec::new() };
        let response = router
            .call(framed_request(path::GET_BLOCK, id.encode_to_vec().into()))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), Some(&HeaderValue::from_static("0")));

        let body = body_of(response).await;
        let decoded = cf::CompactBlock::decode(&body[FRAME_HEADER..]).expect("one framed message");
        assert_eq!(decoded.height, 3);
        assert_eq!(decoded.vtx[0].vin.len(), 1, "GetBlock carries transparent");

        // Data frames in order, then the trailers — a streaming body's `grpc-status` is not in
        // the headers, so a client that never drains the body never sees a status at all.
        async fn drained(response: Response<Body>) -> (Vec<bytes::Bytes>, HeaderMap) {
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

        // GetBlockRange with the default (empty) poolTypes: shielded only, each file window
        // projected whole (one data frame per window, ≤ 1 MiB), records still framed one by one
        let range = proto::BlockRange {
            start: Some(proto::BlockId { height: 1, hash: Vec::new() }),
            end: Some(proto::BlockId { height: 4, hash: Vec::new() }),
            pool_types: Vec::new(),
        };
        let response = router
            .call(framed_request(path::GET_BLOCK_RANGE, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        let headers = response.headers();
        let grpc = HeaderValue::from_static("application/grpc");
        let (status, content) =
            (headers.get("grpc-status"), headers.get(http::header::CONTENT_TYPE));
        assert_eq!((status, content), (None, Some(&grpc)), "stream status rides in the trailers");

        let (chunks, trailing) = drained(response).await;
        assert_eq!(chunks.len(), 1, "four committed records = one projected window");
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));

        // Walk the joined body as the client would: one framed message after another.
        let body = chunks.concat();
        let mut at = 0usize;
        let mut heights = Vec::new();
        while at < body.len() {
            let len = u32::from_be_bytes(body[at + 1..at + 5].try_into().expect("len")) as usize;
            let message = &body[at + FRAME_HEADER..at + FRAME_HEADER + len];
            let block = cf::CompactBlock::decode(message).expect("framed message");

            assert!(block.vtx[0].vin.is_empty(), "default prunes transparent");
            assert_eq!(block.vtx[0].actions.len(), 1, "shielded kept");
            heights.push(block.height);
            at += FRAME_HEADER + len;
        }
        assert_eq!(heights, vec![1, 2, 3, 4], "in order, none missing");

        // An explicit poolTypes list is honoured.
        let range = proto::BlockRange {
            start: Some(proto::BlockId { height: 2, hash: Vec::new() }),
            end: Some(proto::BlockId { height: 2, hash: Vec::new() }),
            pool_types: vec![proto::PoolType::Transparent as i32],
        };
        let response = router
            .call(framed_request(path::GET_BLOCK_RANGE, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        let (chunks, trailing) = drained(response).await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let decoded =
            cf::CompactBlock::decode(&chunks.concat()[FRAME_HEADER..]).expect("framed message");
        assert_eq!(decoded.vtx[0].vin.len(), 1, "transparent requested, so kept");
        assert!(decoded.vtx[0].actions.is_empty(), "orchard not requested");

        // TODO: REMOVE with the deprecated alias. `[TRANSPARENT]` ignored → shielded default;
        // every nullifier kept, everything else gone
        let range_of = |pool_types: Vec<i32>| proto::BlockRange {
            start: Some(proto::BlockId { height: 1, hash: Vec::new() }),
            end: Some(proto::BlockId { height: 4, hash: Vec::new() }),
            pool_types,
        };
        let decoded_all = |body: bytes::Bytes| {
            let mut at = 0usize;
            let mut blocks = Vec::new();
            while at < body.len() {
                let len =
                    u32::from_be_bytes(body[at + 1..at + 5].try_into().expect("len")) as usize;
                blocks.push(
                    cf::CompactBlock::decode(&body[at + FRAME_HEADER..at + FRAME_HEADER + len])
                        .expect("framed message"),
                );
                at += FRAME_HEADER + len;
            }
            blocks
        };
        let (full, _) = drained(
            router
                .call(framed_request(
                    path::GET_BLOCK_RANGE,
                    range_of(Vec::new()).encode_to_vec().into(),
                ))
                .await
                .expect("router answers"),
        )
        .await;
        let (nullifiers, trailing) = drained(
            router
                .call(framed_request(
                    path::GET_BLOCK_RANGE_NULLIFIERS,
                    range_of(vec![proto::PoolType::Transparent as i32]).encode_to_vec().into(),
                ))
                .await
                .expect("router answers"),
        )
        .await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let (full, nullifiers) =
            (decoded_all(full.concat().into()), decoded_all(nullifiers.concat().into()));
        let expected: Vec<cf::CompactBlock> = full
            .iter()
            .map(|block| cf::CompactBlock {
                chain_metadata: Some(cf::ChainMetadata::default()),
                vtx: block
                    .vtx
                    .iter()
                    .map(|tx| cf::CompactTx {
                        outputs: Vec::new(),
                        vin: Vec::new(),
                        vout: Vec::new(),
                        actions: tx
                            .actions
                            .iter()
                            .map(|a| cf::CompactOrchardAction {
                                nullifier: a.nullifier.clone(),
                                ..Default::default()
                            })
                            .collect(),
                        ironwood_actions: tx
                            .ironwood_actions
                            .iter()
                            .map(|a| cf::CompactOrchardAction {
                                nullifier: a.nullifier.clone(),
                                ..Default::default()
                            })
                            .collect(),
                        ..tx.clone()
                    })
                    .collect(),
                ..block.clone()
            })
            .collect();
        assert_eq!(nullifiers, expected, "nullifier-only projection of the same four blocks");
        let mut actions = nullifiers.iter().flat_map(|b| &b.vtx).flat_map(|tx| &tx.actions);
        let carried = actions.any(|a| !a.nullifier.is_empty());
        assert!(carried, "fixture carries nullifiers (projection not vacuously empty)");

        // Over the per-range limit: a status, not a short body a wallet would read as the end
        // of the chain.
        let mut capped = unwired(SpyInner::default()).with_compact_block(
            CompactBlockService::new(Served::fixed(store.reader().pin()))
                .with_max_range(std::num::NonZeroU32::new(2).expect("2 is non-zero")),
        );
        let range = proto::BlockRange {
            start: Some(proto::BlockId { height: 1, hash: Vec::new() }),
            end: Some(proto::BlockId { height: 5, hash: Vec::new() }),
            pool_types: Vec::new(),
        };
        let response = capped
            .call(framed_request(path::GET_BLOCK_RANGE, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        // grpc-message = percent-encoded
        let invalid = HeaderValue::from_static("3");
        let named = "requested%205%20blocks,%20the%20per-range%20maximum%20is%202";
        let headers = response.headers();
        let reply = headers.get("grpc-message").and_then(|message| message.to_str().ok());
        assert_eq!((headers.get("grpc-status"), reply), (Some(&invalid), Some(named)));
        assert!(body_of(response).await.is_empty(), "no blocks, never a truncated range");

        // Malformed ranges are refused at the boundary, never reaching the index's asserts.
        let at = |height: u64| Some(proto::BlockId { height, hash: Vec::new() });
        let above_ceiling = |bound: &str, height: u64| {
            format!("range%20{bound}%20{height}%20is%20above%20the%20protocol%20height%20ceiling")
        };
        for (start, end, message) in [
            (None, at(1), "range%20has%20no%20start".to_owned()),
            (at(3), at(1), "range%20start%203%20is%20above%20end%201".to_owned()),
            (at(1), at(1 << 31), above_ceiling("end", 1 << 31)),
            (at(u64::MAX), at(1), above_ceiling("start", u64::MAX)),
        ] {
            let bad = proto::BlockRange { start, end, pool_types: Vec::new() };
            let request = framed_request(path::GET_BLOCK_RANGE, bad.encode_to_vec().into());
            let response = router.call(request).await.expect("router answers");
            let headers = response.headers();
            let reply = headers.get("grpc-message").and_then(|m| m.to_str().ok());
            let got = (headers.get("grpc-status"), reply);
            assert_eq!(got, (Some(&invalid), Some(message.as_str())), "{bad:?}");
        }
    }

    /// Three indexes on one router each answer their own paths and nobody else's, and an index
    /// that is still syncing answers `Unavailable` on every one of them — one code for one
    /// state, rather than a different answer per height.
    #[tokio::test]
    async fn every_index_claims_its_own_paths_and_a_syncing_one_says_retry() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_compact_block::{CompactBlockService, CompactBlockStore};
        use zaino_index_transparent_address::TransparentAddressIndexWriter;
        use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateService, TreeStateStore};
        use zaino_proto::proto::service as proto;
        use zaino_sync::IndexWriter as _;

        let fs = zaino_persistence::fs::SimFs::new();
        let net = zcash_protocol::consensus::NetworkType::Regtest;

        let (compact_synced, compact_synced_rx) = tokio::sync::watch::channel(false);
        let compact_block = CompactBlockService::new(Served::new(
            std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                CompactBlockStore::open(fs.clone(), std::path::Path::new("/compact-block"), net)
                    .expect("open")
                    .reader()
                    .pin(),
            )),
            compact_synced_rx,
        ));
        let tree_state_writer = TreeStateIndexWriter::new(
            TreeStateStore::open(fs.clone(), std::path::Path::new("/tree-state"), net)
                .expect("open"),
        )
        .expect("new");
        // Start unsynced, flip below: the refusal and the answer come from one wiring.
        let (tree_state_synced, tree_state_synced_rx) = tokio::sync::watch::channel(false);
        let (transparent_synced, transparent_synced_rx) = tokio::sync::watch::channel(false);

        let tree_state = TreeStateService::new(
            Served::new(
                std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(tree_state_writer.view())),
                tree_state_synced_rx,
            ),
            net,
        );
        let transparent_writer = TransparentAddressIndexWriter::open(
            fs,
            std::path::Path::new("/transparent-address"),
            net,
        )
        .expect("open");
        let transparent = TransparentAddressService::new(
            Served::new(
                std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(transparent_writer.view())),
                transparent_synced_rx,
            ),
            net,
        );

        let spy = SpyInner::default();
        let reached = spy.reached.clone();
        let mut router = unwired(spy)
            .with_compact_block(compact_block)
            .with_tree_state(tree_state)
            .with_transparent_address(transparent);

        for path in [
            path::GET_LATEST_BLOCK,
            path::GET_BLOCK,
            path::GET_BLOCK_RANGE,
            path::GET_BLOCK_RANGE_NULLIFIERS,
            path::GET_TREE_STATE,
            path::GET_LATEST_TREE_STATE,
            path::GET_SUBTREE_ROOTS,
            path::GET_ADDRESS_UTXOS,
            path::GET_ADDRESS_UTXOS_STREAM,
            path::GET_TADDRESS_BALANCE,
            path::GET_TADDRESS_BALANCE_STREAM,
            path::GET_TADDRESS_TRANSACTIONS,
            path::GET_TADDRESS_TXIDS,
        ] {
            assert!(router.claimed(path).is_some(), "an index claims {path}");
        }

        // chain view unwired: its methods stay with `inner`
        let unclaimed = [
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction",
            "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetMempoolStream",
        ];
        for path in unclaimed {
            assert!(router.claimed(path).is_none(), "not claimed: {path}");
            let _ = router.call(request(path)).await.expect("inner answers");
        }
        assert_eq!(reached.load(std::sync::atomic::Ordering::SeqCst), unclaimed.len());

        // Every claimed path, on all three indexes: UNAVAILABLE (14), never UNIMPLEMENTED and
        // never a per-height answer. The sweep is exhaustive on purpose — a path that forgot the
        // gate would leak a partial index, and only a path-by-path check catches that.
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

    /// A populated t-address index answers all four of its methods from the same rows: the two
    /// utxo shapes agree, the two balance shapes agree, and an address that will not parse is a
    /// bad request rather than an empty answer a gap-limit walk would read as "no history".
    #[tokio::test]
    async fn a_populated_transparent_index_answers_utxos_and_balances_in_both_shapes() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_transparent_address::TransparentAddressIndexWriter;
        use zaino_primitives::types::{
            Block, BlockHeader, Script, Transaction, TransactionId, TransparentData,
            TransparentOutput, Zatoshis,
        };
        use zaino_proto::proto::service as proto;
        use zaino_sync::IndexWriter as _;

        // `t1Hsc…` is hash160 `00…00`, `t3Mg6…` is p2sh `22…22` (base58check, mainnet prefixes).
        const ALICE: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        const BOB: &str = "t3Mg6o2UpMFVtrzqGs7f2VTS6DaiPnFT5rL";
        let alice_script = [&[0x76, 0xa9, 0x14][..], &[0x00; 20], &[0x88, 0xac]].concat();
        let bob_script = [&[0xa9, 0x14][..], &[0x22; 20], &[0x87]].concat();

        let mut writer = TransparentAddressIndexWriter::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/ta"),
            zcash_protocol::consensus::NetworkType::Main,
        )
        .expect("open");

        // Height 0: alice 500 (vout 0), bob 70 (vout 1). Height 1: alice 300.
        let blocks: Vec<std::sync::Arc<Block>> = [
            (0u32, 0x10u8, vec![(alice_script.clone(), 500u64), (bob_script.clone(), 70u64)]),
            (1, 0x11, vec![(alice_script.clone(), 300)]),
        ]
        .into_iter()
        .map(|(height, tag, outputs)| {
            let block = Block::new(
                BlockHeader::for_tests(
                    height,
                    [height as u8; 32],
                    [height.wrapping_sub(1) as u8; 32],
                    1_700_000_000 + height,
                ),
                vec![Transaction {
                    txid: TransactionId::from([tag; 32]),
                    transparent: TransparentData {
                        inputs: Vec::new(),
                        outputs: outputs
                            .into_iter()
                            .map(|(script, value)| TransparentOutput {
                                value: Zatoshis::new(value).expect("in supply"),
                                script: Script::new(script),
                            })
                            .collect(),
                    },
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            );
            std::sync::Arc::new(block)
        })
        .collect();
        zaino_sync::finalize_now(&mut writer, &blocks).await.expect("finalize");

        let mut router = unwired(SpyInner::default()).with_transparent_address(
            TransparentAddressService::new(Served::fixed(writer.view()), MAINNET),
        );

        async fn body_of(response: Response<Body>) -> bytes::Bytes {
            use http_body_util::BodyExt as _;
            response.into_body().collect().await.expect("body").to_bytes()
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

        // GetAddressUtxos: one list message, height-ordered, script and address rebuilt.
        let response = router
            .call(framed_request(
                path::GET_ADDRESS_UTXOS,
                proto::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
                    max_entries: 0,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let list =
            proto::GetAddressUtxosReplyList::decode(&body_of(response).await[FRAME_HEADER..])
                .expect("one framed message");
        let alice_utxos = vec![
            proto::GetAddressUtxosReply {
                address: ALICE.to_owned(),
                txid: vec![0x10; 32],
                index: 0,
                script: alice_script.clone(),
                value_zat: 500,
                height: 0,
            },
            proto::GetAddressUtxosReply {
                address: ALICE.to_owned(),
                txid: vec![0x11; 32],
                index: 0,
                script: alice_script.clone(),
                value_zat: 300,
                height: 1,
            },
        ];
        assert_eq!(list.address_utxos, alice_utxos);

        // maxEntries caps the list, keeping the oldest.
        let response = router
            .call(framed_request(
                path::GET_ADDRESS_UTXOS,
                proto::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
                    max_entries: 1,
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let capped =
            proto::GetAddressUtxosReplyList::decode(&body_of(response).await[FRAME_HEADER..])
                .expect("one framed message");
        assert_eq!(capped.address_utxos, list.address_utxos[..1]);

        // GetAddressUtxosStream: the same records, one framed reply per data frame.
        let response = router
            .call(framed_request(
                path::GET_ADDRESS_UTXOS_STREAM,
                proto::GetAddressUtxosArg {
                    addresses: vec![ALICE.to_owned()],
                    start_height: 0,
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
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let reply = |chunk: &bytes::Bytes| {
            proto::GetAddressUtxosReply::decode(&chunk[FRAME_HEADER..]).expect("framed reply")
        };
        let streamed: Vec<_> = chunks.iter().map(reply).collect();
        assert_eq!(streamed, list.address_utxos, "one reply per record, same as the list shape");

        // GetTaddressBalance over both addresses (alice repeated: counted once), and the
        // client-streaming shape of the same ask.
        let response = router
            .call(framed_request(
                path::GET_TADDRESS_BALANCE,
                proto::AddressList {
                    addresses: vec![ALICE.to_owned(), BOB.to_owned(), ALICE.to_owned()],
                }
                .encode_to_vec()
                .into(),
            ))
            .await
            .expect("router answers");
        let balance = proto::Balance::decode(&body_of(response).await[FRAME_HEADER..]);
        assert_eq!(balance.expect("balance"), proto::Balance { value_zat: 870 });

        let mut streamed = Vec::new();
        for address in [ALICE, BOB] {
            let message = proto::Address { address: address.to_owned() }.encode_to_vec();
            streamed.push(0);
            streamed.extend_from_slice(&(message.len() as u32).to_be_bytes());
            streamed.extend_from_slice(&message);
        }
        let response = router
            .call(
                Request::builder()
                    .uri(format!("http://localhost{}", path::GET_TADDRESS_BALANCE_STREAM))
                    .body(Full::new(bytes::Bytes::from(streamed)))
                    .expect("request"),
            )
            .await
            .expect("router answers");
        let balance = proto::Balance::decode(&body_of(response).await[FRAME_HEADER..]);
        assert_eq!(balance.expect("balance"), proto::Balance { value_zat: 870 }, "= list shape");

        // An unparseable address is a bad request, never a successful empty answer.
        let response = router
            .call(framed_request(
                path::GET_TADDRESS_BALANCE,
                proto::AddressList { addresses: vec!["not-an-address".to_owned()] }
                    .encode_to_vec()
                    .into(),
            ))
            .await
            .expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("3")), "invalid argument");

        // GetTaddressTransactions: index names the txids, the validator port supplies the bytes
        // (raw = txid tag repeated, height = the block that holds it)
        struct TaggedBytes;
        impl crate::validator::FetchRawTransaction for TaggedBytes {
            fn fetch(
                &self,
                txid: TransactionId,
            ) -> Pin<Box<dyn Future<Output = Result<proto::RawTransaction, Status>> + Send + '_>>
            {
                let tag = <[u8; 32]>::from(txid)[0];
                Box::pin(async move {
                    Ok(proto::RawTransaction {
                        data: vec![tag; 3].into(),
                        height: u64::from(tag - 0x10),
                    })
                })
            }
        }
        let mut router = Router::new(
            SpyInner::default(),
            std::sync::Arc::new(TaggedBytes),
            ReadLanes::new(&crate::GrpcLimits::default()),
        )
        .with_transparent_address(TransparentAddressService::new(
            Served::fixed(writer.view()),
            MAINNET,
        ));
        let filter = proto::TransparentAddressBlockFilter {
            address: ALICE.to_owned(),
            range: Some(proto::BlockRange {
                start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                end: Some(proto::BlockId { height: 1, hash: Vec::new() }),
                pool_types: Vec::new(),
            }),
        };

        // TODO: REMOVE the `GetTaddressTxids` half with the deprecated alias
        let mut answers = Vec::new();
        for path in [path::GET_TADDRESS_TRANSACTIONS, path::GET_TADDRESS_TXIDS] {
            let (chunks, trailing) = drained(
                router
                    .call(framed_request(path, filter.encode_to_vec().into()))
                    .await
                    .expect("router answers"),
            )
            .await;
            assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")), "{path}");
            let txs: Vec<proto::RawTransaction> = chunks
                .iter()
                .map(|chunk| proto::RawTransaction::decode(&chunk[FRAME_HEADER..]).expect("tx"))
                .collect();
            answers.push(txs);
        }
        let alice_txs = vec![
            proto::RawTransaction { data: vec![0x10; 3].into(), height: 0 },
            proto::RawTransaction { data: vec![0x11; 3].into(), height: 1 },
        ];
        assert_eq!(answers[0], alice_txs, "both of alice's, height order, validator bytes");
        assert_eq!(answers[0], answers[1], "the deprecated alias answers identically");
    }

    /// Foreign-network encoding of a funded hash160 → `INVALID_ARGUMENT` naming both networks,
    /// on every transparent method (never this network's rows under another network's address)
    #[tokio::test]
    async fn a_foreign_network_address_is_refused_by_every_transparent_method() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_transparent_address::TransparentAddressIndexWriter;
        use zaino_primitives::types::{
            Block, BlockHeader, Script, Transaction, TransactionId, TransparentData,
            TransparentOutput, Zatoshis,
        };
        use zaino_proto::proto::service as proto;
        use zaino_sync::IndexWriter as _;
        use zcash_address::ToAddress as _;
        use zcash_protocol::consensus::NetworkType;

        let mut writer = TransparentAddressIndexWriter::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/ta"),
            NetworkType::Main,
        )
        .expect("open");
        let funded = Block::new(
            BlockHeader::for_tests(0, [0; 32], [0xff; 32], 1_700_000_000),
            vec![Transaction {
                txid: TransactionId::from([0x10; 32]),
                transparent: TransparentData {
                    inputs: Vec::new(),
                    outputs: vec![TransparentOutput {
                        value: Zatoshis::new(500).expect("in supply"),
                        script: Script::new(
                            [&[0x76, 0xa9, 0x14][..], &[0x00; 20], &[0x88, 0xac]].concat(),
                        ),
                    }],
                },
                sprout: Default::default(),
                sapling: Default::default(),
                orchard: Default::default(),
                ironwood: Default::default(),
            }],
        );
        zaino_sync::finalize_now(&mut writer, &[std::sync::Arc::new(funded)])
            .await
            .expect("finalize");

        let mut router = unwired(SpyInner::default()).with_transparent_address(
            TransparentAddressService::new(Served::fixed(writer.view()), MAINNET),
        );

        let mainnet = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
        let testnet =
            zcash_address::ZcashAddress::from_transparent_p2pkh(NetworkType::Test, [0x00; 20])
                .encode();
        let every_method = |address: &str| {
            let range = Some(proto::BlockRange {
                start: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                end: Some(proto::BlockId { height: 0, hash: Vec::new() }),
                pool_types: Vec::new(),
            });
            [
                (
                    path::GET_TADDRESS_BALANCE,
                    proto::AddressList { addresses: vec![address.to_owned()] }.encode_to_vec(),
                ),
                (
                    path::GET_ADDRESS_UTXOS,
                    proto::GetAddressUtxosArg {
                        addresses: vec![address.to_owned()],
                        start_height: 0,
                        max_entries: 0,
                    }
                    .encode_to_vec(),
                ),
                (
                    path::GET_TADDRESS_TRANSACTIONS,
                    proto::TransparentAddressBlockFilter { address: address.to_owned(), range }
                        .encode_to_vec(),
                ),
            ]
        };

        let response = router
            .call(framed_request(
                path::GET_TADDRESS_BALANCE,
                every_method(mainnet)[0].1.clone().into(),
            ))
            .await
            .expect("router answers");
        let status = response.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("0")), "own network's encoding answers");

        for (path, request) in every_method(&testnet) {
            let response =
                router.call(framed_request(path, request.into())).await.expect("router answers");
            let status = Status::from_header_map(response.headers()).expect("a status header");
            let named = format!("address {testnet}: a testnet address, and this index is mainnet");
            let got = (status.code(), status.message());
            assert_eq!(got, (tonic::Code::InvalidArgument, named.as_str()), "{path}");
        }
    }

    /// By height, at the tip, and by hash through the compact locator; every pool its own hex tree
    /// (an absent field would read as `CommitmentTree::empty()`)
    #[tokio::test]
    async fn a_populated_tree_state_index_answers_by_height_at_the_tip_and_by_hash() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_index_tree_state::{TreeStateIndexWriter, TreeStateService, TreeStateStore};
        use zaino_primitives::types::{
            Block, BlockHeader, CompactCiphertext, SaplingData, SaplingOutput, Transaction,
            TransactionId,
        };
        use zaino_proto::proto::service as proto;
        use zaino_sync::IndexWriter as _;

        // Small little-endian value: canonical under both moduli, unlike a repeated-byte filler.
        let mut cmu = [0u8; 32];
        cmu[0] = 7;

        let mut writer = TreeStateIndexWriter::new(
            TreeStateStore::open(
                zaino_persistence::fs::SimFs::new(),
                std::path::Path::new("/ts"),
                zcash_protocol::consensus::NetworkType::Regtest,
            )
            .expect("open"),
        )
        .expect("new");

        let block = Block::new(
            BlockHeader::for_tests(0, [0xab; 32], [0x00; 32], 1_700_000_042),
            vec![Transaction {
                txid: TransactionId::from([0x33; 32]),
                transparent: Default::default(),
                sprout: Default::default(),
                sapling: SaplingData {
                    outputs: vec![SaplingOutput {
                        cmu: cmu.into(),
                        ephemeral_key: [2u8; 32].into(),
                        enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
                    }],
                    ..Default::default()
                },
                orchard: Default::default(),
                ironwood: Default::default(),
            }],
        );
        zaino_sync::finalize_now(&mut writer, &[std::sync::Arc::new(block)])
            .await
            .expect("finalize");

        let service = TreeStateService::new(
            Served::fixed(writer.view()),
            zcash_protocol::consensus::NetworkType::Regtest,
        );
        let mut router = unwired(SpyInner::default()).with_tree_state(service);

        async fn tree_state_of(response: Response<Body>) -> proto::TreeState {
            use http_body_util::BodyExt as _;
            let body = response.into_body().collect().await.expect("body").to_bytes();
            proto::TreeState::decode(&body[FRAME_HEADER..]).expect("one framed message")
        }

        let response = router
            .call(framed_request(
                path::GET_TREE_STATE,
                proto::BlockId { height: 0, hash: Vec::new() }.encode_to_vec().into(),
            ))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), Some(&HeaderValue::from_static("0")));

        let state = tree_state_of(response).await;
        assert_eq!(state.height, 0);
        assert_eq!(state.time, 1_700_000_042);
        // Display order, like every hash-bearing string field on this wire.
        assert_eq!(state.hash, "ab".repeat(32));
        assert_ne!(state.sapling_tree, "000000", "the one commitment landed in sapling");
        assert_eq!(state.orchard_tree, "000000", "a real empty tree, not \"\"");
        assert_eq!(state.ironwood_tree, "000000");

        // Tip = the same answer, asked for differently; framed once per publication, so a second
        // ask is the same allocation
        let latest = || async {
            use http_body_util::BodyExt as _;
            let response = router
                .clone()
                .call(framed_request(path::GET_LATEST_TREE_STATE, Vec::new().into()))
                .await
                .expect("router answers");
            let mut body = std::pin::pin!(response.into_body());
            body.frame().await.expect("a frame").expect("ok").into_data().expect("data")
        };
        let (first, second) = (latest().await, latest().await);
        assert_eq!(proto::TreeState::decode(&first[FRAME_HEADER..]).expect("decodes"), state);
        assert_eq!(first.as_ptr(), second.as_ptr(), "one render per publication, shared");

        // By hash: resolved through the block-hash index, so without it, unimplemented
        let by_hash = |hash: [u8; 32]| {
            framed_request(
                path::GET_TREE_STATE,
                proto::BlockId { height: 0, hash: hash.to_vec() }.encode_to_vec().into(),
            )
        };
        let status = |response: &Response<Body>| {
            Status::from_header_map(response.headers())
                .map(|status| status.code())
                .unwrap_or(tonic::Code::Ok)
        };
        let response = router.call(by_hash([0xab; 32])).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::Unimplemented);

        // Block-hash index holding the same block at 0, and one holding another block there
        // (its chain reorged away from the tree-state index's)
        let locator = |hash: [u8; 32]| async move {
            let mut writer = zaino_internal_block_hash_to_height::BlockHashIndexWriter::new(
                zaino_internal_block_hash_to_height::BlockHashStore::open(
                    zaino_persistence::fs::SimFs::new(),
                    std::path::Path::new("/bh"),
                    zcash_protocol::consensus::NetworkType::Regtest,
                )
                .expect("open"),
            );
            let block = Block::new(
                BlockHeader::for_tests(0, hash, [0x00; 32], 1_700_000_042),
                vec![Transaction {
                    txid: TransactionId::from([0x33; 32]),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: Default::default(),
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            );
            zaino_sync::finalize_now(&mut writer, &[std::sync::Arc::new(block)])
                .await
                .expect("finalize");
            zaino_internal_block_hash_to_height::BlockHashService::new(Served::fixed(writer.view()))
        };

        let mut linked = router.clone().with_block_hash(locator([0xab; 32]).await);
        let response = linked.call(by_hash([0xab; 32])).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::Ok);
        assert_eq!(tree_state_of(response).await, state, "same answer as by height");
        let response = linked.call(by_hash([0xee; 32])).await.expect("answers");
        assert_eq!(status(&response), tonic::Code::NotFound, "a hash no index holds");

        let mut forked = router.with_block_hash(locator([0xcd; 32]).await);
        let response = forked.call(by_hash([0xcd; 32])).await.expect("answers");
        let located = status(&response);
        assert_eq!(located, tonic::Code::NotFound, "located at 0, another block held there");
    }

    /// A body over its method's cap is `RESOURCE_EXHAUSTED` before any decode; one byte under is
    /// decoded as usual (then refused for what it says: a hash is 32 bytes)
    #[tokio::test]
    async fn a_request_body_over_its_cap_is_refused_before_it_is_decoded() {
        use tower::Service as _;
        use zaino_index_compact_block::{CompactBlockService, CompactBlockStore};

        let store = CompactBlockStore::open(
            zaino_persistence::fs::SimFs::new(),
            std::path::Path::new("/cb"),
            zcash_protocol::consensus::NetworkType::Regtest,
        )
        .expect("open");
        let mut router = unwired(SpyInner::default())
            .with_compact_block(CompactBlockService::new(Served::fixed(store.reader().pin())));

        // `BlockID.hash` bytes field: tag + 3-byte varint length + payload
        let block_id = |payload: usize| {
            let mut message = vec![0x12];
            let mut len = payload;
            while len >= 0x80 {
                message.push((len as u8) | 0x80);
                len >>= 7;
            }
            message.push(len as u8);
            message.resize(message.len() + payload, 0);
            bytes::Bytes::from(message)
        };
        let fits = request_limit::MESSAGE - FRAME_HEADER - 4;
        for (payload, code) in [(fits, "3"), (fits + 1, "8")] {
            let response = router
                .call(framed_request(path::GET_BLOCK, block_id(payload)))
                .await
                .expect("router answers");
            let status = response.headers().get("grpc-status");
            assert_eq!(status, Some(&HeaderValue::from_static(code)), "{payload}-byte hash");
        }
    }

    /// One validator's tip + mempool; `ready = false` answers "not synced"
    #[derive(Default)]
    struct NodeState {
        ready: bool,
        tip: u8,
        mempool: std::collections::BTreeMap<[u8; 32], Vec<u8>>,
    }

    #[derive(Default)]
    struct FakeNode(std::sync::Mutex<NodeState>);

    impl zaino_source::GetChainTip for FakeNode {
        async fn get_chain_tip(
            &self,
        ) -> Result<
            (zaino_primitives::types::BlockHash, Height),
            zaino_source::QueryError<zaino_source::GetChainTipError>,
        > {
            let node = self.0.lock().expect("fake node");
            match node.ready {
                true => Ok((
                    [node.tip; 32].into(),
                    Height::try_from(u32::from(node.tip)).expect("small"),
                )),
                false => {
                    Err(zaino_source::QueryError::Domain(zaino_source::GetChainTipError::NotReady))
                }
            }
        }
    }

    impl zaino_source::GetMempoolSourceTip for FakeNode {
        async fn get_mempool_source_tip(
            &self,
        ) -> Result<zaino_source::SourceTip, zaino_source::QueryError<std::convert::Infallible>>
        {
            let tip = self.0.lock().expect("fake node").tip;
            let height = Height::try_from(u32::from(tip)).expect("small");
            Ok(zaino_source::SourceTip { hash: [tip; 32].into(), height, estimated_height: height })
        }
    }

    impl zaino_source::GetMempoolListing for FakeNode {
        async fn get_mempool_listing(
            &self,
        ) -> Result<
            Vec<zaino_source::MempoolListed>,
            zaino_source::QueryError<zaino_source::GetMempoolListingError>,
        > {
            let listed: Vec<_> =
                self.0.lock().expect("fake node").mempool.keys().copied().collect();
            let fee = zaino_primitives::types::Zatoshis::new(1_000).expect("in supply");
            Ok(listed
                .into_iter()
                .map(|txid| zaino_source::MempoolListed { txid: txid.into(), fee })
                .collect())
        }
    }

    impl zaino_source::GetRawMempoolTransaction for FakeNode {
        async fn get_raw_mempool_transaction(
            &self,
            txid: zaino_primitives::types::TransactionId,
        ) -> Result<Vec<u8>, zaino_source::QueryError<zaino_source::GetRawMempoolTransactionError>>
        {
            let node = self.0.lock().expect("fake node");
            let held = node.mempool.get(&<[u8; 32]>::from(txid)).cloned();
            held.ok_or(zaino_source::QueryError::Domain(
                zaino_source::GetRawMempoolTransactionError::NotFound(txid),
            ))
        }
    }

    impl zaino_source::GetPeerInfo for FakeNode {
        async fn get_peer_info(
            &self,
        ) -> Result<
            Vec<zaino_primitives::types::PeerInfo>,
            zaino_source::QueryError<zaino_source::GetPeerInfoError>,
        > {
            Ok(Vec::new())
        }
    }

    impl zaino_source::SendRawTransaction for FakeNode {
        async fn send_raw_transaction(
            &self,
            _: Vec<u8>,
        ) -> Result<
            zaino_primitives::types::TransactionId,
            zaino_source::QueryError<zaino_source::SendRawTransactionError>,
        > {
            unreachable!("the stream relays nothing")
        }
    }

    struct NoProjection;

    impl crate::validator::ProjectCompact for NoProjection {
        fn project(
            &self,
            _: u64,
            _: &[u8],
            _: Option<zaino_primitives::types::Zatoshis>,
        ) -> Result<zaino_proto::proto::compact_formats::CompactTx, Status> {
            unreachable!("the stream projects nothing")
        }
    }

    /// Two wallets on one published view share one rendered snapshot (the same bytes, not
    /// copies); each arrival follows as its own record; a block ends both in `OK` trailers; below
    /// quorum the stream is refused rather than opened silent
    #[tokio::test(start_paused = true)]
    async fn mempool_streams_share_one_rendered_snapshot_then_tail_until_a_block() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service as proto;

        let node = std::sync::Arc::new(FakeNode::default());
        let (view, pollers) = zaino_chainview::ChainView::new(vec![zaino_chainview::Endpoint {
            address: "node:8232".to_owned(),
            source: std::sync::Arc::clone(&node),
        }])
        .expect("one endpoint");
        let reader = view.subscriber();
        let cancel = tokio_util::sync::CancellationToken::new();
        for poller in pollers {
            tokio::spawn(poller.run(cancel.child_token()));
        }
        let mut router = unwired(SpyInner::default()).with_chainview(ChainViewHandles {
            view: reader.clone(),
            relay: std::sync::Arc::new(view),
            compact: std::sync::Arc::new(NoProjection),
        });
        let stream = || framed_request(path::GET_MEMPOOL_STREAM, bytes::Bytes::new());
        // Up to 10 poll rounds (paused clock: instant), until the fold lands
        async fn rounds(until: impl Fn() -> bool) {
            for _ in 0..10 {
                if until() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            panic!("the fold never landed");
        }

        let below = router.call(stream()).await.expect("router answers");
        let status = below.headers().get("grpc-status");
        assert_eq!(status, Some(&HeaderValue::from_static("14")), "no quorum yet: UNAVAILABLE");

        *node.0.lock().expect("fake node") = NodeState {
            ready: true,
            tip: 10,
            mempool: [1u8, 2].map(|seed| ([seed; 32], vec![seed; 300])).into_iter().collect(),
        };
        rounds(|| reader.current().mempool().is_ok_and(|m| m.entries().count() == 2)).await;

        let (first, second) = (
            router.call(stream()).await.expect("router answers"),
            router.call(stream()).await.expect("router answers"),
        );
        let mut first = std::pin::pin!(first.into_body());
        let mut second = std::pin::pin!(second.into_body());
        let next_data = |frame: Option<Result<http_body::Frame<bytes::Bytes>, Status>>| {
            frame.expect("a frame").expect("ok").into_data().expect("data")
        };
        let (snapshot, same) = (next_data(first.frame().await), next_data(second.frame().await));
        assert_eq!(snapshot.as_ptr(), same.as_ptr(), "one render, shared by both subscribers");

        let decoded = |mut chunk: bytes::Bytes| {
            let mut records = Vec::new();
            while !chunk.is_empty() {
                let len = u32::from_be_bytes(chunk[1..5].try_into().expect("header")) as usize;
                let record = proto::RawTransaction::decode(&chunk[5..5 + len]).expect("decodes");
                records.push((record.data.to_vec(), record.height));
                chunk = chunk.slice(5 + len..);
            }
            records
        };
        let expected = vec![(vec![1u8; 300], 0), (vec![2u8; 300], 0)];
        assert_eq!(decoded(snapshot), expected, "the whole mempool, unmined, in txid order");

        node.0.lock().expect("fake node").mempool.insert([3; 32], vec![3; 300]);
        let arrival = next_data(first.frame().await);
        assert_eq!(decoded(arrival), [(vec![3u8; 300], 0)], "the arrival, as its own record");
        assert_eq!(decoded(next_data(second.frame().await)), [(vec![3u8; 300], 0)]);

        node.0.lock().expect("fake node").tip = 11;
        for body in [&mut first, &mut second] {
            let ended = body.frame().await.expect("a frame").expect("ok");
            let trailers = ended.into_trailers().expect("the block ends the stream in trailers");
            assert_eq!(trailers.get("grpc-status"), Some(&HeaderValue::from_static("0")));
            assert!(body.frame().await.is_none(), "nothing after the trailers");
        }
        cancel.cancel();
    }
}
