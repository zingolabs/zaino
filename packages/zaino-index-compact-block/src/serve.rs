//! Reads `GetBlock` and `GetBlockRange` answer with, over one reader (one snapshot's)
//!
//! - answers in stored bytes, never decoded or re-encoded
//! - block / range = zero-copy [`Bytes`] slices of the mapping (range walked per [`WINDOW_BYTES`]
//!   window), pool pruning = framing walk
//! - layer/committed seam = the reader's: a commit landing mid-stream cannot move it

use bytes::Bytes;
use zaino_persistence::{LayeredView, SequenceRead, View};
use zaino_primitives::types::Height;

use crate::{
    project::{project, record_hash},
    CompactBlockReader, Pools, HASH,
};

/// Ceiling on one range window's records (one chunk handed to the socket)
const WINDOW_BYTES: usize = 1 << 20;

/// Small (transport maps these onto gRPC codes; this crate names no transport)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("block {height} is not in the index")]
    NotFound { height: Height },

    #[error("block hash is not in the index")]
    HashNotFound,

    /// Stored record that will not walk (corruption, not a bad request)
    #[error("stored record at height {height} is malformed")]
    Malformed { height: Height },
}

impl<V: SequenceRead> CompactBlockReader<V> {
    /// `GetBlock` by hash, `height` located by the block-hash index: the record there only if it
    /// is `hash`'s (the locator and this index may hold different blocks there)
    pub fn block_at(&self, height: Height, hash: &[u8; HASH]) -> Result<Bytes, ServeError> {
        let record = self.block(height).ok_or(ServeError::HashNotFound)?;
        match record_hash(&record) {
            Some(held) if held == *hash => Ok(record),
            Some(_) => Err(ServeError::HashNotFound),
            None => Err(ServeError::Malformed { height }),
        }
    }
}

/// A snapshot's seam: its layer above the committed records
impl<V: SequenceRead> CompactBlockReader<LayeredView<V>> {
    /// Last committed height, inclusive (`None` = nothing committed)
    fn committed(&self) -> Option<Height> {
        self.view().durable().tip().map(|tip| tip.height)
    }

    /// In the layer above the committed records: RAM, no page touched (`None` = ask `block`)
    pub fn resident_block(&self, height: Height) -> Option<Bytes> {
        (Some(height) > self.committed()).then(|| self.block(height)).flatten()
    }
}

/// `GetBlockRange` walked one chunk at a time: a file window below the seam, one layer record
/// above it; either direction
///
/// - no `Iterator` impl (the caller routes a disk step to the blocking pool first)
/// - `reader` held for the whole stream: committed records, layer and seam frozen
/// - `next` `None` = spent; `last` = final height served, inclusive
pub struct RangeCursor<V> {
    reader: CompactBlockReader<LayeredView<V>>,
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
    /// Heights `start` to `end`, both inclusive, served no higher than `tip` (the snapshot's)
    ///
    /// - `start > end` = descending, top down (proto: "decreasing height order")
    /// - top past `tip` = clamped, never refused (a wallet asking past the tip wants what exists)
    /// - no length cap (pepper-sync asks a whole shard; work bounded per window)
    /// - no read here (the first file window = the cursor's first blocking step)
    pub fn new(
        reader: CompactBlockReader<LayeredView<V>>,
        start: Height,
        end: Height,
        tip: Height,
        pools: Pools,
    ) -> Result<Self, ServeError> {
        Self::with_budget(reader, start, end, tip, pools, WINDOW_BYTES)
    }

    /// [`new`](Self::new) with an explicit window size (a test forces a refill)
    pub(crate) fn with_budget(
        reader: CompactBlockReader<LayeredView<V>>,
        start: Height,
        end: Height,
        tip: Height,
        pools: Pools,
        budget: usize,
    ) -> Result<Self, ServeError> {
        let descending = start > end;
        let (low, high) = if descending { (end, start) } else { (start, end) };
        let held = reader.tip().map(|held| held.height.min(tip));
        let Some(top) = held.filter(|&top| low <= top) else {
            return Err(ServeError::NotFound { height: low });
        };
        let high = high.min(top);
        let (next, last) = if descending { (high, low) } else { (low, high) };

        Ok(Self { reader, budget, pools, descending, next: Some(next), last })
    }

    /// Next chunk reads the files (a cold window faults: the blocking pool's step)
    pub fn next_touches_disk(&self) -> bool {
        self.next.is_some_and(|next| Some(next) <= self.reader.committed())
    }

