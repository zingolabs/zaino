//! Compact-block methods: `GetLatestBlock`, `GetBlock`, `GetBlockRange` (stored records, sent as
//! the body)

use bytes::Bytes;
use http::{HeaderValue, Response};
use http_body::Frame;
use http_body_util::StreamBody;
use tonic::{body::Body, Status};
use zaino_index_compact_block::{CompactBlockService, Pools, RangeCursor, ServeError};
use zaino_internal_block_hash_to_height::BlockHashService;
use zaino_persistence::{MapRead, SequenceRead};

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::wire::{self, path, status_response, trailers, unary_response};
use zaino_proto::proto::service as proto;

/// Maps an index error onto a gRPC status, keeping the kinds distinct: a miss is not a bad
/// request, and corruption is not either.
fn to_status(error: ServeError) -> Status {
    match &error {
        // `Unavailable`, not `FailedPrecondition`: clears on its own (retry with backoff)
        ServeError::Syncing | ServeError::Empty => Status::unavailable(error.to_string()),
        ServeError::NotFound { .. } | ServeError::HashNotFound => {
            Status::not_found(error.to_string())
        }
        ServeError::Malformed { .. } => Status::internal(error.to_string()),
    }
}

/// Dispatches a claimed compact-block path.
pub(crate) async fn dispatch<V, B>(
    service: CompactBlockService<V>,
    locator: Option<BlockHashService<V>>,
    path: &str,
    body: B,
    reads: ReadLanes,
) -> Response<Body>
where
    V: SequenceRead + MapRead,
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
fn range_response<V: SequenceRead>(
    cursor: RangeCursor<V>,
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
            // Range lane, per disk step (not per request): a stream reading the non-finalized
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
fn latest<V: SequenceRead>(service: &CompactBlockService<V>) -> Result<Bytes, Status> {
    let (height, hash) = service.latest_id().map_err(to_status)?;
    Ok(wire::frame(&proto::BlockId { height: height.into(), hash: hash.to_vec() }))
}

/// `GetBlock` answers one whole block, every pool included.
///
/// TODO: Deprecate this asymmetry, pending ZIP updates to the light client protocol.
/// `GetBlock` returns every pool while `GetBlockRange` defaults to shielded-only, so the
/// same wallet asking for one block and for a range of one block gets two different
/// answers. `BlockID` has no `poolTypes`, so there is no way to ask for less. Matching
/// lightwalletd is the only reason to keep it.
async fn block<V, B>(
    service: CompactBlockService<V>,
    locator: Option<BlockHashService<V>>,
    body: B,
    reads: &ReadLanes,
) -> Result<Bytes, Status>
where
    V: SequenceRead + MapRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let id: proto::BlockId = wire::decode_request(body).await?;

    // by height at the tip (pepper-sync's reorg check): RAM, answered inline
    if id.hash.is_empty() {
        let height = wire::height(id.height, "height")?;
        if let Some(record) = service.resident_block(height).map_err(to_status)? {
            return Ok(record);
        }
    }

    reads
        .read(Lane::Point, move || {
            // A hash, when given, wins: it names one block across a reorg, a height does not.
            if !id.hash.is_empty() {
                let (height, hash) = wire::locate(locator.as_ref(), &id.hash, "GetBlock")?;
                return service.block_at_hash(height, &hash).map_err(to_status);
            }
            service.block(wire::height(id.height, "height")?).map_err(to_status)
        })
        .await?
}

/// `GetBlockRange`: the wallet-sync path, and the one that has to be cheap.
async fn range<V, B>(service: &CompactBlockService<V>, body: B) -> Result<RangeCursor<V>, Status>
where
    V: SequenceRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: proto::BlockRange = wire::decode_request(body).await?;

    open_range(service, &request, wire::pools(&request.pool_types)?)
}

