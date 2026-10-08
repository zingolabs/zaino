//! Compact-block methods: `GetLatestBlock` (the snapshot's tip), `GetBlock`, `GetBlockRange`
//! (stored records, sent as the body, at heights `<=` [`At::answers_through`])

use bytes::Bytes;
use http::Response;
use http_body::Frame;
use tonic::{body::Body, Status};
use zaino_index_compact_block::{CompactBlockReader, Pools, RangeCursor, ServeError};
use zaino_nfs::At;
use zaino_persistence::{IndexKind, MapRead, OverlayView, SequenceRead};
use zaino_primitives::types::Height;

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::wire::{self, path, status_response, trailers, unary_response};
use zaino_proto::proto::service as proto;

/// One snapshot's compact-block records
type Blocks<V> = CompactBlockReader<OverlayView<V>>;

/// Kinds kept distinct (miss != bad request != corruption)
fn to_status(error: ServeError) -> Status {
    match &error {
        ServeError::NotFound { .. } | ServeError::HashNotFound => {
            Status::not_found(error.to_string())
        }
        ServeError::Malformed { .. } => Status::internal(error.to_string()),
    }
}

pub(crate) async fn dispatch<V, B>(
    at: &At<V>,
    blocks: Blocks<V>,
    path: &str,
    body: B,
    reads: ReadLanes,
) -> Response<Body>
where
    V: SequenceRead + MapRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let through = at.answers_through(IndexKind::CompactBlock);
    let answer = match path {
        path::GET_LATEST_BLOCK => Ok(unary_response(latest(at))),
        path::GET_BLOCK => block(at, blocks, body, &reads).await.map(unary_response),
        path::GET_BLOCK_RANGE => {
            let range = range(blocks, through, body).await;
            range.map(|walk| range_response(walk, reads, Ok))
        }
        path::GET_BLOCK_RANGE_NULLIFIERS => deprecated_nullifiers::range(blocks, through, body)
            .await
            .map(|walk| range_response(walk, reads, deprecated_nullifiers::reproject)),
        _ => Err(Status::unimplemented("not a compact-block method")),
    };

    match answer {
        Ok(response) => response,
        Err(status) => status_response(status),
    }
}

/// One range's cursor + the status its stream ends in once the cursor is spent
///
/// - `ends` = `OUT_OF_RANGE` when the range reaches past the compact-block tip (lightwalletd: lazily
///   to the tip, then the error; an early `OK` end hangs the iOS downloader for good)
struct Walk<V> {
    cursor: RangeCursor<V>,
    ends: Status,
}

/// Server-streaming response, cursor chunk by chunk
///
/// - `grpc-status` in trailers only (headers precede the walk; a status there = response done)
/// - `reproject` = per-chunk rewrite (`Ok` for `GetBlockRange`, so its records stay slices)
fn range_response<V: SequenceRead>(
    walk: Walk<V>,
    reads: ReadLanes,
    reproject: fn(Bytes) -> Result<Bytes, Status>,
) -> Response<Body> {
    let Walk { cursor, ends } = walk;
    let frames = futures::stream::unfold(Some((cursor, reads, ends)), move |state| async move {
        let (mut cursor, reads, ends) = state?;

        // - Blocking pool for disk steps only (cold mmap refill ~11.7 ms would stall the worker)
        // - In-memory steps inline (else a task per block on the highest-volume RPC)
        let (cursor, chunk) = if cursor.next_touches_disk() {
            // - Range lane per disk step, not per request (layer reads never queue)
            // - Err = runtime shutting down → stream ends
            let stepped = reads.read(Lane::Range, move || {
                let chunk = cursor.next_chunk();
                (cursor, chunk)
            });
            stepped.await.ok()?
        } else {
            let chunk = cursor.next_chunk();
            (cursor, chunk)
        };

        let last = match chunk.map(|read| read.map_err(to_status).and_then(reproject)) {
            Some(Ok(chunk)) => return Some((Ok(Frame::data(chunk)), Some((cursor, reads, ends)))),
            Some(Err(status)) => trailers(&status),
            None => trailers(&ends),
        };

        Some((Ok::<_, Status>(Frame::trailers(last)), None))
    });

    // layer steps = one record each, ready back to back: joined per DATA frame
    wire::streaming(frames)
}

