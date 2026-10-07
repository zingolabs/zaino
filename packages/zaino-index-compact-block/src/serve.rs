//! RPC surface: `GetLatestBlock`, `GetBlock`, `GetBlockRange`
//!
//! - here, not in the server crate (an index disabled = its methods never compiled)
//! - answers in stored bytes, never decoded or re-encoded
//! - block / range = zero-copy [`Bytes`] slices of the mapping (range walked per [`WINDOW_BYTES`]
//!   window), pool pruning = framing walk
//! - held/committed seam = the pinned reader's: a commit landing mid-stream cannot move it

use std::sync::Arc;

use bytes::Bytes;
use zaino_persistence::{LayeredView, SequenceRead, View};
use zaino_primitives::types::Height;
use zaino_sync::Served;

use crate::{
    project::{project, record_hash},
    CompactBlockReader, Pools, HASH,
};

/// Ceiling on one range window's records (one chunk handed to the socket)
const WINDOW_BYTES: usize = 1 << 20;

/// Pinned reader over both tiers: committed records up to the durable tip, held ones above
type Pinned<V> = Arc<CompactBlockReader<LayeredView<V>>>;

/// Tier seam (serving only: gone with `Tiered`)
impl<V: SequenceRead> CompactBlockReader<LayeredView<V>> {
    /// Last committed height, inclusive (`None` = nothing committed)
    fn durable_height(&self) -> Option<Height> {
        self.view().durable().tip().map(|tip| tip.height)
    }

    /// Held above the committed tip: RAM, no page touched (`None` = not held, maybe committed)
    pub(crate) fn resident_block(&self, height: Height) -> Option<Bytes> {
        (Some(height) > self.durable_height()).then(|| self.block(height)).flatten()
    }
}

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

/// Two tiers, one surface: committed records up to the finalised tip, held ones above it
///
/// - a request pins **both at once** (one reader, one load): the seam cannot move mid-stream
#[derive(Debug, Clone)]
pub struct CompactBlockService<V> {
    served: Served<CompactBlockReader<LayeredView<V>>>,
}

impl<V: SequenceRead> CompactBlockService<V> {
    /// Unsynced → [`ServeError::Syncing`] past the durable tip, committed heights answered
    pub fn new(served: Served<CompactBlockReader<LayeredView<V>>>) -> Self {
        Self { served }
    }

    /// Every tier pinned for one request or stream (one load); checked before any other
    /// validation (a syncing index = one answer, not one per request shape)
    fn pin(&self) -> Result<Pinned<V>, ServeError> {
        self.served.pin().ok_or(ServeError::Syncing)
    }

    /// [`pin`](Self::pin), or while syncing a reader whose files reach `last` (durable = final:
    /// the producer stops on a contradiction, never rewrites)
    fn pin_through(&self, last: Height) -> Result<Pinned<V>, ServeError> {
        let reader = self.served.pin_any();
        match Some(last) <= reader.durable_height() || self.served.synced() {
            true => Ok(reader),
            false => Err(ServeError::Syncing),
        }
    }

    /// Last height any tier can serve, inclusive, synced or not (non-finalized included: what
    /// `GetLatestBlock` answers once synced; `None` = nothing held)
    pub fn tip(&self) -> Option<Height> {
        self.served.pin_any().tip().map(|tip| tip.height)
    }

    /// `GetBlock`: every pool, transparent included (only `GetBlockRange` filters: the protocol's
    /// asymmetry, lightwalletd's shape)
    pub fn block(&self, height: Height) -> Result<Bytes, ServeError> {
        self.pin_through(height)?.block(height).ok_or(ServeError::NotFound { height })
    }

    /// [`block`](Self::block) when held above the committed tip (RAM, no page read: a transport
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
    /// - the reader's tip: RAM, no page read (a transport may answer inline)
    pub fn latest_id(&self) -> Result<(Height, [u8; HASH]), ServeError> {
        let tip = self.pin()?.tip().ok_or(ServeError::Empty)?;
        Ok((tip.height, tip.hash.into()))
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
    ) -> Result<RangeCursor<V>, ServeError> {
        self.range_with_budget(start, end, pools, WINDOW_BYTES)
    }

    /// [`range`](Self::range) with an explicit window size (a test forces a refill)
    pub(crate) fn range_with_budget(
        &self,
        start: Height,
        end: Height,
        pools: Pools,
        budget: usize,
    ) -> Result<RangeCursor<V>, ServeError> {
        let descending = start > end;
        let (low, high) = if descending { (end, start) } else { (start, end) };
        let reader = self.pin_through(high)?;
        let Some(tip) = reader.tip().map(|tip| tip.height).filter(|&tip| low <= tip) else {
            return Err(ServeError::NotFound { height: low });
        };

        // clamp, never refuse (a wallet asking past the tip wants what exists)
        let high = high.min(tip);
        let (next, last) = if descending { (high, low) } else { (low, high) };

        Ok(RangeCursor { reader, budget, pools, descending, next: Some(next), last })
    }
}

