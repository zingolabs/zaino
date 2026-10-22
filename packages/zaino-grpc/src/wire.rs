//! gRPC wire shared by every route: framing, bounded request decoding, response shapes

use std::fmt;

use http::{HeaderMap, HeaderValue, Response};
use tonic::{body::Body, Status};
use zaino_proto::frame::{frame_into, split_frame, FRAME_HEADER};

use crate::service::Method;

/// No answer from this `GrpcSnapshot`: retry (`UNAVAILABLE`, the reason as its message)
pub(crate) fn unavailable(why: zaino_grpc_snapshot::Unavailable) -> Status {
    Status::unavailable(why.to_string())
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

/// Request body caps (gRPC framing included)
///
/// - over one = `RESOURCE_EXHAUSTED` (= tonic's own)
/// - collected once by the dispatch, before the handler (tonic's 4 MiB default never applies)
pub(super) mod request_limit {
    /// Every claimed request but a transaction: ids, ranges, address lists (~1.5k t-addresses,
    /// clients send tens)
    pub(crate) const MESSAGE: usize = 64 * 1024;

    /// `SendTransaction`: a block's worth + framing + the height field
    pub(crate) const TRANSACTION: usize = zaino_primitives::protocol::MAX_BLOCK_BYTES + 1024;

    /// Whole-body deadline (a trickling peer holds its stream permit)
    pub(super) const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
}

/// Unary response, body = one already-framed record, then `OK` trailers
///
/// - never `grpc-status` in headers beside a body (= Trailers-Only, body forbidden)
/// - grpc-go, grpc-java, grpcio fail it "closed the stream without sending trailers" (tonic
///   tolerates it)
pub(super) fn unary_response(record: bytes::Bytes) -> Response<Body> {
    streamed_response(vec![record])
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

/// `method`'s whole request body, refused past its cap (checked as it arrives, never buffered
/// beyond) or past [`request_limit::DEADLINE`]
pub(crate) async fn collect_request<B>(body: B, method: Method) -> Result<bytes::Bytes, Status>
where
    B: http_body::Body,
    B::Error: fmt::Display,
{
    use http_body_util::{BodyExt as _, LengthLimitError, Limited};

    let limit = match method {
        Method::SendTransaction => request_limit::TRANSACTION,
        _ => request_limit::MESSAGE,
    };
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

/// Unary request message (exactly one frame)
pub(crate) fn decode<M: prost::Message + Default>(body: &[u8]) -> Result<M, Status> {
    match split_frame(body) {
        Some((message, [])) => decode_message(message),
        _ => Err(Status::invalid_argument("request body is not one whole frame")),
    }
}

/// Client-streaming request: every frame's message (sole user `GetTaddressBalanceStream` answers
/// one total: nothing to emit before the last address)
pub(crate) fn decode_each<M: prost::Message + Default>(body: &[u8]) -> Result<Vec<M>, Status> {
    let messages =
        messages(body).ok_or_else(|| Status::invalid_argument("request body ends mid-frame"))?;
    messages.into_iter().map(decode_message).collect()
}

fn decode_message<M: prost::Message + Default>(message: &[u8]) -> Result<M, Status> {
    M::decode(message).map_err(|error| Status::invalid_argument(error.to_string()))
}

/// Each frame's message, in order (`None` = ends mid-frame)
pub(crate) fn messages(mut framed: &[u8]) -> Option<Vec<&[u8]>> {
    let mut messages = Vec::new();
    while !framed.is_empty() {
        let (message, rest) = split_frame(framed)?;
        messages.push(message);
        framed = rest;
    }
    Some(messages)
}

#[cfg(test)]
mod tests {
    use tonic::Code;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_persistence::IndexKind;
    use zaino_proto::frame::FRAME_HEADER;

    use super::request_limit;
    use crate::service::Method;
    use crate::testing::{
        answer, dispatch, fixed_snapshots, framed_request, indexed, routes_serving, MAINNET,
    };

    /// At the cap → decoded as usual, then refused for what it says (a hash = 32 bytes)
    #[tokio::test]
    async fn a_request_body_over_its_cap_is_refused_before_it_is_decoded() {
        use tower::Service as _;

        let chain = zaino_source::testing::MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let views = vec![indexed(IndexKind::CompactBlock, &chain, genesis)];
        let snapshots =
            fixed_snapshots(&chain, genesis, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::CompactBlock], snapshots));

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
        for (payload, code) in [(fits, Code::InvalidArgument), (fits + 1, Code::ResourceExhausted)]
        {
            let response = router.call(framed_request(Method::GetBlock, block_id(payload))).await;

            let (_, status) = answer(response.expect("infallible")).await;
            assert_eq!(
                status.code(),
                code,
                "{payload}-byte hash, cap {}: {status:?}",
                request_limit::MESSAGE
            );
        }
    }
}