/// `GetLatestBlock` answers a `BlockID`, not a block: the snapshot's tip (no read at all)
fn latest<V>(at: &At<V>) -> Bytes {
    let tip = at.tip();
    let hash = <[u8; 32]>::from(tip.hash).to_vec();
    wire::frame(&proto::BlockId { height: tip.height.into(), hash })
}

/// `GetBlock`: one whole block, every pool
///
/// - TODO: deprecate pending light-client ZIP updates (`GetBlockRange` defaults shielded-only:
///   one block != range of one; `BlockID` has no `poolTypes`; kept = lightwalletd parity)
async fn block<V, B>(
    at: &At<V>,
    blocks: Blocks<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Bytes, Status>
where
    V: SequenceRead + MapRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let id: proto::BlockId = wire::decode_request(body).await?;

    // Hash wins when given (names one block across a reorg; a height doesn't)
    if !id.hash.is_empty() {
        let through = at.answers_through(IndexKind::CompactBlock);
        let (height, hash) = wire::locate(at, through, &id.hash, "GetBlock")?;
        let read = move || blocks.block_at(height, &hash).map_err(to_status);
        return reads.read(Lane::Point, read).await?;
    }

    let height = wire::height(id.height, "height")?;
    let missing = move || to_status(ServeError::NotFound { height });
    if height > at.answers_through(IndexKind::CompactBlock) {
        return Err(missing());
    }
    // the snapshot's layer (pepper-sync's reorg check at the tip): RAM, answered inline
    if let Some(record) = blocks.resident_block(height) {
        return Ok(record);
    }
    reads.read(Lane::Point, move || blocks.block(height).ok_or_else(missing)).await?
}

/// `GetBlockRange` (wallet-sync path: must stay cheap)
async fn range<V, B>(blocks: Blocks<V>, through: Height, body: B) -> Result<Walk<V>, Status>
where
    V: SequenceRead,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: proto::BlockRange = wire::decode_request(body).await?;

    open_range(blocks, through, &request, wire::pools(&request.pool_types)?)
}

