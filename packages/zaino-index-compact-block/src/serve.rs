//! RPC surface: `GetLatestBlock`, `GetBlock`, `GetBlockRange`
//!
//! - here, not in the server crate (an index disabled = its methods never compiled)
//! - answers in stored bytes, never decoded or re-encoded
//! - block / range = zero-copy [`Bytes`] slices of the mapping (range walked per [`SPAN_BUDGET`]
//!   window), pool pruning = framing walk

use std::sync::Arc;

use bytes::Bytes;
use zaino_primitives::types::Height;
use zaino_sync::Served;

use crate::{
    project::{project, project_reversed, record_hash},
    view::ReadView,
    Pools, HASH,
};

/// Ceiling on one range window (one readahead + one slice handed to the socket)
///
/// - `GetBlockRange` work per step bounded by this, not by range length
pub(crate) const SPAN_BUDGET: usize = 1 << 20;

/// Small (transport maps these onto gRPC codes; this crate names no transport)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// Until the index reaches the tip: any request past the durable tip, and `GetLatestBlock`
    /// (a partial tip answered = "no such block" and "not indexed yet" read the same)
    #[error("the compact-block index is still syncing")]
    Syncing,

    #[error("block {height} is not in the index")]
    NotFound { height: Height },

    #[error("block hash is not in the index")]
    HashNotFound,

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
}

impl CompactBlockService {
    /// Unsynced → [`ServeError::Syncing`] past the durable tip, committed heights answered
    pub fn new(served: Served<ReadView>) -> Self {
        Self { served }
    }

    /// Every tier pinned for one request or stream (one load); checked before any other
    /// validation (a syncing index = one answer, not one per request shape)
    fn pin(&self) -> Result<Arc<ReadView>, ServeError> {
        self.served.pin().ok_or(ServeError::Syncing)
    }

    /// [`pin`](Self::pin), or while syncing a view whose files reach `last` (durable = final: the
    /// producer stops on a contradiction, never rewrites)
    fn pin_through(&self, last: Height) -> Result<Arc<ReadView>, ServeError> {
        let view = self.served.pin_any();
        match Some(last) <= view.finalized_tip() || self.served.synced() {
            true => Ok(view),
            false => Err(ServeError::Syncing),
        }
    }

    /// Last height any tier can serve, inclusive, synced or not (non-finalized included: what
    /// `GetLatestBlock` answers once synced; `None` = nothing held)
    pub fn tip(&self) -> Option<Height> {
        self.served.pin_any().tip()
    }

    /// `GetBlock`: every pool, transparent included (only `GetBlockRange` filters: the protocol's
    /// asymmetry, lightwalletd's shape)
    pub fn block(&self, height: Height) -> Result<Bytes, ServeError> {
        self.pin_through(height)?.block(height).ok_or(ServeError::NotFound { height })
    }

    /// [`block`](Self::block) when the non-finalized tier holds it (RAM, no page read: a transport
    /// may answer inline); `Ok(None)` = ask [`block`](Self::block)
    pub fn resident_block(&self, height: Height) -> Result<Option<Bytes>, ServeError> {
        Ok(self.pin_through(height)?.resident_block(height))
    }

    /// `GetBlock` by hash, `height` located by the block-hash index
    ///
    /// - served only if this index holds `hash` there (independent publications: a reorg can
    ///   land between the locate and this read)
    pub fn block_at_hash(&self, height: Height, hash: &[u8; HASH]) -> Result<Bytes, ServeError> {
        let record = self.pin_through(height)?.block(height).ok_or(ServeError::HashNotFound)?;
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
    /// - `start > end` = descending, top down (proto: "decreasing height order")
    /// - no length cap (pepper-sync asks a whole shard, unbounded in blocks; work bounded per window)
    /// - no read here (the first file window = the cursor's first blocking step)
    /// - syncing: served only if the top (as asked, before the clamp) is committed (a range cut at
    ///   the durable tip = a wallet reading it as the chain tip)
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
        let descending = start > end;
        let (low, high) = if descending { (end, start) } else { (start, end) };
        let view = self.pin_through(high)?;
        let Some(tip) = view.tip().filter(|&tip| low <= tip) else {
            return Err(ServeError::NotFound { height: low });
        };

        // clamp, never refuse (a wallet asking past the tip wants what exists)
        let high = high.min(tip);
        let (next, last) = if descending { (high, low) } else { (low, high) };

        Ok(RangeCursor { view, budget, pools, descending, next: Some(next), last })
    }
}

