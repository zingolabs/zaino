//! gRPC wire shared by every route: method paths, framing, bounded request decoding, response
//! shapes, client-field parsing

use std::fmt;

use http::{HeaderMap, HeaderValue, Response};
use http_body_util::Full;
use tonic::{body::Body, Status};
use zaino_index_compact_block::Pools;
use zaino_primitives::types::Height;
use zaino_proto::frame::{frame_into, split_frame, FRAME_HEADER};

/// Every `CompactTxStreamer` method path, as `/{service}/{method}`, grouped by what answers it
pub(crate) mod path {
    pub(crate) const GET_LIGHTD_INFO: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLightdInfo";
    pub(crate) const GET_TRANSACTION: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTransaction";

    pub(crate) const GET_LATEST_BLOCK: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestBlock";
    pub(crate) const GET_BLOCK: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlock";
    pub(crate) const GET_BLOCK_RANGE: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange";
    /// TODO: REMOVE THIS — deprecated alias of `GET_BLOCK_RANGE` (pepper-sync still calls it)
    pub(crate) const GET_BLOCK_RANGE_NULLIFIERS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRangeNullifiers";

    pub(crate) const GET_TREE_STATE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTreeState";
    pub(crate) const GET_LATEST_TREE_STATE: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetLatestTreeState";
    pub(crate) const GET_SUBTREE_ROOTS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetSubtreeRoots";

    pub(crate) const GET_ADDRESS_UTXOS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetAddressUtxos";
    pub(crate) const GET_ADDRESS_UTXOS_STREAM: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetAddressUtxosStream";
    pub(crate) const GET_TADDRESS_BALANCE: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressBalance";
    pub(crate) const GET_TADDRESS_BALANCE_STREAM: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressBalanceStream";

    /// Both halves of the boundary: index names the txs, validator holds the bytes
    /// (`docs/design/boundaries.md`)
    pub(crate) const GET_TADDRESS_TRANSACTIONS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressTransactions";
    /// TODO: REMOVE THIS — deprecated alias of `GET_TADDRESS_TRANSACTIONS` (pepper-sync still
    /// calls it)
    pub(crate) const GET_TADDRESS_TXIDS: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetTaddressTxids";

    /// One random entry per attempt, watched until it spreads (`chainview.md` §6); marked ours on
    /// acceptance (a wallet sees its own send before any listing)
    pub(crate) const SEND_TRANSACTION: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/SendTransaction";
    pub(crate) const GET_MEMPOOL_TX: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetMempoolTx";
    pub(crate) const GET_MEMPOOL_STREAM: &str =
        "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetMempoolStream";
}

/// No answer from this snapshot: retry (`UNAVAILABLE`, the reason as its message)
pub(crate) fn unavailable(why: zaino_snapshot::Unavailable) -> Status {
    Status::unavailable(why.to_string())
}

/// Client height → [`Height`] (past the protocol ceiling = the client's error, never a service's)
pub(super) fn height(raw: u64, field: &str) -> Result<Height, Status> {
    Height::try_from(raw).map_err(|_| {
        Status::invalid_argument(format!("{field} {raw} is above the protocol height ceiling"))
    })
}

/// `BlockID.hash` → `(height, hash)` via `at`'s block-hash index, at or below its tip
///
/// - height only: the answering index confirms it holds `hash` there (indexes fold one block at
///   a time; a test may pair views of different chains)
pub(super) fn locate<V: zaino_persistence::MapRead>(
    at: &zaino_nfs::At<V>,
    raw: &[u8],
    method: &str,
) -> Result<(Height, [u8; 32]), Status> {
    let hash: [u8; 32] =
        raw.try_into().map_err(|_| Status::invalid_argument("block hash must be 32 bytes"))?;
    let locator = at.views().block_hash().ok_or_else(|| {
        let by = format!("{method} by hash resolves through the block-hash index");
        match at.views().syncing(zaino_persistence::IndexKind::BlockHash) {
            true => Status::unavailable(format!("{by}, which is syncing")),
            false => Status::unimplemented(format!("{by}, which is off")),
        }
    })?;
    let height = locator.height_of(&hash.into()).filter(|&height| height <= at.tip().height);
    let missing = || Status::not_found("block hash is not in the index");
    Ok((height.ok_or_else(missing)?, hash))
}