/// - walk order up to the first height past `through`: a first height past it = refused before
///   any block (lightwalletd: the first `getblock` fails)
fn open_range<V: SequenceRead>(
    blocks: Blocks<V>,
    through: Height,
    request: &proto::BlockRange,
    pools: Pools,
) -> Result<Walk<V>, Status> {
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
    // start > end = descending (the cursor walks it top down)
    let (start, end) = (wire::height(start, "range start")?, wire::height(end, "range end")?);

    let past = |height| {
        Status::out_of_range(format!("block {height} is above the compact-block tip {through}"))
    };
    if start > through {
        return Err(past(start));
    }
    let ends = if end > through { past(through.next()) } else { Status::ok("") };
    let cursor = RangeCursor::new(blocks, start, end, through, pools).map_err(to_status)?;

    Ok(Walk { cursor, ends })
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
    use zaino_primitives::types::Height;
    use zaino_proto::frame::{frame_into, split_frame};
    use zaino_proto::proto::compact_formats as cf;
    use zaino_proto::proto::service as proto;

    use super::{open_range, Blocks, Walk};
    use crate::wire::{self, pools};

    /// Proto: MUST ignore a `TRANSPARENT` member (dropped before projection → `[TRANSPARENT]`
    /// alone = empty = shielded default, not "no pools")
    pub(super) async fn range<V, B>(
        blocks: Blocks<V>,
        through: Height,
        body: B,
    ) -> Result<Walk<V>, Status>
    where
        V: zaino_persistence::SequenceRead,
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let mut request: proto::BlockRange = wire::decode_request(body).await?;
        request.pool_types.retain(|pool| *pool != proto::PoolType::Transparent as i32);

        open_range(blocks, through, &request, pools(&request.pool_types)?)
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
    use zaino_persistence::IndexKind;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_proto::frame::{split_frame, FRAME_HEADER};

    use crate::service::Routes;
    use crate::testing::{dispatch, framed_request, indexed, routes, snapshot, MAINNET};
    use crate::wire::path;

    /// `GetBlock` by hash: the block-hash index locates, the compact index answers only where it
    /// holds that same block; no locator wired = `Unimplemented`
    #[tokio::test]
    async fn get_block_by_hash_answers_only_where_the_compact_index_holds_that_block() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service as proto;

        // compact index over 0..=2; locator over 0, 1, then a sibling at 2
        let mut chain = MockChain::regtest().network(MAINNET);
        let two = chain.mine_empty(2);
        let other_2 = chain.fork(h(1)).mine_empty(1).tip();
        let held = |at: u32| <[u8; 32]>::from(chain.at(h(at)).hash);
        let locator = indexed(IndexKind::BlockHash, &chain, other_2);
        let other_2 = <[u8; 32]>::from(other_2.hash);

        let compact = indexed(IndexKind::CompactBlock, &chain, two);
        let unlocated = dispatch(Routes {
            snapshots: snapshot(&chain, two, vec![compact.clone()]),
            ..routes()
        });
        let mut router = dispatch(Routes {
            snapshots: snapshot(&chain, two, vec![compact, locator]),
            ..routes()
        });
        let request = |hash: Vec<u8>, height: u64| {
            framed_request(path::GET_BLOCK, proto::BlockId { height, hash }.encode_to_vec().into())
        };
        let code = |response: &Response<Body>| {
            Status::from_header_map(response.headers())
                .map(|status| status.code())
                .unwrap_or(tonic::Code::Ok)
        };

        let response = router.call(request(held(1).to_vec(), 0)).await.expect("answers");
        assert_eq!(code(&response), tonic::Code::Ok);
        let by_hash = response.into_body().collect().await.expect("body").to_bytes();
        let response = router.call(request(Vec::new(), 1)).await.expect("answers");
        let by_height = response.into_body().collect().await.expect("body").to_bytes();
        assert_eq!(by_hash, by_height, "same stored record as by height");

        let mut codes = Vec::new();
        for (mut router, hash) in [
            (router.clone(), vec![0xee; 32]),
            (router.clone(), other_2.to_vec()),
            (router.clone(), held(1)[..31].to_vec()),
            (unlocated, held(1).to_vec()),
        ] {
            codes.push(code(&router.call(request(hash, 0)).await.expect("answers")));
        }
        use tonic::Code::{InvalidArgument, NotFound, Unimplemented};
        let expected = [NotFound, NotFound, InvalidArgument, Unimplemented];
        assert_eq!(codes, expected, "unknown hash, other block at 2, short hash, no locator");
    }

    /// W4 over a snapshot served at 5: a range reaching past the tip streams every block to the tip
    /// in walk order, then `OUT_OF_RANGE` in the trailers, never an early `OK` end (iOS hangs on
    /// one); a first height past the tip = `OUT_OF_RANGE` before any block (trailers-only)
    #[tokio::test]
    async fn get_block_range_past_the_tip_streams_to_it_then_ends_out_of_range() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        let mut chain = MockChain::regtest().network(MAINNET);
        let tip = chain.mine_empty(5);
        let compact = indexed(IndexKind::CompactBlock, &chain, tip);
        let mut router =
            dispatch(Routes { snapshots: snapshot(&chain, tip, vec![compact]), ..routes() });
        let past = |h: u32| format!("block%20{h}%20is%20above%20the%20compact-block%20tip%205");
        let cases = [
            (path::GET_BLOCK_RANGE, (1, 4), (vec![1, 2, 3, 4], "0", None)),
            (path::GET_BLOCK_RANGE, (5, 1), (vec![5, 4, 3, 2, 1], "0", None)),
            (path::GET_BLOCK_RANGE, (3, 9), (vec![3, 4, 5], "11", Some(past(6)))),
            (path::GET_BLOCK_RANGE_NULLIFIERS, (3, 9), (vec![3, 4, 5], "11", Some(past(6)))),
            (path::GET_BLOCK_RANGE, (9, 3), (vec![], "11", Some(past(9)))),
            (path::GET_BLOCK_RANGE, (7, 9), (vec![], "11", Some(past(7)))),
        ];
        for (path, (start, end), expected) in cases {
            let range = proto::BlockRange {
                start: Some(proto::BlockId { height: start, hash: Vec::new() }),
                end: Some(proto::BlockId { height: end, hash: Vec::new() }),
                pool_types: Vec::new(),
            };
            let request = framed_request(path, range.encode_to_vec().into());
            let response = router.call(request).await.expect("router answers");
            let headers = response.headers().clone();
            let body = response.into_body().collect().await.expect("body");
            let status = body.trailers().cloned().unwrap_or(headers);
            let text =
                |name: &str| status.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
            let bytes = body.to_bytes();
            let mut rest = &bytes[..];
            let mut heights = Vec::new();
            while !rest.is_empty() {
                let (message, tail) = split_frame(rest).expect("whole frame");
                heights.push(cf::CompactBlock::decode(message).expect("a block").height);
                rest = tail;
            }
            let code = text("grpc-status").expect("a status");
            let got = (heights, code.as_str(), text("grpc-message"));
            assert_eq!(got, (expected.0, expected.1, expected.2), "{path} {start}..={end}");
        }
    }

    /// Blocks 1..=5: a coinbase paying alice, then one tx spending it into every pool (transparent
    /// in + out, a sapling spend + output, one orchard, two ironwood actions); body = stored bytes
    #[tokio::test]
    async fn get_block_and_get_block_range_answer_from_stored_records() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::testing::{outpoint, p2pkh};
        use zaino_proto::proto::compact_formats as cf;
        use zaino_proto::proto::service as proto;

        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=5u8 {
            let leaf = u32::from(at);
            chain.mine(|b| {
                b.coinbase(|c| c.txid([at; 32]).pay(&alice, 17_345)).tx(|t| {
                    t.spend(outpoint([at; 32], 0))
                        .pay(&alice, 12_345)
                        .fee(5_000)
                        .sapling_spend([0x30 + at; 32])
                        .sapling_output(leaf)
                        .orchard_action([0x70 + at; 32], leaf)
                        .ironwood_action([0x80 + at; 32], 2 * leaf)
                        .ironwood_action([0x90 + at; 32], 2 * leaf + 1)
                })
            });
        }
        let tip = chain.tip();
        let compact = indexed(IndexKind::CompactBlock, &chain, tip);
        let mut router =
            dispatch(Routes { snapshots: snapshot(&chain, tip, vec![compact]), ..routes() });

        async fn body_of(response: Response<Body>) -> bytes::Bytes {
            response.into_body().collect().await.expect("body").to_bytes()
        }

        let id = proto::BlockId { height: 3, hash: Vec::new() };
        let response = router
            .call(framed_request(path::GET_BLOCK, id.encode_to_vec().into()))
            .await
            .expect("router answers");
        assert_eq!(response.headers().get("grpc-status"), Some(&HeaderValue::from_static("0")));

        let body = body_of(response).await;
        let decoded = cf::CompactBlock::decode(&body[FRAME_HEADER..]).expect("one framed message");
        assert_eq!(decoded.height, 3);
        assert_eq!(decoded.vtx[1].vin.len(), 1, "GetBlock carries transparent (slot 1, the spend)");

        // Data frames, then trailers (streamed `grpc-status` = trailers only: undrained = none)
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

        // Empty poolTypes = shielded only; one data frame per file window (<= 1 MiB), records
        // still framed one by one
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
        assert_eq!(decoded.vtx.len(), 2, "transparent requested: the coinbase kept too");
        assert_eq!(decoded.vtx[1].vin.len(), 1, "transparent requested, so kept");
        assert!(decoded.vtx[1].actions.is_empty(), "orchard not requested");

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

        // - Malformed ranges refused at the boundary (never reach the index's asserts)
        // - grpc-message = percent-encoded
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
