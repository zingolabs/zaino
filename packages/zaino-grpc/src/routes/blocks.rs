//! Compact-block methods: `GetLatestBlock` (the served tip), `GetBlock`, `GetBlockRange`
//! (stored records, sent as the body, at heights `<=` [`IndexesAtBlock::answers_through`])

use bytes::Bytes;
use http::Response;
use http_body::Frame;
use tonic::{body::Body, Status};
use zaino_index_compact_block::{CompactBlockReader, Pools, RangeCursor, ServeError};
use zaino_nfs::IndexesAtBlock;
use zaino_persistence::{IndexKind, OverlayView, PinnedView};
use zcash_protocol::consensus::BlockHeight;

use super::args;
use crate::read_lanes::{Lane, ReadLanes};
use crate::wire::{self, trailers, unary_response};
use zaino_proto::proto::service;

/// One request's compact blocks: the served indexes + the compact-block reader over them
pub(crate) struct CompactBlocks<'a, V> {
    pub(crate) served: &'a IndexesAtBlock<V>,
    pub(crate) reader: CompactBlockReader<OverlayView<V>>,
}

/// `GetLatestBlock` answers a `BlockID`, not a block: the served tip (no read at all)
pub(crate) fn get_latest_block<V>(served: &IndexesAtBlock<V>) -> Response<Body> {
    let tip = served.tip();
    let block_id = service::BlockId { height: tip.height.into(), hash: tip.hash.0.to_vec() };
    unary_response(wire::frame(&block_id))
}

/// `GetBlock`: one whole block, every pool
///
/// - hash wins when given (names one block across a reorg, a height doesn't)
/// - one block != range of one (`GetBlockRange` defaults shielded-only, `BlockID` has no
///   `poolTypes`)
/// - TODO: deprecate pending light-client ZIP updates (kept = lightwalletd parity)
pub(crate) async fn get_block<V: PinnedView>(
    compact_blocks: CompactBlocks<'_, V>,
    block_id: service::BlockId,
    reads: &ReadLanes,
) -> Result<Response<Body>, Status> {
    let block = if block_id.hash.is_empty() {
        let height = args::height(block_id.height, "height")?;
        compact_blocks.read_by_height(height, reads).await?
    } else {
        let (height, hash) = compact_blocks.locate(&block_id.hash, "GetBlock")?;
        compact_blocks.read_by_hash(height, hash, reads).await?
    };
    Ok(unary_response(block))
}

/// `GetBlockRange` (wallet-sync path: must stay cheap)
pub(crate) fn get_block_range<V: PinnedView>(
    compact_blocks: CompactBlocks<'_, V>,
    range: service::BlockRange,
    reads: ReadLanes,
) -> Result<Response<Body>, Status> {
    let pools = args::pools(&range.pool_types)?;
    let blocks = compact_blocks.open_range(&range, pools)?;
    Ok(blocks.into_response(reads, Ok))
}