/// `GetBlockRange` walked one chunk at a time: a file window below the seam, one non-finalized
/// record above it; either direction
///
/// - no `Iterator` impl (the caller routes a disk step to the blocking pool first)
/// - `view` pinned for the whole stream: every tier + the seam between them frozen
/// - `next` `None` = spent; `last` = final height served, inclusive
#[derive(Debug)]
pub struct RangeCursor {
    view: Arc<ReadView>,
    budget: usize,
    pools: Pools,
    descending: bool,
    next: Option<Height>,
    last: Height,
}

impl RangeCursor {
    /// Next chunk reads the files (a cold window faults: the blocking pool's step)
    pub fn next_touches_disk(&self) -> bool {
        self.next.is_some_and(|next| Some(next) <= self.view.finalized_tip())
    }

    /// Next wire chunk (framed records back to back, in walk order, projected to the cursor's
    /// pools), `None` once the range is spent
    pub fn next_chunk(&mut self) -> Option<Result<Bytes, ServeError>> {
        let height = self.next?;
        if !self.next_touches_disk() {
            // non-finalized: projected at apply for the default pools (every synced wallet's ask)
            self.step_past(height);
            let projected = self.view.resident_projected(height, self.pools);
            return Some(projected.ok_or(ServeError::NotFound { height }));
        }

        let window = match self.descending {
            false => {
                let files_end =
                    self.view.finalized_tip().map_or(self.last, |tip| tip.min(self.last));
                self.view.span_from(height, files_end, self.budget)
            }
            true => self.view.span_to(self.last, height, self.budget),
        };
        let Some((records, reached)) = window else {
            return Some(Err(ServeError::NotFound { height }));
        };
        self.step_past(reached);

        let projected = match self.descending {
            false => project(&records, self.pools),
            true => project_reversed(&records, self.pools),
        };
        Some(projected.ok_or(ServeError::Malformed { height }))
    }

