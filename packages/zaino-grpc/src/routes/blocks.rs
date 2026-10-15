//! Compact-block methods: `GetLatestBlock` (the served tip), `GetBlock`, `GetBlockRange`
//! (stored records, sent as the body, at heights `<=` [`At::answers_through`])

use bytes::Bytes;
use http::Response;
use http_body::Frame;
use tonic::{body::Body, Status};
use zaino_index_compact_block::{CompactBlockReader, Pools, RangeCursor, ServeError};
use zaino_nfs::At;
use zaino_persistence::{IndexKind, OverlayView, PinnedView};
use zcash_protocol::consensus::BlockHeight;

use crate::limits::Lane;
use crate::limits::ReadLanes;
use crate::wire::{self, trailers, unary_response};
use zaino_proto::proto::service;

pub(crate) use deprecated_nullifiers::range_nullifiers;

/// One `GrpcSnapshot`'s compact-block records
type Blocks<V> = CompactBlockReader<OverlayView<V>>;

/// Served `GrpcSnapshot` + its compact-block records
pub(crate) type Compact<'a, V> = (&'a At<V>, Blocks<V>);

/// Kinds kept distinct (miss != bad request != corruption)
fn to_status(error: ServeError) -> Status {
    match &error {
        ServeError::NotFound { .. } | ServeError::HashNotFound => {
            Status::not_found(error.to_string())
        }
        ServeError::Malformed { .. } => Status::internal(error.to_string()),
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
fn range_response<V: PinnedView>(
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
            // Range lane per disk step, not per request (layer reads never queue)
            let stepped = reads.read(Lane::Range, move || {
                let chunk = cursor.next_chunk();
                (cursor, chunk)
            });
            match stepped.await {
                Ok(stepped) => stepped,
                Err(status) => return Some((Ok(Frame::trailers(trailers(&status))), None)),
            }
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

/// `GetLatestBlock` answers a `BlockID`, not a block: the served tip (no read at all)
pub(crate) fn latest<V>(at: &At<V>) -> Response<Body> {
    let tip = at.tip();
    let hash = tip.hash.0.to_vec();
    unary_response(wire::frame(&service::BlockId { height: tip.height.into(), hash }))
}

/// `GetBlock`: one whole block, every pool
///
/// - TODO: deprecate pending light-client ZIP updates (`GetBlockRange` defaults shielded-only:
///   one block != range of one; `BlockID` has no `poolTypes`; kept = lightwalletd parity)
pub(crate) async fn block<V, B>(
    (at, blocks): Compact<'_, V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status>
where
    V: PinnedView,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    stored_block(at, blocks, body, reads).await.map(unary_response)
}

async fn stored_block<V, B>(
    at: &At<V>,
    blocks: Blocks<V>,
    body: B,
    reads: &ReadLanes,
) -> Result<Bytes, Status>
where
    V: PinnedView,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let id: service::BlockId = wire::decode_request(body).await?;

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
    // NFS layer (pepper-sync's reorg check at the tip): RAM, answered inline
    if let Some(record) = blocks.resident_block(height) {
        return Ok(record);
    }
    reads.read(Lane::Point, move || blocks.block(height).ok_or_else(missing)).await?
}

/// `GetBlockRange` (wallet-sync path: must stay cheap)
pub(crate) async fn range<V, B>(
    (at, blocks): Compact<'_, V>,
    body: B,
    reads: ReadLanes,
) -> Result<Response<Body>, Status>
where
    V: PinnedView,
    B: http_body::Body,
    B::Error: std::fmt::Display,
{
    let request: service::BlockRange = wire::decode_request(body).await?;

    let through = at.answers_through(IndexKind::CompactBlock);
    let walk = open_range(blocks, through, &request, wire::pools(&request.pool_types)?)?;
    Ok(range_response(walk, reads, Ok))
}

/// - walk order up to the first height past `through`: a first height past it = refused before
///   any block (lightwalletd: the first `getblock` fails)
fn open_range<V: PinnedView>(
    blocks: Blocks<V>,
    through: BlockHeight,
    request: &service::BlockRange,
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
    let ends = if end > through { past(through + 1) } else { Status::ok("") };
    let cursor = RangeCursor::new(blocks, start, end, through, pools).map_err(to_status)?;

    Ok(Walk { cursor, ends })
}

// =================================================================================================
// !!! TODO: REMOVE THIS MODULE. DEPRECATED `GetBlockRangeNullifiers`.
// !!! - Served only because pepper-sync still calls it; not part of Zaino's supported surface
// !!! - Delete with its `ROUTES` row + `Method` variant and its proto rpc once pepper-sync
// !!!   requests `GetBlockRange` with `poolTypes`
// =================================================================================================
mod deprecated_nullifiers {
    use bytes::Bytes;
    use http::Response;
    use prost::Message as _;
    use tonic::{body::Body, Status};
    use zaino_persistence::IndexKind;
    use zaino_proto::frame::frame_into;
    use zaino_proto::proto::compact_formats;
    use zaino_proto::proto::service;

    use super::{open_range, range_response, Compact};
    use crate::limits::ReadLanes;
    use crate::wire::{self, pools};

    /// Proto: MUST ignore a `TRANSPARENT` member (dropped before projection → `[TRANSPARENT]`
    /// alone = empty = shielded default, not "no pools")
    pub(crate) async fn range_nullifiers<V, B>(
        (at, blocks): Compact<'_, V>,
        body: B,
        reads: ReadLanes,
    ) -> Result<Response<Body>, Status>
    where
        V: zaino_persistence::PinnedView,
        B: http_body::Body,
        B::Error: std::fmt::Display,
    {
        let mut request: service::BlockRange = wire::decode_request(body).await?;
        request.pool_types.retain(|pool| *pool != service::PoolType::Transparent as i32);

        let through = at.answers_through(IndexKind::CompactBlock);
        let walk = open_range(blocks, through, &request, pools(&request.pool_types)?)?;
        Ok(range_response(walk, reads, reproject))
    }

    /// Framed records → framed nullifier-only records, one frame per block
    fn reproject(chunk: Bytes) -> Result<Bytes, Status> {
        let mut out = Vec::with_capacity(chunk.len());
        let messages = wire::messages(&chunk)
            .ok_or_else(|| Status::internal("stored range chunk is not whole frames"))?;
        for message in messages {
            let block = compact_formats::CompactBlock::decode(message)
                .map_err(|e| Status::internal(e.to_string()));
            let block = nullifiers_only(block?);
            frame_into(&mut out, |out| block.encode_raw(out));
        }

        Ok(Bytes::from(out))
    }

    /// Spends whole (a spend = its nullifier record), actions cut to `nullifier`, rest emptied
    fn nullifiers_only(mut block: compact_formats::CompactBlock) -> compact_formats::CompactBlock {
        let cut = |actions: Vec<compact_formats::CompactOrchardAction>| {
            actions
                .into_iter()
                .map(|action| compact_formats::CompactOrchardAction {
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
        block.chain_metadata = Some(compact_formats::ChainMetadata::default());

        block
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Response};
    use tonic::{body::Body, Status};
    use zaino_persistence::IndexKind;
    use zaino_primitives::testing::{h, MockChain};
    use zaino_proto::frame::FRAME_HEADER;

    use crate::service::{Method, Routes};
    use crate::testing::{
        decode_frames, dispatch, drained, framed_request, grpc_snapshots, indexed, routes, MAINNET,
    };

    /// `GetBlock` by hash: the block-hash index locates, the compact index answers only where it
    /// holds that same block; no locator wired = `Unimplemented`
    #[tokio::test]
    async fn get_block_by_hash_answers_only_where_the_compact_index_holds_that_block() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service;

        // compact index over 0..=2; locator over 0, 1, then a sibling at 2
        let mut chain = MockChain::regtest().network(MAINNET);
        let two = chain.mine_empty(2);
        let other_2 = chain.fork(h(1)).mine_empty(1).tip();
        let held = |at: u32| chain.at(h(at)).hash.0;
        let locator = indexed(IndexKind::BlockHash, &chain, other_2);
        let other_2 = other_2.hash.0;

        let compact = indexed(IndexKind::CompactBlock, &chain, two);
        let unlocated = dispatch(Routes {
            snapshots: grpc_snapshots(&chain, two, vec![compact.clone()]),
            ..routes()
        });
        let mut router = dispatch(Routes {
            snapshots: grpc_snapshots(&chain, two, vec![compact, locator]),
            ..routes()
        });
        let request = |hash: Vec<u8>, height: u64| {
            framed_request(
                Method::GetBlock,
                service::BlockId { height, hash }.encode_to_vec().into(),
            )
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

    /// GrpcSnapshot served at 5: a range reaching past the tip streams every block to the tip
    /// in walk order, then `OUT_OF_RANGE` in the trailers, never an early `OK` end (iOS hangs on
    /// one); a first height past the tip = `OUT_OF_RANGE` before any block (trailers-only)
    #[tokio::test]
    async fn get_block_range_past_the_tip_streams_to_it_then_ends_out_of_range() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats;
        use zaino_proto::proto::service;

        let mut chain = MockChain::regtest().network(MAINNET);
        let tip = chain.mine_empty(5);
        let compact = indexed(IndexKind::CompactBlock, &chain, tip);
        let mut router =
            dispatch(Routes { snapshots: grpc_snapshots(&chain, tip, vec![compact]), ..routes() });
        let past = |h: u32| format!("block%20{h}%20is%20above%20the%20compact-block%20tip%205");
        let (range, nullifiers) = (Method::GetBlockRange, Method::GetBlockRangeNullifiers);
        let cases = [
            (range, (1, 4), (vec![1, 2, 3, 4], "0", None)),
            (range, (5, 1), (vec![5, 4, 3, 2, 1], "0", None)),
            (range, (3, 9), (vec![3, 4, 5], "11", Some(past(6)))),
            (nullifiers, (3, 9), (vec![3, 4, 5], "11", Some(past(6)))),
            (range, (9, 3), (vec![], "11", Some(past(9)))),
            (range, (7, 9), (vec![], "11", Some(past(7)))),
        ];
        for (method, (start, end), expected) in cases {
            let range = service::BlockRange {
                start: Some(service::BlockId { height: start, hash: Vec::new() }),
                end: Some(service::BlockId { height: end, hash: Vec::new() }),
                pool_types: Vec::new(),
            };
            let request = framed_request(method, range.encode_to_vec().into());
            let response = router.call(request).await.expect("router answers");
            let headers = response.headers().clone();
            let body = response.into_body().collect().await.expect("body");
            let status = body.trailers().cloned().unwrap_or(headers);
            let text =
                |name: &str| status.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
            let blocks = decode_frames::<compact_formats::CompactBlock>(&body.to_bytes());
            let heights: Vec<_> = blocks.into_iter().map(|block| block.height).collect();
            let code = text("grpc-status").expect("a status");
            let got = (heights, code.as_str(), text("grpc-message"));
            assert_eq!(got, (expected.0, expected.1, expected.2), "{method:?} {start}..={end}");
        }
    }

    /// Blocks 1..=5: a coinbase paying alice, then one tx spending it into every pool (transparent
    /// in + out, a sapling spend + output, one orchard, two ironwood actions); body = stored bytes
    /// - `GetBlock` 3: one unary record, transparent kept
    /// - `GetBlockRange` 1..=4, no `poolTypes`: shielded only, one data frame, `OK` trailers
    /// - `[TRANSPARENT]`: the coinbase kept, orchard dropped
    #[tokio::test]
    async fn get_block_and_get_block_range_answer_stored_records_in_their_shapes() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::testing::p2pkh;
        use zaino_proto::proto::compact_formats;
        use zaino_proto::proto::service;
        use zcash_transparent::bundle::OutPoint;

        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=5u8 {
            let leaf = u32::from(at);
            chain.mine(|b| {
                b.coinbase(|c| c.txid([at; 32]).pay(&alice, 17_345)).tx(|t| {
                    t.spend(OutPoint::new([at; 32], 0))
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
            dispatch(Routes { snapshots: grpc_snapshots(&chain, tip, vec![compact]), ..routes() });

        let id = service::BlockId { height: 3, hash: Vec::new() };
        let response = router
            .call(framed_request(Method::GetBlock, id.encode_to_vec().into()))
            .await
            .expect("router answers");
        // unary too: a body + `grpc-status` in headers = malformed Trailers-Only (grpc-go refuses)
        assert_eq!(response.headers().get("grpc-status"), None, "unary status in headers");
        let (records, trailing) = drained(response).await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")), "unary");
        let body = records.concat();
        let decoded = compact_formats::CompactBlock::decode(&body[FRAME_HEADER..])
            .expect("one framed message");
        assert_eq!(decoded.height, 3);
        assert_eq!(decoded.vtx[1].vin.len(), 1, "GetBlock carries transparent (slot 1, the spend)");

        // one data frame per file window (<= 1 MiB), records still framed one by one
        let range = service::BlockRange {
            start: Some(service::BlockId { height: 1, hash: Vec::new() }),
            end: Some(service::BlockId { height: 4, hash: Vec::new() }),
            pool_types: Vec::new(),
        };
        let response = router
            .call(framed_request(Method::GetBlockRange, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        let headers = response.headers();
        let grpc = HeaderValue::from_static("application/grpc");
        let (status, content) =
            (headers.get("grpc-status"), headers.get(http::header::CONTENT_TYPE));
        assert_eq!((status, content), (None, Some(&grpc)), "stream status rides in the trailers");
        let (chunks, trailing) = drained(response).await;
        assert_eq!(chunks.len(), 1, "four durable records = one projected window");
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let blocks = decode_frames::<compact_formats::CompactBlock>(&chunks.concat());
        let shapes: Vec<_> = blocks
            .iter()
            .map(|block| (block.height, block.vtx.len(), block.vtx[0].vin.len()))
            .collect();
        let shielded_only = vec![(1, 1, 0), (2, 1, 0), (3, 1, 0), (4, 1, 0)];
        assert_eq!(shapes, shielded_only, "(height, txs, transparent ins): coinbase pruned");

        let range = service::BlockRange {
            start: Some(service::BlockId { height: 2, hash: Vec::new() }),
            end: Some(service::BlockId { height: 2, hash: Vec::new() }),
            pool_types: vec![service::PoolType::Transparent as i32],
        };
        let response = router
            .call(framed_request(Method::GetBlockRange, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        let (chunks, trailing) = drained(response).await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));
        let decoded = compact_formats::CompactBlock::decode(&chunks.concat()[FRAME_HEADER..])
            .expect("framed message");
        let spend = &decoded.vtx[1];
        let shape = (decoded.vtx.len(), spend.vin.len(), spend.actions.len());
        assert_eq!(shape, (2, 1, 0), "(txs, transparent ins, orchard actions): coinbase kept");
    }

    /// TODO: REMOVE with the deprecated alias. Blocks 1..=4, each one tx spending sapling
    /// nullifier `0x30 + h`, orchard `0x70 + h`, ironwood `0x80 + h` and `0x90 + h`:
    /// `[TRANSPARENT]` ignored (shielded default), each block = its nullifiers, all else gone
    #[tokio::test]
    async fn get_block_range_nullifiers_answers_only_each_blocks_nullifiers() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_primitives::testing::p2pkh;
        use zaino_proto::proto::compact_formats;
        use zaino_proto::proto::service;
        use zcash_transparent::bundle::OutPoint;

        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=4u8 {
            let leaf = u32::from(at);
            chain.mine(|b| {
                b.coinbase(|c| c.txid([at; 32]).pay(&alice, 17_345)).tx(|t| {
                    t.spend(OutPoint::new([at; 32], 0))
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
            dispatch(Routes { snapshots: grpc_snapshots(&chain, tip, vec![compact]), ..routes() });

        let range = service::BlockRange {
            start: Some(service::BlockId { height: 1, hash: Vec::new() }),
            end: Some(service::BlockId { height: 4, hash: Vec::new() }),
            pool_types: vec![service::PoolType::Transparent as i32],
        };
        let response = router
            .call(framed_request(Method::GetBlockRangeNullifiers, range.encode_to_vec().into()))
            .await
            .expect("router answers");
        let (chunks, trailing) = drained(response).await;
        assert_eq!(trailing.get("grpc-status"), Some(&HeaderValue::from_static("0")));

        // per tx: (sapling spends, sapling outputs, transparent in + out, orchard, ironwood)
        let projected: Vec<_> = decode_frames::<compact_formats::CompactBlock>(&chunks.concat())
            .into_iter()
            .map(|block| {
                let txs: Vec<_> = block
                    .vtx
                    .into_iter()
                    .map(|tx| {
                        let spends: Vec<_> = tx.spends.into_iter().map(|spend| spend.nf).collect();
                        let transparent = tx.vin.len() + tx.vout.len();
                        (spends, tx.outputs.len(), transparent, tx.actions, tx.ironwood_actions)
                    })
                    .collect();
                (block.height, block.chain_metadata, txs)
            })
            .collect();
        let nullifier = |seed: u8| compact_formats::CompactOrchardAction {
            nullifier: vec![seed; 32],
            ..Default::default()
        };
        let expected: Vec<_> = (1..=4u8)
            .map(|at| {
                let spends = vec![vec![0x30 + at; 32]];
                let ironwood = vec![nullifier(0x80 + at), nullifier(0x90 + at)];
                let tx = (spends, 0, 0, vec![nullifier(0x70 + at)], ironwood);
                (u64::from(at), Some(compact_formats::ChainMetadata::default()), vec![tx])
            })
            .collect();
        assert_eq!(projected, expected);
    }

    /// Malformed ranges refused at the boundary (never reach the index's asserts); grpc-message
    /// percent-encoded
    #[tokio::test]
    async fn a_malformed_range_is_invalid_argument_before_any_read() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service;

        let chain = MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let compact = indexed(IndexKind::CompactBlock, &chain, genesis);
        let mut router = dispatch(Routes {
            snapshots: grpc_snapshots(&chain, genesis, vec![compact]),
            ..routes()
        });

        let at = |height: u64| Some(service::BlockId { height, hash: Vec::new() });
        let above_ceiling = |bound: &str, height: u64| {
            format!("range%20{bound}%20{height}%20is%20above%20the%20protocol%20height%20ceiling")
        };
        let cases = [
            (None, at(1), "range%20has%20no%20start".to_owned()),
            (at(1), None, "range%20has%20no%20end".to_owned()),
            (at(1), at(1 << 32), above_ceiling("end", 1 << 32)),
            (at(u64::MAX), at(1), above_ceiling("start", u64::MAX)),
        ];
        let mut answered = Vec::new();
        for (start, end, _) in &cases {
            let bad =
                service::BlockRange { start: start.clone(), end: end.clone(), pool_types: vec![] };
            let request = framed_request(Method::GetBlockRange, bad.encode_to_vec().into());
            let response = router.call(request).await.expect("router answers");
            let header = |name| response.headers().get(name).and_then(|v| v.to_str().ok());
            answered.push((
                header("grpc-status").map(str::to_owned),
                header("grpc-message").map(str::to_owned),
            ));
        }
        let invalid = |(_, _, message): (_, _, String)| (Some("3".to_owned()), Some(message));
        assert_eq!(answered, cases.map(invalid));
    }
}
