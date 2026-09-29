//! RPC surface: `GetLatestBlock`, `GetBlock`, `GetBlockRange`
//!
//! - here, not in the server crate (an index disabled = its methods never compiled)
//! - answers in stored bytes, never decoded or re-encoded
//! - block / range = zero-copy [`Bytes`] slices of the mapping (range walked per [`SPAN_BUDGET`]
//!   window), pool pruning = framing walk

use std::{num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use zaino_primitives::types::Height;
use zaino_sync::Served;

use crate::{project::project, project::record_hash, view::ReadView, Pools, HASH};

/// Ceiling on one range window (one readahead + one slice handed to the socket)
///
/// - `GetBlockRange` work per step bounded by this, not by range length
pub(crate) const SPAN_BUDGET: usize = 1 << 20;

/// Blocks one `GetBlockRange` may return unless an operator overrides it
///
/// - 2 x the 2^16 sapling/orchard subtree: pepper-sync asks for a whole shard range in one call
/// - headroom for a scan range straddling a subtree boundary (client never shrinks its ask)
/// - not a memory bound (streamed 1 MiB at a time); a cap on one request's work
pub const DEFAULT_MAX_BLOCK_RANGE: NonZeroU32 =
    NonZeroU32::new(2 * 65_536).expect("131072 is non-zero");

/// Small (transport maps these onto gRPC codes; this crate names no transport)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// Every method until the index reaches the tip; no progress carried (a partial index
    /// answering what it holds = "no such block" and "not indexed yet" read the same)
    #[error("the compact-block index is still syncing")]
    Syncing,

    #[error("block {height} is not in the index")]
    NotFound { height: Height },

    #[error("block hash is not in the index")]
    HashNotFound,

    #[error("requested {asked} blocks, the per-range maximum is {limit}")]
    RangeTooLarge { asked: u64, limit: u32 },

    #[error("the index holds no blocks")]
    Empty,

    /// Stored record that will not walk (corruption, not a bad request)
    #[error("stored record at height {height} is malformed")]
    Malformed { height: Height },
}

/// Two tiers, one surface: files up to the finalised tip, the non-finalized tier above it
///
/// - a request pins **both at once** ([`ReadView`], one load): the seam cannot move mid-stream
#[derive(Debug, Clone)]
pub struct CompactBlockService {
    served: Served<ReadView>,
    max_range: NonZeroU32,
}

impl CompactBlockService {
    /// Unsynced → every method [`ServeError::Syncing`]
    pub fn new(served: Served<ReadView>) -> Self {
        Self { served, max_range: DEFAULT_MAX_BLOCK_RANGE }
    }

    /// Overrides [`DEFAULT_MAX_BLOCK_RANGE`] for `GetBlockRange`
    pub fn with_max_range(mut self, max_range: NonZeroU32) -> Self {
        self.max_range = max_range;
        self
    }

    /// Every tier pinned for one request or stream (one load); checked before any other
    /// validation (a syncing index = one answer, not one per request shape)
    fn pin(&self) -> Result<Arc<ReadView>, ServeError> {
        self.served.pin().ok_or(ServeError::Syncing)
    }

    /// Last height any tier can serve, inclusive, synced or not (non-finalized included: what
    /// `GetLatestBlock` answers once synced; `None` = nothing held)
    pub fn tip(&self) -> Option<Height> {
        self.served.pin_any().tip()
    }

    /// `GetBlock`: every pool, transparent included (only `GetBlockRange` filters: the protocol's
    /// asymmetry, lightwalletd's shape)
    pub fn block(&self, height: Height) -> Result<Bytes, ServeError> {
        self.pin()?.block(height).ok_or(ServeError::NotFound { height })
    }

    /// [`block`](Self::block) when the non-finalized tier holds it (RAM, no page read: a transport
    /// may answer inline); `Ok(None)` = ask [`block`](Self::block)
    pub fn resident_block(&self, height: Height) -> Result<Option<Bytes>, ServeError> {
        Ok(self.pin()?.resident_block(height))
    }