/// `GetBlockRange` walked one chunk at a time: a file window below the seam, one held record
/// above it; either direction
///
/// - no `Iterator` impl (the caller routes a disk step to the blocking pool first)
/// - `reader` pinned for the whole stream: every tier + the seam between them frozen
/// - `next` `None` = spent; `last` = final height served, inclusive
pub struct RangeCursor<V> {
    reader: Pinned<V>,
    budget: usize,
    pools: Pools,
    descending: bool,
    next: Option<Height>,
    last: Height,
}

impl<V: View> std::fmt::Debug for RangeCursor<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RangeCursor")
            .field("reader", &self.reader)
            .field("pools", &self.pools)
            .field("descending", &self.descending)
            .field("next", &self.next)
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

impl<V: SequenceRead> RangeCursor<V> {
    /// Next chunk reads the files (a cold window faults: the blocking pool's step)
    pub fn next_touches_disk(&self) -> bool {
        self.next.is_some_and(|next| Some(next) <= self.reader.durable_height())
    }

    /// Next wire chunk (framed records back to back, in walk order, projected to the cursor's
    /// pools), `None` once the range is spent
    pub fn next_chunk(&mut self) -> Option<Result<Bytes, ServeError>> {
        let height = self.next?;
        if !self.next_touches_disk() {
            // held: one record per chunk, projected on read
            self.step_past(height);
            let Some(record) = self.reader.resident_block(height) else {
                return Some(Err(ServeError::NotFound { height }));
            };
            let projected = project(std::slice::from_ref(&record), self.pools);
            return Some(projected.ok_or(ServeError::Malformed { height }));
        }

        // descending: every height left sits in the files (the held ones went first)
        let files_last = match self.descending {
            false => self.reader.durable_height().map_or(self.last, |tip| tip.min(self.last)),
            true => self.last,
        };
        let (records, reached) = self.reader.range(height, files_last, self.budget);
        self.step_past(reached);
        Some(project(&records, self.pools).ok_or(ServeError::Malformed { height }))
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
    use std::num::NonZeroUsize;

    use super::*;
    use crate::{
        fold,
        reader::WINDOW_RECORDS,
        schema,
        testing::{block, committed},
    };
    use prost::Message;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine, Tiered,
    };
    use zaino_proto::frame::{framed_len, split_frame};
    use zaino_proto::proto::compact_formats as cf;
    use zcash_protocol::consensus::NetworkType;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// `testing::block(height)`'s hash
    fn hash_at(height: u32) -> [u8; HASH] {
        block(height).0.header().hash.into()
    }

    fn store() -> DiskStore {
        let schema = schema(NetworkType::Regtest);
        DiskEngine::new(SimFs::new()).open(std::path::Path::new("/cb"), &schema).expect("open")
    }

    fn service(blocks: u32) -> CompactBlockService<DiskView> {
        CompactBlockService::new(Served::fixed(committed(store(), blocks)))
    }

    /// `testing::block(0..=3)` committed, `4..=6` applied above them
    fn four_committed_three_applied() -> Tiered<DiskStore> {
        let mut tiered = Tiered::new(store(), NonZeroUsize::MAX);
        for height in 0..7u32 {
            let (block, fees) = block(height);
            let parent = CompactBlockReader::new(tiered.view(), NetworkType::Regtest);
            let changes = fold(&parent, &block, &fees).expect("small tree sizes");
            match height {
                0..=3 => assert!(!tiered.stage(changes, 0), "one commit for all four"),
                _ => tiered.apply(changes),
            }
            if height == 3 {
                tiered.finalize(h(3));
            }
        }
        tiered
    }

    /// Every framed record a chunk carries, decoded
    fn decode(chunk: &[u8]) -> Vec<cf::CompactBlock> {
        let mut blocks = Vec::new();
        let mut rest = chunk;
        while !rest.is_empty() {
            let (message, tail) = split_frame(rest).expect("whole frame");
            blocks.push(cf::CompactBlock::decode(message).expect("message"));
            rest = tail;
        }
        blocks
    }

    /// Cursor walked to the end: the chunks it yielded (a spent cursor must stay spent)
    fn drain(mut cursor: RangeCursor<DiskView>) -> Vec<Bytes> {
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

        let by_hash = service.block_at_hash(h(2), &hash_at(2)).expect("by hash");
        assert_eq!(by_hash, service.block(h(2)).expect("by height"));
        let other = service.block_at_hash(h(2), &hash_at(3));
        assert_eq!(other, Err(ServeError::HashNotFound), "another block at the located height");
        let past = service.block_at_hash(h(9), &hash_at(9));
        assert_eq!(past, Err(ServeError::HashNotFound), "a height past the tip");
    }