    /// Next wire chunk (framed records back to back, in walk order, projected to the cursor's
    /// pools), `None` once the range is spent
    pub fn next_chunk(&mut self) -> Option<Result<Bytes, ServeError>> {
        let height = self.next?;
        if !self.next_touches_disk() {
            // layer: one record per chunk, projected on read
            self.step_past(height);
            let Some(record) = self.reader.resident_block(height) else {
                return Some(Err(ServeError::NotFound { height }));
            };
            let projected = project(std::slice::from_ref(&record), self.pools);
            return Some(projected.ok_or(ServeError::Malformed { height }));
        }

        // descending: every height left sits in the files (the layer's went first)
        let files_last = match self.descending {
            false => self.reader.committed().map_or(self.last, |tip| tip.min(self.last)),
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
    use super::*;
    use crate::{
        fold,
        reader::WINDOW_RECORDS,
        schema,
        testing::{block, committed},
    };
    use prost::Message;
    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine, Store};
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

    /// `testing::block(0..count)` committed, read as a snapshot reads it (nothing above)
    fn reader(count: u32) -> CompactBlockReader<LayeredView<DiskView>> {
        CompactBlockReader::new(committed(store(), count).staged(), NetworkType::Regtest)
    }

    /// `testing::block(0..=3)` committed, `4..=6` in the layer above them
    fn four_committed_three_above() -> CompactBlockReader<LayeredView<DiskView>> {
        let mut store = committed(store(), 4);
        for height in 4..7u32 {
            let (block, fees) = block(height);
            let parent = CompactBlockReader::new(store.staged(), NetworkType::Regtest);
            store.apply(fold(&parent, &block, &fees).expect("small tree sizes"));
        }
        CompactBlockReader::new(store.staged(), NetworkType::Regtest)
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
    fn single_block_reads_carry_every_pool_and_confirm_the_located_hash() {
        let reader = reader(4);

        let body = reader.block(h(2)).expect("block 2");
        let [decoded] = decode(&body).try_into().expect("one record");
        assert_eq!(decoded.height, 2);

        // GetBlock unfiltered, unlike GetBlockRange
        assert_eq!(decoded.vtx[0].vin.len(), 1, "transparent present");
        assert_eq!(decoded.vtx[0].ironwood_actions.len(), 2, "ironwood present");
        assert_eq!(body[0], 0, "gRPC compression flag");
        assert_eq!(framed_len(&body), Some(body.len()), "frame length = the message it carries");

        let tip = reader.tip().expect("held");
        let [stored] = decode(&reader.block(h(3)).expect("tip")).try_into().expect("one record");
        assert_eq!((tip.height, <[u8; HASH]>::from(tip.hash).to_vec()), (h(3), stored.hash));
        assert_eq!(reader.block(h(4)), None);

        assert_eq!(reader.block_at(h(2), &hash_at(2)), Ok(body), "by hash = by height");
        let other = reader.block_at(h(2), &hash_at(3));
        assert_eq!(other, Err(ServeError::HashNotFound), "another block at the located height");
        let past = reader.block_at(h(9), &hash_at(9));
        assert_eq!(past, Err(ServeError::HashNotFound), "a height past the tip");
    }

    /// Range crossing the seam (where an off-by-one hides): every height exactly once, in order,
    /// no duplicate or gap at the boundary; files in one window, the layer per record
    #[test]
    fn a_range_spans_the_committed_records_and_the_layer_without_a_seam() {
        // committed 0 to 3 (both inclusive), layer from 4
        let reader = four_committed_three_above();
        let range = |start, end, pools| {
            RangeCursor::new(reader.clone(), h(start), h(end), h(6), pools).expect("range")
        };
        let single = |height| decode(&reader.block(h(height)).expect("block"))[0].height;
        assert_eq!((single(2), single(5)), (2, 5));

        let chunks = drain(range(2, 6, Pools::ALL));
        assert_eq!(heights(&chunks), [2, 3, 4, 5, 6], "no gap or repeat at the seam");
        let per_chunk: Vec<usize> = chunks.iter().map(|chunk| decode(chunk).len()).collect();
        assert_eq!(per_chunk, [2, 1, 1, 1], "file span whole, layer per block");

        let (above, below) = (drain(range(5, 6, Pools::ALL)), drain(range(0, 1, Pools::ALL)));
        assert_eq!((heights(&above), heights(&below)), (vec![5, 6], vec![0, 1]));

        // default pools either side of the seam = the full records projected on read
        let shielded = drain(range(2, 6, Pools::default()));
        let records: Vec<Bytes> =
            (2..=6).map(|height| reader.block(h(height)).expect("held")).collect();
        let projected = project(&records, Pools::default()).expect("walks");
        assert_eq!(shielded.concat(), projected, "layer records = the committed projection");
        assert_eq!(shielded.len(), chunks.len(), "same chunks as the unprojected range");
        assert_eq!(reader.resident_block(h(5)), reader.block(h(5)), "layer: RAM");
        assert_eq!(reader.resident_block(h(2)), None, "files: not resident");
        assert!(reader.block_at(h(5), &hash_at(5)).is_ok(), "layer hash");
    }

    /// `start > end` = lightwalletd's descending range: the ascending range's records, top down,
    /// across the seam (layer first), clamped at the tip like the ascending one
    #[test]
    fn a_descending_range_serves_the_ascending_records_top_down_across_the_seam() {
        let reader = four_committed_three_above();
        let range =
            |start, end, pools| RangeCursor::new(reader.clone(), h(start), h(end), h(6), pools);

        let ascending = drain(range(1, 6, Pools::ALL).expect("ascending"));
        let descending = drain(range(6, 1, Pools::ALL).expect("descending"));
        let mut reversed = decode(&ascending.concat());
        reversed.reverse();
        assert_eq!(decode(&descending.concat()), reversed, "6..=1 = 1..=6 reversed");
        let per_chunk: Vec<usize> = descending.iter().map(|chunk| decode(chunk).len()).collect();
        assert_eq!(per_chunk, [1, 1, 1, 3], "layer per block, then one file window");

        // budget under one record: one record per file window, still top down
        let narrow = RangeCursor::with_budget(reader.clone(), h(6), h(1), h(6), Pools::ALL, 1);
        assert_eq!(heights(&drain(narrow.expect("narrow"))), [6, 5, 4, 3, 2, 1]);

        let clamped = drain(range(99, 4, Pools::ALL).expect("clamped"));
        assert_eq!(heights(&clamped), [6, 5, 4], "top past the tip = from the tip down");
        let past_tip = range(99, 7, Pools::ALL).err();
        assert_eq!(past_tip, Some(ServeError::NotFound { height: h(7) }), "bottom past the tip");

        let shielded = drain(range(6, 1, Pools::default()).expect("shielded"));
        let records: Vec<Bytes> =
            (1..=6).rev().map(|height| reader.block(h(height)).expect("held")).collect();
        let projected = project(&records, Pools::default()).expect("walks");
        assert_eq!(shielded.concat(), projected, "projected record for record, order kept");
        assert_eq!(shielded.len(), descending.len(), "same chunks as the unprojected range");
    }

    /// A reader past the served tip (a root snapshot during bulk sync) serves to the tip only:
    /// clamped either direction, a range starting past it = a miss
    #[test]
    fn a_range_never_serves_past_the_snapshot_tip() {
        let reader = four_committed_three_above();
        let range =
            |start, end| RangeCursor::new(reader.clone(), h(start), h(end), h(4), Pools::ALL);

        assert_eq!(heights(&drain(range(2, 99).expect("up"))), [2, 3, 4]);
        assert_eq!(heights(&drain(range(99, 3).expect("down"))), [4, 3]);
        assert_eq!(range(5, 6).err(), Some(ServeError::NotFound { height: h(5) }));
    }

    #[test]
    fn a_range_is_walked_in_bounded_windows() {
        let reader = reader(8);

        // budget under one record: one record per window anyway (progress beats the bound)
        let one = RangeCursor::with_budget(reader.clone(), h(0), h(7), h(7), Pools::ALL, 1);
        let chunks = drain(one.expect("range"));
        let sizes: Vec<Option<usize>> = chunks.iter().map(|chunk| framed_len(chunk)).collect();
        let whole: Vec<Option<usize>> = chunks.iter().map(|chunk| Some(chunk.len())).collect();
        assert_eq!(sizes, whole, "each window = exactly one framed record");
        assert_eq!(heights(&chunks), [0, 1, 2, 3, 4, 5, 6, 7], "in order, none lost");

        // budget > whole span → one window, same bytes
        let windowed =
            drain(RangeCursor::new(reader, h(0), h(7), h(7), Pools::ALL).expect("range"));
        assert_eq!(windowed.len(), 1, "unprojected span goes out whole");
        assert_eq!(windowed.concat(), chunks.concat(), "same bytes, one window");

        // records per window capped under any budget, either direction
        let tip = WINDOW_RECORDS + 1;
        let long = self::reader(tip + 1);
        let up =
            drain(RangeCursor::new(long.clone(), h(0), h(tip), h(tip), Pools::ALL).expect("up"));
        let down = drain(RangeCursor::new(long, h(tip), h(0), h(tip), Pools::ALL).expect("down"));
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
        let reader = reader(6);
        let range =
            |start, end, pools| RangeCursor::new(reader.clone(), h(start), h(end), h(5), pools);

        let inner = drain(range(1, 4, Pools::ALL).expect("range"));
        assert_eq!(heights(&inner), [1, 2, 3, 4], "exactly the asked heights, in order");

        let clamped = drain(range(4, 99, Pools::ALL).expect("clamped"));
        assert_eq!(heights(&clamped), [4, 5], "past the tip = what exists, no error");

        let past_tip = range(6, 7, Pools::ALL).err();
        assert_eq!(past_tip, Some(ServeError::NotFound { height: h(6) }), "starts past the tip");

        // shielded default: transparent pruned from every record, shielded pools intact
        let full = decode(&drain(range(0, 2, Pools::ALL).expect("all")).concat());
        let shielded = drain(range(0, 2, Pools::default()).expect("filtered"));
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