    /// `GetBlock` by hash, `height` located by the block-hash index
    ///
    /// - served only if this index holds `hash` there (independent publications: a reorg can
    ///   land between the locate and this read)
    pub fn block_at_hash(&self, height: Height, hash: &[u8; HASH]) -> Result<Bytes, ServeError> {
        let record = self.pin()?.block(height).ok_or(ServeError::HashNotFound)?;
        match record_hash(&record) {
            Some(held) if held == *hash => Ok(record),
            Some(_) => Err(ServeError::HashNotFound),
            None => Err(ServeError::Malformed { height }),
        }
    }

    /// `GetLatestBlock`: the tip's height and hash (a `BlockID`, not a block)
    ///
    /// - resolved when the view was published: RAM, no page read (a transport may answer inline)
    pub fn latest_id(&self) -> Result<(Height, [u8; HASH]), ServeError> {
        match self.pin()?.tip_id() {
            None => Err(ServeError::Empty),
            Some((height, None)) => Err(ServeError::Malformed { height }),
            Some((height, Some(hash))) => Ok((height, hash)),
        }
    }

    /// `GetBlockRange` of heights `start` to `end`, both inclusive, as a cursor over both tiers
    ///
    /// - `start <= end` (callers refuse a reversed request)
    /// - no read here (the first file window = the cursor's first blocking step)
    pub fn range(
        &self,
        start: Height,
        end: Height,
        pools: Pools,
    ) -> Result<RangeCursor, ServeError> {
        self.range_with_budget(start, end, pools, SPAN_BUDGET)
    }

    /// [`range`](Self::range) with an explicit window size (a test forces a refill)
    pub(crate) fn range_with_budget(
        &self,
        start: Height,
        end: Height,
        pools: Pools,
        budget: usize,
    ) -> Result<RangeCursor, ServeError> {
        assert!(start <= end, "reversed range {start:?}..={end:?} past the request boundary");
        let view = self.pin()?;
        let Some(tip) = view.tip().filter(|&tip| start <= tip) else {
            return Err(ServeError::NotFound { height: start });
        };

        // clamp, never refuse (a wallet asking past the tip wants what exists)
        let served_end = end.min(tip);

        // measured after the clamp (an open-ended `end` over a short chain = a small request)
        // still over = refused, never truncated (a short answer reads as the chain's end)
        let asked = u64::from(served_end) - u64::from(start) + 1;
        if asked > u64::from(self.max_range.get()) {
            return Err(ServeError::RangeTooLarge { asked, limit: self.max_range.get() });
        }

        Ok(RangeCursor {
            finalized_tip: view.finalized_tip().min(Some(served_end)),
            view,
            budget,
            pools,
            served: start.checked_sub(1),
            end: served_end,
        })
    }
}

/// `GetBlockRange` walked one chunk at a time: a file window below the seam, one non-finalized
/// record above it
///
/// - no `Iterator` impl (the caller routes a disk step to the blocking pool first)
/// - `view` pinned for the whole stream: every tier + the seam between them frozen
/// - `served`, `finalized_tip`, `end` = last heights, inclusive (`served` `None` = nothing yet;
///   `finalized_tip` `None` = no file heights in range)
#[derive(Debug)]
pub struct RangeCursor {
    view: Arc<ReadView>,
    budget: usize,
    pools: Pools,
    served: Option<Height>,
    finalized_tip: Option<Height>,
    end: Height,
}

impl RangeCursor {
    /// Next chunk reads the files (a cold window faults: the blocking pool's step)
    pub fn next_touches_disk(&self) -> bool {
        self.served < self.finalized_tip
    }