/// Client `poolTypes` → [`Pools`] (empty = shielded default; unknown / `POOL_TYPE_INVALID` refused)
pub(super) fn pools(raw: &[i32]) -> Result<Pools, Status> {
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

/// `(start, end)`, both inclusive, refused when reversed (services take ordered ranges)
pub(super) fn ordered(start: Height, end: Height) -> Result<(Height, Height), Status> {
    match start <= end {
        true => Ok((start, end)),
        false => Err(Status::invalid_argument(format!("range start {start} is above end {end}"))),
    }
}

/// `message` → its wire frame
pub(super) fn frame<M: prost::Message>(message: &M) -> bytes::Bytes {
    frame_all(std::slice::from_ref(message))
}

/// `messages` framed back to back into one buffer (one allocation, one DATA chunk)
pub(super) fn frame_all<M: prost::Message>(messages: &[M]) -> bytes::Bytes {
    let total = messages.iter().map(|message| FRAME_HEADER + message.encoded_len()).sum();
    let mut framed = Vec::with_capacity(total);
    for message in messages {
        frame_into(&mut framed, |out| message.encode_raw(out));
    }
    bytes::Bytes::from(framed)
}

/// Request body caps (gRPC framing included); over one = `RESOURCE_EXHAUSTED` (= tonic's own)
///
/// - claimed paths collect their own bodies (tonic's 4 MiB default never applies)
pub(super) mod request_limit {
    /// Every claimed request but a transaction: ids, ranges, address lists (~1.5k t-addresses;
    /// clients send tens)
    pub(crate) const MESSAGE: usize = 64 * 1024;

    /// `SendTransaction`: a block's worth + framing + the height field
    pub(crate) const TRANSACTION: usize = zaino_primitives::protocol::MAX_BLOCK_BYTES + 1024;

    /// Whole-body deadline (a trickling peer holds its stream permit)
    pub(super) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
}

/// Unary response, body = one already-framed record
///
/// - `grpc-status` in headers (legal: body complete when headers written → one write, the point
///   of wire-shaped records; streams can't, [`streamed_response`])
pub(super) fn unary_response(record: bytes::Bytes) -> Response<Body> {
    let mut response = Response::new(Body::new(Full::new(record)));

    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/grpc"));
    response.headers_mut().insert("grpc-status", HeaderValue::from_static("0"));

    response
}

/// Final `grpc-status` trailers, as tonic's encoder emits them
///
/// - non-header-safe message dropped, never the trailer (no `grpc-status` = truncated stream)
pub(super) fn trailers(status: &Status) -> HeaderMap {
    let mut map = HeaderMap::new();

    if status.add_header(&mut map).is_err() {
        map.clear();
        let _ = Status::new(status.code(), "").add_header(&mut map);
    }

    map
}

/// Server-streaming response over materialised framed records
///
/// - `grpc-status` in trailers only (one in headers = complete before the body arrives)
pub(super) fn streamed_response(records: Vec<bytes::Bytes>) -> Response<Body> {
    use futures::StreamExt as _;
    use http_body::Frame;

    let data = futures::stream::iter(records.into_iter().map(|record| Ok(Frame::data(record))));
    let trailing = futures::stream::once(async {
        Ok::<_, Status>(Frame::trailers(trailers(&Status::ok(""))))
    });

    streaming(data.chain(trailing))
}

/// Server-streaming response, ready records joined into full DATA frames
/// ([`Coalesced`](crate::coalesce::Coalesced): one frame per record floods h2 clients)
pub(super) fn streaming<S>(frames: S) -> Response<Body>
where
    S: futures::Stream<Item = Result<http_body::Frame<bytes::Bytes>, Status>> + Send + 'static,
{
    let frames = Body::new(http_body_util::StreamBody::new(frames));
    let mut response = Response::new(Body::new(crate::coalesce::Coalesced::new(frames)));
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

/// Unary request message (body <= [`request_limit::MESSAGE`])
pub(super) async fn decode_request<M, B>(body: B) -> Result<M, Status>
where
    M: prost::Message + Default,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    decode_request_within(body, request_limit::MESSAGE).await
}

/// Unary request message (body <= `limit` bytes, exactly one frame)
pub(super) async fn decode_request_within<M, B>(body: B, limit: usize) -> Result<M, Status>
where
    M: prost::Message + Default,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let collected = collect_limited(body, limit).await?;

    let message = match split_frame(&collected) {
        Some((message, [])) => message,
        _ => return Err(Status::invalid_argument("request body is not one whole frame")),
    };

    M::decode(message).map_err(|error| Status::invalid_argument(error.to_string()))
}

/// Every frame of a client-streaming body, collected (sole user `GetTaddressBalanceStream`
/// answers one total: nothing to emit before the last address)
pub(super) async fn decode_request_stream<M, B>(body: B) -> Result<Vec<M>, Status>
where
    M: prost::Message + Default,
    B: http_body::Body,
    B::Error: fmt::Display,
{
    let collected = collect_limited(body, request_limit::MESSAGE).await?;

    let mut messages = Vec::new();
    let mut rest = &collected[..];
    while !rest.is_empty() {
        let (message, tail) = split_frame(rest)
            .ok_or_else(|| Status::invalid_argument("request body ends mid-frame"))?;
        messages.push(M::decode(message).map_err(|e| Status::invalid_argument(e.to_string()))?);
        rest = tail;
    }

    Ok(messages)
}

pub(super) fn status_response(status: Status) -> Response<Body> {
    status.into_http()
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use zaino_persistence::IndexKind;
    use zaino_proto::frame::FRAME_HEADER;

    use super::{path, request_limit};
    use crate::service::Routes;
    use crate::testing::{dispatch, framed_request, indexed, routes, snapshot, MAINNET};

    /// A body over its method's cap is `RESOURCE_EXHAUSTED` before any decode; one byte under is
    /// decoded as usual (then refused for what it says: a hash is 32 bytes)
    #[tokio::test]
    async fn a_request_body_over_its_cap_is_refused_before_it_is_decoded() {
        use tower::Service as _;

        let chain = zaino_primitives::testing::MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let compact = indexed(IndexKind::CompactBlock, &chain, genesis);
        let snapshots = snapshot(&chain, genesis, vec![compact]);
        let mut router = dispatch(Routes { snapshots, ..routes() });

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
}