fn open_range<V: SequenceRead>(
    service: &CompactBlockService<V>,
    request: &proto::BlockRange,
    pools: Pools,
) -> Result<RangeCursor<V>, Status> {
    let start = request
        .start
        .as_ref()
        .map(|id| id.height)
        .ok_or_else(|| Status::invalid_argument("range has no start"))?;
    let end = request
        .end
        .as_ref()
        .map(|id| id.height)
        .ok_or_else(|| Status::invalid_argument("range has no end"))?;
    // start > end = descending (the service walks it top down)
    let (start, end) = (wire::height(start, "range start")?, wire::height(end, "range end")?);

    service.range(start, end, pools).map_err(to_status)
}

// =================================================================================================
// !!! TODO: REMOVE THIS MODULE. DEPRECATED `GetBlockRangeNullifiers`.
// !!! - Served only because pepper-sync still calls it; not part of Zaino's supported surface
// !!! - Delete with its path, its `GrpcService` stub and its proto rpc once pepper-sync
// !!!   requests `GetBlockRange` with `poolTypes`
// =================================================================================================
mod deprecated_nullifiers {
    use bytes::Bytes;
    use prost::Message as _;
    use tonic::Status;
    use zaino_index_compact_block::{CompactBlockService, RangeCursor};
    use zaino_proto::frame::{frame_into, split_frame};
    use zaino_proto::proto::compact_formats as cf;
    use zaino_proto::proto::service as proto;

    use super::open_range;
    use crate::wire::{self, pools};

    /// Proto: MUST ignore a `TRANSPARENT` member (dropped before projection → `[TRANSPARENT]`
    /// alone = empty = shielded default, not "no pools")
    pub(super) async fn range<V, B>(
        service: &CompactBlockService<V>,
        body: B,
    ) -> Result<RangeCursor<V>, Status>
    where
        V: zaino_persistence::SequenceRead,
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let mut request: proto::BlockRange = wire::decode_request(body).await?;
        request.pool_types.retain(|pool| *pool != proto::PoolType::Transparent as i32);