    /// Next wire chunk (framed records back to back, projected to the cursor's pools), `None`
    /// once the range is spent
    pub fn next_chunk(&mut self) -> Option<Result<Bytes, ServeError>> {
        if self.served >= Some(self.end) {
            return None;
        }

        let height = self.served.map_or(Height::GENESIS, Height::next);
        let Some(last) = self.finalized_tip.filter(|_| self.next_touches_disk()) else {
            // non-finalized: projected at apply for the default pools (every synced wallet's ask)
            self.served = Some(height);
            let projected = self.view.resident_projected(height, self.pools);
            return Some(projected.ok_or(ServeError::NotFound { height }));
        };
        let Some((records, reached)) = self.view.span_from(height, last, self.budget) else {
            return Some(Err(ServeError::NotFound { height }));
        };
        self.served = Some(reached);

        Some(project(&records, self.pools).ok_or(ServeError::Malformed { height }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        encode_compact_block,
        record::{framed_len, FRAME_HEADER},
        testing::block,
        CompactBlockReader, CompactBlockStore, NonFinalizedState,
    };
    use prost::Message;
    use zaino_persistence::fs::SimFs;
    use zaino_proto::proto::compact_formats as cf;
    use zcash_protocol::consensus::NetworkType;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// Store holding `testing::block(0..blocks)`, committed
    fn committed(blocks: u32) -> CompactBlockReader {
        let mut store = CompactBlockStore::open(
            SimFs::new(),
            std::path::Path::new("/cb"),
            NetworkType::Regtest,
        )
        .expect("open");
        for height in 0..blocks {
            let (block, balances, sizes) = block(height);
            let record = encode_compact_block(&block, &balances, &sizes);
            store.append(h(height), block.header().hash.into(), &record).expect("append");
        }
        store.commit(block(0).2).expect("commit");
        store.reader()
    }

    fn service(blocks: u32) -> CompactBlockService {
        CompactBlockService::new(Served::fixed(committed(blocks).pin()))
    }

    /// Every framed record a chunk carries, decoded
    fn decode(chunk: &[u8]) -> Vec<cf::CompactBlock> {
        let mut blocks = Vec::new();
        let mut rest = chunk;
        while !rest.is_empty() {
            let len = framed_len(rest).expect("whole frame");
            blocks.push(cf::CompactBlock::decode(&rest[FRAME_HEADER..len]).expect("message"));
            rest = &rest[len..];
        }
        blocks
    }

    /// Cursor walked to the end: the chunks it yielded (a spent cursor must stay spent)
    fn drain(mut cursor: RangeCursor) -> Vec<Bytes> {
        let mut chunks = Vec::new();
        while let Some(chunk) = cursor.next_chunk() {
            chunks.push(chunk.expect("chunk"));
        }
        assert!(cursor.next_chunk().is_none(), "spent cursor stays spent");
        chunks
    }

    fn heights(chunks: &[Bytes]) -> Vec<u64> {
        chunks.iter().flat_map(|chunk| decode(chunk)).map(|block| block.height).collect()
    }

    #[test]
    fn single_block_reads_carry_every_pool_and_report_misses() {
        let service = service(4);

        let body = service.block(h(2)).expect("block 2");
        let [decoded] = decode(&body).try_into().expect("one record");
        assert_eq!(decoded.height, 2);

        // GetBlock unfiltered, unlike GetBlockRange
        assert_eq!(decoded.vtx[0].vin.len(), 1, "transparent present");
        assert_eq!(decoded.vtx[0].ironwood_actions.len(), 2, "ironwood present");

        // served bytes = stored bytes (same framing, no copy, no re-encode)
        let stored = service.pin().expect("synced").block(h(2)).expect("stored");
        assert_eq!(body, stored, "served record is the stored record");
        assert_eq!(body[0], 0, "gRPC compression flag");
        assert_eq!(framed_len(&body), Some(body.len()), "frame length = the message it carries");

        let (height, hash) = service.latest_id().expect("latest");
        assert_eq!(height, h(3));
        let [tip] = decode(&service.block(h(3)).expect("tip")).try_into().expect("one record");
        assert_eq!(hash.to_vec(), tip.hash, "tip hash = the stored record's");

        assert_eq!(service.block(h(4)), Err(ServeError::NotFound { height: h(4) }));

        let by_hash = service.block_at_hash(h(2), &[2u8; HASH]).expect("by hash");
        assert_eq!(by_hash, service.block(h(2)).expect("by height"));
        let other = service.block_at_hash(h(2), &[0xfe; HASH]);
        assert_eq!(other, Err(ServeError::HashNotFound), "another block at the located height");
        let past = service.block_at_hash(h(9), &[9u8; HASH]);
        assert_eq!(past, Err(ServeError::HashNotFound), "a height past the tip");
    }

    /// Range crossing the tier seam (where an off-by-one hides): every height exactly once, in
    /// order, no duplicate or gap at the boundary
    #[test]
    fn a_range_spans_the_file_store_and_the_window_without_a_seam() {
        // finalized 0 to 3 (both inclusive), non-finalized from 4
        let reader = committed(4);
        let mut non_finalized = NonFinalizedState::default();
        for height in 4..7u32 {
            let (block, balances, sizes) = block(height);
            non_finalized.apply(
                h(height),
                [height as u8; HASH],
                encode_compact_block(&block, &balances, &sizes),
            );
        }

        let window = Arc::new(arc_swap::ArcSwap::from_pointee(reader.pin_with(non_finalized)));
        let (_follower, synced) = tokio::sync::watch::channel(true);
        let service = CompactBlockService::new(Served::new(Arc::clone(&window), synced));
        assert_eq!(service.tip(), Some(h(6)), "window extends the tip");

        // single read resolves on either side of the seam
        let single = |height| decode(&service.block(h(height)).expect("block"))[0].height;
        assert_eq!((single(2), single(5)), (2, 5));
        assert_eq!(service.latest_id().expect("latest").0, h(6), "latest comes from the window");

        // files → one window; non-finalized → one chunk per record
        let chunks = drain(service.range(h(2), h(6), Pools::ALL).expect("range"));
        assert_eq!(heights(&chunks), [2, 3, 4, 5, 6], "no gap or repeat at the seam");
        let per_chunk: Vec<usize> = chunks.iter().map(|chunk| decode(chunk).len()).collect();
        assert_eq!(per_chunk, [2, 1, 1, 1], "file span whole, non-finalized per block");

        let above = drain(service.range(h(5), h(6), Pools::ALL).expect("window only"));
        let below = drain(service.range(h(0), h(1), Pools::ALL).expect("finalised only"));
        assert_eq!((heights(&above), heights(&below)), (vec![5, 6], vec![0, 1]));

        // default pools either side of the seam = the full records projected on read
        let shielded = drain(service.range(h(2), h(6), Pools::default()).expect("shielded"));
        let projected: Vec<Bytes> =
            chunks.iter().map(|chunk| project(chunk, Pools::default()).expect("walks")).collect();
        assert_eq!(shielded, projected, "precomputed tip records = projection on read");
        assert_eq!(service.resident_block(h(5)), Ok(service.block(h(5)).ok()), "window: RAM");
        assert_eq!(service.resident_block(h(2)), Ok(None), "files: not resident");

        // hash confirmation reads both tiers' records
        assert!(service.block_at_hash(h(2), &[2u8; HASH]).is_ok(), "finalised hash");
        assert!(service.block_at_hash(h(5), &[5u8; HASH]).is_ok(), "window hash");

        // stream pinned before a reorg keeps serving its branch (reorg = the writer's `reset`:
        // the whole non-finalized tier goes, the files stay)
        let pinned = service.range(h(4), h(6), Pools::ALL).expect("range");
        window.store(Arc::new(reader.pin()));
        assert_eq!(heights(&drain(pinned)), [4, 5, 6], "pinned view survives the reorg");

        // fresh request: the files alone
        assert_eq!(service.tip(), Some(h(3)), "durable tip");
        assert!(service.block(h(3)).is_ok(), "finalised blocks are untouched");
        let gone = Err(ServeError::NotFound { height: h(4) });
        assert_eq!(service.block(h(4)), gone, "every non-finalized block gone, not only a fork's");
    }

    #[test]
    fn a_range_is_walked_in_bounded_windows_and_refused_past_the_maximum() {
        let service = service(8);

        // budget under one record: one record per window anyway (progress beats the bound)
        let chunks = drain(service.range_with_budget(h(0), h(7), Pools::ALL, 1).expect("range"));
        let sizes: Vec<Option<usize>> = chunks.iter().map(|chunk| framed_len(chunk)).collect();
        let whole: Vec<Option<usize>> = chunks.iter().map(|chunk| Some(chunk.len())).collect();
        assert_eq!(sizes, whole, "each window = exactly one framed record");
        assert_eq!(heights(&chunks), [0, 1, 2, 3, 4, 5, 6, 7], "in order, none lost");

        // budget > whole span → one window, same bytes
        let windowed = drain(service.range(h(0), h(7), Pools::ALL).expect("range"));
        assert_eq!(windowed.len(), 1, "unprojected span goes out whole");
        assert_eq!(windowed.concat(), chunks.concat(), "same bytes, one window");

        // max bounds the work → measured after the tip clamp (100 over an 8-block store = an
        // 8-block request); still over the cap = refused whole, never trimmed (reads as chain end)
        let capped = service.clone().with_max_range(NonZeroU32::new(4).expect("4 is non-zero"));
        let refused = capped.range(h(0), h(99), Pools::ALL).err();
        let too_large = Some(ServeError::RangeTooLarge { asked: 8, limit: 4 });
        assert_eq!(refused, too_large, "refused whole, naming the clamped span");
        let at_limit = drain(capped.range(h(0), h(3), Pools::ALL).expect("at the limit"));
        assert_eq!(heights(&at_limit), [0, 1, 2, 3]);

        // under the cap once clamped (open-ended `to` over a short chain = a small request)
        let roomy = service.with_max_range(NonZeroU32::new(8).expect("8 is non-zero"));
        let clamped = drain(roomy.range(h(0), h(99), Pools::ALL).expect("clamped to 8"));
        assert_eq!(heights(&clamped), [0, 1, 2, 3, 4, 5, 6, 7], "an end past the tip is fine");
    }

    /// Range bounds and pool projection: clamped at the tip, a start past it = a miss, a projected
    /// window = the same records with only the asked pools
    #[test]
    fn a_range_clamps_at_the_tip_and_projects_every_record_it_yields() {
        let service = service(6);

        let inner = drain(service.range(h(1), h(4), Pools::ALL).expect("range"));
        assert_eq!(heights(&inner), [1, 2, 3, 4], "exactly the asked heights, in order");

        let clamped = drain(service.range(h(4), h(99), Pools::ALL).expect("clamped"));
        assert_eq!(heights(&clamped), [4, 5], "past the tip = what exists, no error");

        let past_tip = service.range(h(6), h(7), Pools::ALL).err();
        assert_eq!(past_tip, Some(ServeError::NotFound { height: h(6) }), "starts past the tip");

        // shielded default: transparent pruned from every record, shielded pools intact
        let full = decode(&drain(service.range(h(0), h(2), Pools::ALL).expect("all")).concat());
        let shielded = drain(service.range(h(0), h(2), Pools::default()).expect("filtered"));
        assert_eq!(shielded.len(), 1, "projected span still one window");
        let shielded = decode(&shielded.concat());
        let stripped: Vec<cf::CompactBlock> = full
            .into_iter()
            .map(|mut block| {
                for tx in &mut block.vtx {
                    (tx.vin, tx.vout) = (Vec::new(), Vec::new());
                }
                block
            })
            .collect();
        assert_eq!(shielded, stripped, "only transparent dropped, every other field kept");
        assert!(stripped.iter().all(|block| !block.vtx[0].actions.is_empty()), "orchard present");
    }
}