    /// Range crossing the tier seam (where an off-by-one hides): every height exactly once, in
    /// order, no duplicate or gap at the boundary
    #[test]
    fn a_range_spans_the_file_store_and_the_window_without_a_seam() {
        // finalized 0 to 3 (both inclusive), applied from 4
        let mut tiered = four_committed_three_applied();
        let both = CompactBlockReader::new(tiered.view(), NetworkType::Regtest);
        let window = Arc::new(arc_swap::ArcSwap::from_pointee(both));
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
        let records: Vec<Bytes> =
            (2..=6).map(|height| service.block(h(height)).expect("held")).collect();
        let projected = project(&records, Pools::default()).expect("walks");
        assert_eq!(shielded.concat(), projected, "held records = the committed projection");
        assert_eq!(shielded.len(), chunks.len(), "same chunks as the unprojected range");
        assert_eq!(service.resident_block(h(5)), Ok(service.block(h(5)).ok()), "window: RAM");
        assert_eq!(service.resident_block(h(2)), Ok(None), "files: not resident");

        // hash confirmation reads both tiers' records
        assert!(service.block_at_hash(h(2), &hash_at(2)).is_ok(), "finalised hash");
        assert!(service.block_at_hash(h(5), &hash_at(5)).is_ok(), "window hash");

        // stream pinned before a reorg: still its branch (every applied block gone, files kept)
        let pinned = service.range(h(4), h(6), Pools::ALL).expect("range");
        tiered.reorg();
        window.store(Arc::new(CompactBlockReader::new(tiered.view(), NetworkType::Regtest)));
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
        // finalized 0 to 3 (both inclusive), applied from 4
        let both =
            CompactBlockReader::new(four_committed_three_applied().view(), NetworkType::Regtest);
        let window = Arc::new(arc_swap::ArcSwap::from_pointee(both));
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
        let records: Vec<Bytes> =
            (1..=6).rev().map(|height| service.block(h(height)).expect("held")).collect();
        let projected = project(&records, Pools::default()).expect("walks");
        assert_eq!(shielded.concat(), projected, "projected record for record, order kept");
        assert_eq!(shielded.len(), descending.len(), "same chunks as the unprojected range");
    }

    /// Syncing: committed heights final → answered; anything reaching past them = `Syncing`, never
    /// a cut or a miss (either reads as the chain's end)
    #[test]
    fn a_syncing_index_serves_only_what_it_has_committed() {
        // finalized 0 to 3 (both inclusive), applied from 4
        let both =
            CompactBlockReader::new(four_committed_three_applied().view(), NetworkType::Regtest);
        let window = Arc::new(arc_swap::ArcSwap::from_pointee(both));
        let (synced, synced_rx) = tokio::sync::watch::channel(false);
        let service = CompactBlockService::new(Served::new(Arc::clone(&window), synced_rx));

        let single = |height| decode(&service.block(h(height)).expect("committed"))[0].height;
        assert_eq!((single(0), single(3)), (0, 3), "both ends of the files");
        assert!(service.block_at_hash(h(2), &hash_at(2)).is_ok(), "committed, by hash");
        assert_eq!(service.resident_block(h(2)), Ok(None), "committed: not resident, ask block");
        let committed = drain(service.range(h(1), h(3), Pools::ALL).expect("committed range"));
        assert_eq!(heights(&committed), [1, 2, 3]);

        let syncing = Err(ServeError::Syncing);
        assert_eq!(service.block(h(4)), syncing, "non-finalized: a reorg can still take it");
        assert_eq!(service.block(h(99)), syncing, "past every tier: not a NotFound");
        assert_eq!(service.resident_block(h(5)), Err(ServeError::Syncing), "resident, not final");
        assert_eq!(service.block_at_hash(h(5), &hash_at(5)), syncing);
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

        // records per window capped under any budget, either direction
        let tip = WINDOW_RECORDS + 1;
        let long = CompactBlockService::new(Served::fixed(committed(store(), tip + 1)));
        let up = drain(long.range(h(0), h(tip), Pools::ALL).expect("ascending"));
        let down = drain(long.range(h(tip), h(0), Pools::ALL).expect("descending"));
        let per_chunk = |chunks: &[Bytes]| -> Vec<usize> {
            chunks.iter().map(|chunk| decode(chunk).len()).collect()
        };
        let capped = vec![WINDOW_RECORDS as usize, 2];
        assert_eq!((per_chunk(&up), per_chunk(&down)), (capped.clone(), capped));
        let top_down: Vec<u64> = (0..=u64::from(tip)).rev().collect();
        assert_eq!(heights(&down), top_down, "no gap or repeat at the window edge");
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