        open_range(service, &request, pools(&request.pool_types)?)
    }

    /// Framed records → framed nullifier-only records, one frame per block
    pub(super) fn reproject(chunk: Bytes) -> Result<Bytes, Status> {
        let mut out = Vec::with_capacity(chunk.len());

        let mut rest = &chunk[..];
        while !rest.is_empty() {
            let (message, tail) = split_frame(rest)
                .ok_or_else(|| Status::internal("stored range chunk is not whole frames"))?;
            let block = nullifiers_only(
                cf::CompactBlock::decode(message).map_err(|e| Status::internal(e.to_string()))?,
            );
            frame_into(&mut out, |out| block.encode_raw(out));
            rest = tail;
        }

        Ok(Bytes::from(out))
    }

    /// Spends whole (a spend = its nullifier record), actions cut to `nullifier`, rest emptied
    fn nullifiers_only(mut block: cf::CompactBlock) -> cf::CompactBlock {
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

#[cfg(test)]
mod tests {
    use http::{HeaderMap, HeaderValue, Response};
    use tonic::{body::Body, Status};
    use zaino_primitives::types::Height;
    use zaino_proto::frame::{split_frame, FRAME_HEADER};
    use zaino_sync::Served;

    use crate::service::Routes;
    use crate::testing::{dispatch, framed_request, routes};
    use crate::wire::path;

    /// `GetBlock` by hash: the block-hash index locates, the compact index answers only where it
    /// holds that same block; no locator wired = `Unimplemented`
    #[tokio::test]
    async fn get_block_by_hash_answers_only_where_the_compact_index_holds_that_block() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use std::sync::Arc;
        use tower::Service as _;
        use zaino_index_compact_block::{testing, CompactBlockService};
        use zaino_internal_block_hash_to_height::{BlockHashIndexWriter, BlockHashService};
        use zaino_persistence::IndexKind;
        use zaino_proto::proto::service as proto;

        use crate::testing::{indexed, store};

        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let compact = testing::committed(store("/cb", &zaino_index_compact_block::schema(net)), 3);
        // Locator's chain = the compact index's through 1, then another chain's block at 2
        let held = |at: u32| <[u8; 32]>::from(testing::block(at).0.header().hash);
        let mut other = zaino_primitives::testing::Chain::new();
        let other_2 = other.extend(other.genesis().hash, 2);
        let located = [testing::block(0).0, testing::block(1).0, other.block(other_2.hash).clone()];
        let located: Vec<_> = located.into_iter().map(Arc::new).collect();
        let schema = zaino_internal_block_hash_to_height::schema(net);
        let batch = std::num::NonZeroUsize::MIN;
        let hashes = BlockHashIndexWriter::new(store("/bh", &schema), batch);
        let hashes_served = hashes.published().served();
        indexed(&located, IndexKind::BlockHash.name(), |queue| hashes.run(queue)).await;
        let other_2 = <[u8; 32]>::from(other_2.hash);

        let service = CompactBlockService::new(Served::fixed(compact));
        let unlocated = dispatch(Routes { compact_block: service.clone(), ..routes() });
        let locator = BlockHashService::new(Served::fixed((*hashes_served.pin_any()).clone()));
        let mut router = dispatch(Routes {
            compact_block: service.clone(),
            block_hash: Some(locator),
            ..routes()
        });
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

        let response = router.call(request(held(1).to_vec())).await.expect("answers");
        assert_eq!(code(&response), tonic::Code::Ok);
        let by_height = service.block(Height::try_from(1).expect("height")).expect("by height");
        let body = response.into_body().collect().await.expect("body").to_bytes();
        assert_eq!(body, by_height, "same stored record as by height");

        let mut codes = Vec::new();
        for (mut router, hash) in [
            (router.clone(), vec![0xee; 32]),
            (router.clone(), other_2.to_vec()),
            (router.clone(), held(1)[..31].to_vec()),
            (unlocated, held(1).to_vec()),
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
        use zaino_index_compact_block::{testing, CompactBlockService};
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        let net = zcash_protocol::consensus::NetworkType::Regtest;
        let schema = zaino_index_compact_block::schema(net);
        let committed = testing::committed(crate::testing::store("/cb", &schema), 6);
        let service = CompactBlockService::new(Served::fixed(committed));
        let mut router = dispatch(Routes { compact_block: service, ..routes() });

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
        let mut rest = &body[..];
        let mut heights = Vec::new();
        while !rest.is_empty() {
            let (message, tail) = split_frame(rest).expect("whole frame");
            let block = cf::CompactBlock::decode(message).expect("framed message");

            assert!(block.vtx[0].vin.is_empty(), "default prunes transparent");
            assert_eq!(block.vtx[0].actions.len(), 1, "shielded kept");
            heights.push(block.height);
            rest = tail;
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
            let mut rest = &body[..];
            let mut blocks = Vec::new();
            while !rest.is_empty() {
                let (message, tail) = split_frame(rest).expect("whole frame");
                blocks.push(cf::CompactBlock::decode(message).expect("framed message"));
                rest = tail;
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

        // start > end = descending (proto: "decreasing height order"), same records reversed
        let (descending, trailing) = drained(
            router
                .call(framed_request(
                    path::GET_BLOCK_RANGE,
                    proto::BlockRange {
                        start: Some(proto::BlockId { height: 4, hash: Vec::new() }),
                        end: Some(proto::BlockId { height: 1, hash: Vec::new() }),
                        pool_types: Vec::new(),
                    }
                    .encode_to_vec()
                    .into(),
                ))
                .await
                .expect("router answers"),
        )
        .await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let reversed: Vec<cf::CompactBlock> = full.iter().rev().cloned().collect();
        assert_eq!(decoded_all(descending.concat().into()), reversed, "4..=1 = 1..=4 reversed");

        // Malformed ranges are refused at the boundary, never reaching the index's asserts.
        // grpc-message = percent-encoded
        let invalid = HeaderValue::from_static("3");
        let at = |height: u64| Some(proto::BlockId { height, hash: Vec::new() });
        let above_ceiling = |bound: &str, height: u64| {
            format!("range%20{bound}%20{height}%20is%20above%20the%20protocol%20height%20ceiling")
        };
        for (start, end, message) in [
            (None, at(1), "range%20has%20no%20start".to_owned()),
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
}
