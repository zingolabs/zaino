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
    use tonic::Code;
    use zaino_chainview::TrustedValidatorSet;
    use zaino_persistence::IndexKind;
    use zaino_source::testing::{h, MockChain};

    use crate::service::Method;
    use crate::testing::{
        answer, decode_frames, dispatch, fixed_snapshots, framed_request, indexed, routes_serving,
        MAINNET,
    };

    #[tokio::test]
    async fn get_block_by_hash_answers_only_where_the_compact_index_holds_that_block() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service::BlockId;

        // compact index over 0..=2, block-hash locator over 0, 1, then a sibling at 2
        let mut chain = MockChain::regtest().network(MAINNET);
        let two = chain.mine_empty(2);
        let sibling_2 = chain.fork(h(1)).mine_empty(1).tip();
        let compact = indexed(IndexKind::CompactBlock, &chain, two);
        let locator = indexed(IndexKind::BlockHash, &chain, sibling_2);
        let indexes = [IndexKind::CompactBlock, IndexKind::BlockHash];
        let serving = |views| {
            let held_by = TrustedValidatorSet::default();
            dispatch(routes_serving(
                MAINNET,
                &indexes,
                fixed_snapshots(&chain, two, views, held_by, &[]),
            ))
        };
        let located = serving(vec![compact.clone(), locator]);
        let unlocated = serving(vec![compact]);
        let get_block =
            async |mut router: crate::service::Dispatch<_, _>, hash: Vec<u8>, height| {
                let message = BlockId { height, hash }.encode_to_vec();
                answer(
                    router
                        .call(framed_request(Method::GetBlock, message.into()))
                        .await
                        .expect("infallible"),
                )
                .await
            };
        let block_1 = chain.at(h(1)).hash.0;

        let (by_hash, status) = get_block(located.clone(), block_1.to_vec(), 0).await;
        let (by_height, _) = get_block(located.clone(), Vec::new(), 1).await;
        assert_eq!(status.code(), Code::Ok, "block 1 by hash: {status:?}");
        assert_eq!(by_hash, by_height, "block 1 by hash = the stored record at 1");

        #[rustfmt::skip]
        let refused = [
            ("unknown hash",          located.clone(), vec![0xee; 32],            Code::NotFound),
            ("sibling's hash at 2",   located.clone(), sibling_2.hash.0.to_vec(), Code::NotFound),
            ("31-byte hash",          located,         block_1[..31].to_vec(),    Code::InvalidArgument),
            ("no block-hash locator", unlocated,       block_1.to_vec(),          Code::Unimplemented),
        ];
        for (case, router, hash, code) in refused {
            let (_, status) = get_block(router, hash, 0).await;
            assert_eq!(status.code(), code, "{case}: {status:?}");
        }
    }

    /// Never an early `OK` end past the tip (iOS hangs on one)
    #[tokio::test]
    async fn get_block_range_past_the_tip_streams_to_it_then_ends_out_of_range() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats::CompactBlock;
        use zaino_proto::proto::service::{BlockId, BlockRange};
        use Method::{GetBlockRange, GetBlockRangeNullifiers};

        let mut chain = MockChain::regtest().network(MAINNET);
        let tip_5 = chain.mine_empty(5);
        let views = vec![indexed(IndexKind::CompactBlock, &chain, tip_5)];
        let snapshots = fixed_snapshots(&chain, tip_5, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::CompactBlock], snapshots));
        let above = |height: u32| format!("block {height} is above the compact-block tip 5");

        // case, method, start, end, heights streamed, final code, its message
        #[rustfmt::skip]
        let cases = [
            ("ascending to 4",      GetBlockRange,           1, 4, vec![1, 2, 3, 4],    Code::Ok,         String::new()),
            ("descending from 5",   GetBlockRange,           5, 1, vec![5, 4, 3, 2, 1], Code::Ok,         String::new()),
            ("reaching past 5",     GetBlockRange,           3, 9, vec![3, 4, 5],       Code::OutOfRange, above(6)),
            ("alias reaching past", GetBlockRangeNullifiers, 3, 9, vec![3, 4, 5],       Code::OutOfRange, above(6)),
            ("descending from 9",   GetBlockRange,           9, 3, vec![],              Code::OutOfRange, above(9)),
            ("wholly past the tip", GetBlockRange,           7, 9, vec![],              Code::OutOfRange, above(7)),
        ];
        for (case, method, start, end, heights, code, message) in cases {
            let at = |height| Some(BlockId { height, hash: Vec::new() });
            let range = BlockRange { start: at(start), end: at(end), pool_types: Vec::new() };

            let response = router.call(framed_request(method, range.encode_to_vec().into())).await;
            let (chunks, status) = answer(response.expect("infallible")).await;

            let blocks = decode_frames::<CompactBlock>(&chunks.concat());
            let streamed: Vec<_> = blocks.iter().map(|block| block.height).collect();
            assert_eq!(streamed, heights, "{case} ({start}..={end}): heights streamed");
            assert_eq!((status.code(), status.message()), (code, message.as_str()), "{case}");
        }
    }

    /// Unary status in trailers too (a body + `grpc-status` in headers = malformed Trailers-Only:
    /// grpc-go refuses)
    #[tokio::test]
    async fn get_block_answers_one_stored_record_with_every_pool() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
        use zaino_proto::proto::service::BlockId;

        // 1..=5: a coinbase paying alice, then one tx spending it into transparent + sapling
        let alice = zaino_source::testing::p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=5u32 {
            chain.mine(|b| {
                b.coinbase(|c| c.pay(&alice, 17_345)).tx(|t| {
                    let spend = t.spend_in_block(0, 0).pay(&alice, 12_345).fee(5_000);
                    spend.sapling_spend(0x30 + at).sapling_output(at)
                })
            });
        }
        let tip = chain.tip();
        let views = vec![indexed(IndexKind::CompactBlock, &chain, tip)];
        let snapshots = fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::CompactBlock], snapshots));

        let id = BlockId { height: 3, hash: Vec::new() };
        let response =
            router.call(framed_request(Method::GetBlock, id.encode_to_vec().into())).await;

        let response = response.expect("infallible");
        assert_eq!(response.headers().get("grpc-status"), None, "unary status in headers");
        let (records, status) = answer(response).await;
        assert_eq!(status.code(), Code::Ok, "{status:?}");
        let blocks = decode_frames::<CompactBlock>(&records.concat());
        let [block] = <[CompactBlock; 1]>::try_from(blocks)
            .unwrap_or_else(|blocks| panic!("one framed record, got {}", blocks.len()));
        let shape = |tx: &CompactTx| (tx.vin.len(), tx.spends.len(), tx.outputs.len());
        let txs: Vec<_> = block.vtx.iter().map(shape).collect();
        assert_eq!(block.height, 3, "{block:?}");
        assert_eq!(
            txs,
            [(0, 0, 0), (1, 1, 1)],
            "per tx (transparent ins, sapling spends, sapling outputs): coinbase + the spend"
        );
    }

    #[tokio::test]
    async fn get_block_range_streams_shielded_only_unless_transparent_is_named() {
        use http::HeaderValue;
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::compact_formats::CompactBlock;
        use zaino_proto::proto::service::{BlockId, BlockRange, PoolType};

        // 1..=5: a coinbase paying alice, then one tx spending it into transparent + orchard
        let alice = zaino_source::testing::p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().network(MAINNET);
        for at in 1..=5u32 {
            chain.mine(|b| {
                b.coinbase(|c| c.pay(&alice, 17_345)).tx(|t| {
                    t.spend_in_block(0, 0)
                        .pay(&alice, 12_345)
                        .fee(5_000)
                        .orchard_action(0x70 + at, at)
                })
            });
        }
        let tip = chain.tip();
        let views = vec![indexed(IndexKind::CompactBlock, &chain, tip)];
        let snapshots = fixed_snapshots(&chain, tip, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::CompactBlock], snapshots));
        let mut range = async |start, end, pool_types| {
            let at = |height| Some(BlockId { height, hash: Vec::new() });
            let message = BlockRange { start: at(start), end: at(end), pool_types }.encode_to_vec();
            router
                .call(framed_request(Method::GetBlockRange, message.into()))
                .await
                .expect("infallible")
        };
        // per block: (height, txs, the spend's transparent ins, its orchard actions)
        let shapes = |blocks: Vec<CompactBlock>| -> Vec<_> {
            let shape = |block: &CompactBlock| {
                let spend = block.vtx.last().expect("the spend survives every selection");
                (block.height, block.vtx.len(), spend.vin.len(), spend.actions.len())
            };
            blocks.iter().map(shape).collect()
        };

        let shielded = range(1, 4, Vec::new()).await;
        let grpc = HeaderValue::from_static("application/grpc");
        let content_type = shielded.headers().get(http::header::CONTENT_TYPE);
        assert_eq!(content_type, Some(&grpc), "{:?}", shielded.headers());
        assert_eq!(shielded.headers().get("grpc-status"), None, "stream status in the headers");
        let (chunks, status) = answer(shielded).await;
        assert_eq!(status.code(), Code::Ok, "no poolTypes: {status:?}");
        let blocks = shapes(decode_frames(&chunks.concat()));
        assert_eq!(
            blocks,
            [(1, 1, 0, 1), (2, 1, 0, 1), (3, 1, 0, 1), (4, 1, 0, 1)],
            "coinbase pruned"
        );

        let (chunks, status) = answer(range(2, 2, vec![PoolType::Transparent as i32]).await).await;
        assert_eq!(status.code(), Code::Ok, "[TRANSPARENT]: {status:?}");
        let blocks = shapes(decode_frames(&chunks.concat()));
        assert_eq!(blocks, [(2, 2, 1, 0)], "[TRANSPARENT]: coinbase kept, orchard dropped");
    }

    /// Refused at the boundary: never reaches the index's asserts
    #[tokio::test]
    async fn a_malformed_range_is_invalid_argument_before_any_read() {
        use prost::Message as _;
        use tower::Service as _;
        use zaino_proto::proto::service::{BlockId, BlockRange};

        let chain = MockChain::regtest().network(MAINNET);
        let genesis = chain.genesis();
        let views = vec![indexed(IndexKind::CompactBlock, &chain, genesis)];
        let snapshots =
            fixed_snapshots(&chain, genesis, views, TrustedValidatorSet::default(), &[]);
        let mut router = dispatch(routes_serving(MAINNET, &[IndexKind::CompactBlock], snapshots));
        let at = |height: u64| Some(BlockId { height, hash: Vec::new() });
        let above_ceiling = |bound: &str, height: u64| {
            format!("range {bound} {height} is above the protocol height ceiling")
        };

        #[rustfmt::skip]
        let cases = [
            ("no start",           None,         at(1),       "range has no start".to_owned()),
            ("no end",             at(1),        None,        "range has no end".to_owned()),
            ("end past u32",       at(1),        at(1 << 32), above_ceiling("end", 1 << 32)),
            ("start at u64::MAX",  at(u64::MAX), at(1),       above_ceiling("start", u64::MAX)),
        ];
        for (case, start, end, message) in cases {
            let bad = BlockRange { start, end, pool_types: Vec::new() };

            let response = router
                .call(framed_request(Method::GetBlockRange, bad.encode_to_vec().into()))
                .await;

            let (_, status) = answer(response.expect("infallible")).await;
            let expected = (Code::InvalidArgument, message.as_str());
            assert_eq!((status.code(), status.message()), expected, "{case}");
        }
    }
}