    /// `next` = one past `reached` in walk order (`None` once `last` is served)
    fn step_past(&mut self, reached: Height) {
        self.next = match (reached == self.last, self.descending) {
            (true, _) => None,
            (false, false) => Some(reached.next()),
            (false, true) => reached.checked_sub(1),
        };
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
                sizes,
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

    /// `start > end` = lightwalletd's descending range: the ascending range's records, top down,
    /// across the seam (non-finalized first), clamped at the tip like the ascending one
    #[test]
    fn a_descending_range_serves_the_ascending_records_top_down_across_the_seam() {
        // finalized 0 to 3 (both inclusive), non-finalized from 4
        let reader = committed(4);
        let mut non_finalized = NonFinalizedState::default();
        for height in 4..7u32 {
            let (block, balances, sizes) = block(height);
            non_finalized.apply(
                h(height),
                [height as u8; HASH],
                encode_compact_block(&block, &balances, &sizes),
                sizes,
            );
        }
        let window = Arc::new(arc_swap::ArcSwap::from_pointee(reader.pin_with(non_finalized)));
        let (_follower, synced) = tokio::sync::watch::channel(true);
        let service = CompactBlockService::new(Served::new(window, synced));

        let ascending = drain(service.range(h(1), h(6), Pools::ALL).expect("ascending"));
        let descending = drain(service.range(h(6), h(1), Pools::ALL).expect("descending"));
        let mut reversed = decode(&ascending.concat());
        reversed.reverse();
        assert_eq!(decode(&descending.concat()), reversed, "6..=1 = 1..=6 reversed");
        let per_chunk: Vec<usize> = descending.iter().map(|chunk| decode(chunk).len()).collect();
        assert_eq!(per_chunk, [1, 1, 1, 3], "non-finalized per block, then one file window");

        // budget under one record: one record per file window, still top down
        let narrow = service.range_with_budget(h(6), h(1), Pools::ALL, 1).expect("narrow");
        assert_eq!(heights(&drain(narrow)), [6, 5, 4, 3, 2, 1]);

        let clamped = drain(service.range(h(99), h(4), Pools::ALL).expect("clamped"));
        assert_eq!(heights(&clamped), [6, 5, 4], "top past the tip = from the tip down");
        let past_tip = service.range(h(99), h(7), Pools::ALL).err();
        assert_eq!(past_tip, Some(ServeError::NotFound { height: h(7) }), "bottom past the tip");

        let shielded = drain(service.range(h(6), h(1), Pools::default()).expect("shielded"));
        let projected: Vec<Bytes> = descending
            .iter()
            .map(|chunk| project(chunk, Pools::default()).expect("walks"))
            .collect();
        assert_eq!(shielded, projected, "projected chunk for chunk, order kept");
    }

    /// Syncing: committed heights final → answered; anything reaching past them = `Syncing`, never
    /// a cut or a miss (either reads as the chain's end)
    #[test]
    fn a_syncing_index_serves_only_what_it_has_committed() {
        // finalized 0 to 3 (both inclusive), non-finalized from 4
        let reader = committed(4);
        let mut non_finalized = NonFinalizedState::default();
        for height in 4..7u32 {
            let (block, balances, sizes) = block(height);
            non_finalized.apply(
                h(height),
                [height as u8; HASH],
                encode_compact_block(&block, &balances, &sizes),
                sizes,
            );
        }
        let window = Arc::new(arc_swap::ArcSwap::from_pointee(reader.pin_with(non_finalized)));
        let (synced, synced_rx) = tokio::sync::watch::channel(false);
        let service = CompactBlockService::new(Served::new(Arc::clone(&window), synced_rx));

        let single = |height| decode(&service.block(h(height)).expect("committed"))[0].height;
        assert_eq!((single(0), single(3)), (0, 3), "both ends of the files");
        assert!(service.block_at_hash(h(2), &[2u8; HASH]).is_ok(), "committed, by hash");
        assert_eq!(service.resident_block(h(2)), Ok(None), "committed: not resident, ask block");
        let committed = drain(service.range(h(1), h(3), Pools::ALL).expect("committed range"));
        assert_eq!(heights(&committed), [1, 2, 3]);

        let syncing = Err(ServeError::Syncing);
        assert_eq!(service.block(h(4)), syncing, "non-finalized: a reorg can still take it");
        assert_eq!(service.block(h(99)), syncing, "past every tier: not a NotFound");
        assert_eq!(service.resident_block(h(5)), Err(ServeError::Syncing), "resident, not final");
        assert_eq!(service.block_at_hash(h(5), &[5u8; HASH]), syncing);
        assert_eq!(service.latest_id(), Err(ServeError::Syncing), "tip mid-sync != chain tip");
        for (start, end) in [(2, 4), (3, 99), (4, 6), (4, 2), (99, 3)] {
            let refused = service.range(h(start), h(end), Pools::ALL).err();
            assert_eq!(refused, Some(ServeError::Syncing), "range {start}..={end}: no cut at 3");
        }
        let committed_down = drain(service.range(h(3), h(1), Pools::ALL).expect("committed, down"));
        assert_eq!(heights(&committed_down), [3, 2, 1], "descending within the files");

        synced.send(true).expect("service holds the receiver");
        assert_eq!(decode(&service.block(h(5)).expect("synced"))[0].height, 5);
        assert_eq!(service.latest_id().expect("synced").0, h(6));
        let clamped = drain(service.range(h(3), h(99), Pools::ALL).expect("synced range"));
        assert_eq!(heights(&clamped), [3, 4, 5, 6], "synced: clamped at the tip, not refused");
    }

    #[test]
    fn a_range_is_walked_in_bounded_windows() {
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