impl<V: PinnedView> CompactBlocks<'_, V> {
    /// Highest height this request answers (the compact-block index's, capped at the served tip)
    fn served_through(&self) -> BlockHeight {
        self.served.answers_through(IndexKind::CompactBlock)
    }

    /// `BlockID.hash` → its height via the block-hash index, at or below [`Self::served_through`]
    fn locate(&self, hash: &[u8], method: &str) -> Result<(BlockHeight, [u8; 32]), Status> {
        args::locate(self.served, self.served_through(), hash, method)
    }

    /// NFS layer (pepper-sync's reorg check at the tip) = RAM, answered inline; else the point lane
    async fn read_by_height(self, height: BlockHeight, reads: &ReadLanes) -> Result<Bytes, Status> {
        let missing = move || serve_error_status(ServeError::NotFound { height });
        if height > self.served_through() {
            return Err(missing());
        }
        if let Some(block) = self.reader.resident_block(height) {
            return Ok(block);
        }
        let reader = self.reader;
        reads.read(Lane::Point, move || reader.block(height).ok_or_else(missing)).await?
    }

    /// Only where this index holds that same block (a located height can lag a reorg)
    async fn read_by_hash(
        self,
        height: BlockHeight,
        hash: [u8; 32],
        reads: &ReadLanes,
    ) -> Result<Bytes, Status> {
        let reader = self.reader;
        let read = move || reader.block_at(height, &hash).map_err(serve_error_status);
        reads.read(Lane::Point, read).await?
    }

    /// Walk order up to the first height past [`Self::served_through`]
    ///
    /// - first height past it = refused before any block (lightwalletd: the first `getblock` fails)
    /// - start > end = descending (the cursor walks it top down)
    pub(super) fn open_range(
        self,
        range: &service::BlockRange,
        pools: Pools,
    ) -> Result<BlockStream<V>, Status> {
        let bound = |id: &Option<service::BlockId>, field: &str| {
            let id = id
                .as_ref()
                .ok_or_else(|| Status::invalid_argument(format!("range has no {field}")))?;
            args::height(id.height, &format!("range {field}"))
        };
        let (start, end) = (bound(&range.start, "start")?, bound(&range.end, "end")?);

        let through = self.served_through();
        let above_tip = |height| {
            Status::out_of_range(format!("block {height} is above the compact-block tip {through}"))
        };
        if start > through {
            return Err(above_tip(start));
        }
        let final_status = if end > through { above_tip(through + 1) } else { Status::ok("") };
        let cursor = RangeCursor::new(self.reader, start, end, through, pools)
            .map_err(serve_error_status)?;

        Ok(BlockStream { cursor, final_status })
    }
}

/// One range's cursor + the status its stream ends in once the cursor is spent
///
/// - `final_status` = `OUT_OF_RANGE` when the range reaches past the compact-block tip
///   (lightwalletd: lazily to the tip, then the error)
/// - early `OK` end hangs the iOS downloader for good
pub(super) struct BlockStream<V> {
    cursor: RangeCursor<V>,
    final_status: Status,
}

impl<V: PinnedView> BlockStream<V> {
    /// Server-streaming response, cursor chunk by chunk
    ///
    /// - `grpc-status` in trailers only (headers precede the walk, a status there = response done)
    /// - `reproject` = per-chunk rewrite (`Ok` for `GetBlockRange`, so its records stay slices)
    pub(super) fn into_response(
        self,
        reads: ReadLanes,
        reproject: fn(Bytes) -> Result<Bytes, Status>,
    ) -> Response<Body> {
        let BlockStream { cursor, final_status } = self;
        let state = Some((cursor, reads, final_status));
        let frames = futures::stream::unfold(state, move |state| async move {
            let (cursor, reads, final_status) = state?;

            let (cursor, chunk) = match next_chunk(cursor, &reads).await {
                Ok(stepped) => stepped,
                Err(status) => return Some((Ok(Frame::trailers(trailers(&status))), None)),
            };
            let end = match chunk.map(|read| read.map_err(serve_error_status).and_then(reproject)) {
                Some(Ok(chunk)) => {
                    return Some((Ok(Frame::data(chunk)), Some((cursor, reads, final_status))))
                }
                Some(Err(status)) => status,
                None => final_status,
            };
            Some((Ok::<_, Status>(Frame::trailers(trailers(&end))), None))
        });

        // layer steps = one record each, ready back to back: joined per DATA frame
        wire::streaming(frames)
    }
}

/// Cursor's next chunk (`None` = spent)
///
/// - disk step = blocking pool, range lane per step not per request (cold mmap refill ~11.7 ms
///   would stall the worker; layer reads never queue)
/// - in-memory step = inline (else a task per block on the highest-volume RPC)
async fn next_chunk<V: PinnedView>(
    mut cursor: RangeCursor<V>,
    reads: &ReadLanes,
) -> Result<(RangeCursor<V>, Option<Result<Bytes, ServeError>>), Status> {
    if !cursor.next_touches_disk() {
        let chunk = cursor.next_chunk();
        return Ok((cursor, chunk));
    }
    reads
        .read(Lane::Range, move || {
            let chunk = cursor.next_chunk();
            (cursor, chunk)
        })
        .await
}

/// Kinds kept distinct (miss != bad request != corruption)
fn serve_error_status(error: ServeError) -> Status {
    match &error {
        ServeError::NotFound { .. } | ServeError::HashNotFound => {
            Status::not_found(error.to_string())
        }
        ServeError::Malformed { .. } => Status::internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderValue, Response};
    use tonic::{body::Body, Status};
    use zaino_persistence::IndexKind;
    use zaino_proto::frame::FRAME_HEADER;
    use zaino_source::testing::{h, MockChain};

    use crate::service::{Method, RouteDeps};
    use crate::testing::{
        decode_frames, dispatch, drained, framed_request, grpc_snapshots, indexed, routes, MAINNET,
    };

    /// `GetBlock` by hash:
    ///
    /// - block-hash index locates, compact index answers only where it holds that same block
    /// - no locator wired = `Unimplemented`
    #[tokio::test]
    async fn get_block_by_hash_answers_only_where_the_compact_index_holds_that_block() {
        use http_body_util::BodyExt as _;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service;

        // compact index over 0..=2, locator over 0, 1, then a sibling at 2
        let mut chain = MockChain::regtest().network(MAINNET);
        let two = chain.mine_empty(2);
        let other_2 = chain.fork(h(1)).mine_empty(1).tip();
        let held = |at: u32| chain.at(h(at)).hash.0;
        let locator = indexed(IndexKind::BlockHash, &chain, other_2);
        let other_2 = other_2.hash.0;

        let compact = indexed(IndexKind::CompactBlock, &chain, two);
        let unlocated = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, two, vec![compact.clone()]),
            ..routes()
        });
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, two, vec![compact, locator]),
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

    /// GrpcSnapshot served at 5:
    ///
    /// - range reaching past the tip streams every block to the tip in walk order, then
    ///   `OUT_OF_RANGE` in the trailers, never an early `OK` end (iOS hangs on one)
    /// - first height past the tip = `OUT_OF_RANGE` before any block (trailers-only)
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
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, tip, vec![compact]),
            ..routes()
        });
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
    /// in + out, a sapling spend + output, one orchard, two ironwood actions):
    ///
    /// - body = stored bytes
    /// - `GetBlock` 3: one unary record, transparent kept
    /// - `GetBlockRange` 1..=4, no `poolTypes`: shielded only, one data frame, `OK` trailers
    /// - `[TRANSPARENT]`: the coinbase kept, orchard dropped
    #[tokio::test]
    async fn get_block_and_get_block_range_answer_stored_records_in_their_shapes() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats;
        use zaino_proto::proto::service;
        use zaino_source::testing::p2pkh;

        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=5u32 {
            chain.mine(|b| {
                b.coinbase(|c| c.pay(&alice, 17_345)).tx(|t| {
                    t.spend_in_block(0, 0)
                        .pay(&alice, 12_345)
                        .fee(5_000)
                        .sapling_spend(0x30 + at)
                        .sapling_output(at)
                        .orchard_action(0x70 + at, at)
                        .ironwood_action(0x80 + at, 2 * at)
                        .ironwood_action(0x90 + at, 2 * at + 1)
                })
            });
        }
        let tip = chain.tip();
        let compact = indexed(IndexKind::CompactBlock, &chain, tip);
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, tip, vec![compact]),
            ..routes()
        });

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

    /// - malformed ranges refused at the boundary (never reach the index's asserts)
    /// - grpc-message percent-encoded
    #[tokio::test]
    async fn a_malformed_range_is_invalid_argument_before_any_read() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service;

        let chain = MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let compact = indexed(IndexKind::CompactBlock, &chain, genesis);
        let mut router = dispatch(RouteDeps {
            grpc_snapshots: grpc_snapshots(&chain, genesis, vec![compact]),
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
