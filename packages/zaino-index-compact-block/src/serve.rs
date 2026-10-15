//! Reads `GetBlock` and `GetBlockRange` answer with, over one reader (one `GrpcSnapshot`'s)
//!
//! - answers in stored bytes, never decoded or re-encoded
//! - block / range = zero-copy [`Bytes`] slices of the mapping (range walked per [`WINDOW_BYTES`]
//!   window), pool pruning = framing walk
//! - layer/durable seam = the reader's: a commit landing mid-stream cannot move it

use bytes::Bytes;
use zaino_persistence::{OverlayView, SequenceRead, View};
use zcash_protocol::consensus::BlockHeight;

use crate::{
    project::{project, record_hash},
    CompactBlockReader, Pools, HASH,
};

/// Ceiling on one range window's records (one chunk handed to the socket)
const WINDOW_BYTES: usize = 1 << 20;

/// Small (transport maps these onto gRPC codes)
///
/// - no transport named in this crate
/// - `Malformed` = stored record that will not walk (corruption, not a bad request)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    #[error("block {height} is not in the index")]
    NotFound { height: BlockHeight },

    #[error("block hash is not in the index")]
    HashNotFound,

    #[error("stored record at height {height} is malformed")]
    Malformed { height: BlockHeight },
}

impl<V: SequenceRead> CompactBlockReader<V> {
    /// `GetBlock` by hash, `height` located by the block-hash index: the record there only if it
    /// is `hash`'s (the locator and this index may hold different blocks there)
    pub fn block_at(&self, height: BlockHeight, hash: &[u8; HASH]) -> Result<Bytes, ServeError> {
        let record = self.block(height).ok_or(ServeError::HashNotFound)?;
        match record_hash(&record) {
            Some(held) if held == *hash => Ok(record),
            Some(_) => Err(ServeError::HashNotFound),
            None => Err(ServeError::Malformed { height }),
        }
    }
}

/// NFS publication's seam: its layer above the durable records
impl<V: SequenceRead> CompactBlockReader<OverlayView<V>> {
    /// Last durable height, inclusive (`None` = nothing durable)
    fn durable(&self) -> Option<BlockHeight> {
        self.view().durable().tip().map(|tip| tip.height)
    }

    /// In the layer above the durable records: RAM, no page touched (`None` = ask `block`)
    pub fn resident_block(&self, height: BlockHeight) -> Option<Bytes> {
        (Some(height) > self.durable()).then(|| self.block(height)).flatten()
    }
}

/// `GetBlockRange` one chunk at a time, either direction: file window below the seam, layer record
/// above it
/// - no `Iterator` impl (the caller routes a disk step to the blocking pool first)
/// - `reader` held for the whole stream: durable records, layer and seam frozen
/// - `next` `None` = spent
/// - `last` = finalized height served, inclusive
pub struct RangeCursor<V> {
    reader: CompactBlockReader<OverlayView<V>>,
    budget: usize,
    pools: Pools,
    descending: bool,
    next: Option<BlockHeight>,
    last: BlockHeight,
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
    /// Heights `start` to `end`, both inclusive, served no higher than `tip` (the served tip)
    ///
    /// - `start > end` = descending, top down (proto: "decreasing height order")
    /// - top past `tip` = clamped, never refused (a wallet asking past the tip wants what exists)
    /// - no length cap (pepper-sync asks a whole shard)
    /// - work bounded per window
    /// - no read here (the first file window = the cursor's first blocking step)
    pub fn new(
        reader: CompactBlockReader<OverlayView<V>>,
        start: BlockHeight,
        end: BlockHeight,
        tip: BlockHeight,
        pools: Pools,
    ) -> Result<Self, ServeError> {
        Self::with_budget(reader, start, end, tip, pools, WINDOW_BYTES)
    }

    /// [`new`](Self::new) with an explicit window size (a test forces a refill)
    pub(crate) fn with_budget(
        reader: CompactBlockReader<OverlayView<V>>,
        start: BlockHeight,
        end: BlockHeight,
        tip: BlockHeight,
        pools: Pools,
        budget: usize,
    ) -> Result<Self, ServeError> {
        let descending = start > end;
        let (low, high) = if descending { (end, start) } else { (start, end) };
        let held = reader.view().tip().map(|held| held.height.min(tip));
        let Some(top) = held.filter(|&top| low <= top) else {
            return Err(ServeError::NotFound { height: low });
        };
        let high = high.min(top);
        let (next, last) = if descending { (high, low) } else { (low, high) };

        Ok(Self { reader, budget, pools, descending, next: Some(next), last })
    }

    /// Next chunk reads the files (a cold window faults: the blocking pool's step)
    pub fn next_touches_disk(&self) -> bool {
        self.next.is_some_and(|next| Some(next) <= self.reader.durable())
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
            false => self.reader.durable().map_or(self.last, |tip| tip.min(self.last)),
            true => self.last,
        };
        let (records, reached) = self.reader.range(height, files_last, self.budget);
        self.step_past(reached);
        Some(project(&records, self.pools).ok_or(ServeError::Malformed { height }))
    }

    /// `next` = one past `reached` in walk order (`None` once `last` is served)
    fn step_past(&mut self, reached: BlockHeight) {
        self.next = match (reached == self.last, self.descending) {
            (true, _) => None,
            (false, false) => Some(reached + 1),
            (false, true) => u32::from(reached).checked_sub(1).map(BlockHeight::from),
        };
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::{fold, reader::WINDOW_RECORDS};
    use prost::Message;
    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Overlay, Schema, Store};
    use zaino_primitives::testing::{all_at, h, p2pkh, BlockBuilder, MockChain};
    use zaino_proto::frame::{framed_len, split_frame};
    use zaino_proto::proto::compact_formats;
    use zaino_sync::testing::open;
    use zcash_protocol::consensus::NetworkType;
    use zcash_transparent::bundle::OutPoint;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    /// Blocks 0..`count` (every upgrade from genesis)
    ///
    /// - each: coinbase paying 17 345, then one tx spending it into every pool, fee 5 000
    ///   (transparent in + out, a sapling spend + output, one orchard, two ironwood actions)
    /// - `chain(n)` = a prefix of `chain(n + k)`
    fn chain(count: u32) -> MockChain {
        let alice = p2pkh([0xaa; 20]);
        let every_pool = |at: u32| {
            let (seed, leaf) = (at.to_le_bytes(), at);
            let id = move |kind: u8| {
                let mut id = [kind; 32];
                id[..4].copy_from_slice(&seed);
                id
            };
            let alice = alice.clone();
            move |b: BlockBuilder| {
                b.coinbase(|c| c.txid(id(0xc0)).pay(&alice, 17_345)).tx(|t| {
                    t.spend(OutPoint::new(id(0xc0), 0))
                        .pay(&alice, 12_345)
                        .fee(5_000)
                        .sapling_spend(id(0x33))
                        .sapling_output(leaf)
                        .orchard_action(id(0x77), leaf)
                        .ironwood_action(id(0x88), 2 * leaf)
                        .ironwood_action(id(0x99), 2 * leaf + 1)
                })
            }
        };
        let mut chain = MockChain::regtest().upgrades(all_at(h(0))).genesis_with(every_pool(0));
        for at in 1..count {
            chain.mine(every_pool(at));
        }
        chain
    }

    /// `chain`'s blocks `heights` folded onto `store`
    fn folded(mut store: DiskStore, chain: &MockChain, heights: std::ops::Range<u32>) -> DiskStore {
        for block in &chain.blocks(chain.tip())[heights.start as usize..heights.end as usize] {
            let mut changes = store.changes(block.block_ref());
            let parent = CompactBlockReader::new(store.applied());
            let fees = chain.fees(block.header().hash);
            fold(&parent, block, &fees, &mut changes);
            store.apply(changes);
        }
        store
    }

    /// `chain` durable whole, read as an NFS publication reads it (nothing above)
    fn reader(chain: &MockChain) -> CompactBlockReader<OverlayView<DiskView>> {
        let count = u32::from(chain.tip().height) + 1;
        let store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let mut store = folded(store, chain, 0..count);
        store.commit().expect("SimFs commit");
        CompactBlockReader::new(OverlayView::new(store.durable(), Overlay::empty(store.schema())))
    }

    /// `chain(7)`: 0..=3 durable, 4..=6 in the layer above them (as the NFS folds them)
    fn four_durable_three_above(chain: &MockChain) -> CompactBlockReader<OverlayView<DiskView>> {
        let store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let mut store = folded(store, chain, 0..4);
        store.commit().expect("SimFs commit");
        let mut layer = Overlay::empty(store.schema());
        for block in &chain.blocks(chain.tip())[4..7] {
            let parent = CompactBlockReader::new(OverlayView::new(store.durable(), layer.clone()));
            let mut changes = store.schema().changes(block.block_ref());
            let fees = chain.fees(block.header().hash);
            fold(&parent, block, &fees, &mut changes);
            layer = layer.with(&changes);
        }
        CompactBlockReader::new(OverlayView::new(store.durable(), layer))
    }

    /// Every framed record a chunk carries, decoded
    fn decode(chunk: &[u8]) -> Vec<compact_formats::CompactBlock> {
        let mut blocks = Vec::new();
        let mut rest = chunk;
        while !rest.is_empty() {
            let (message, tail) = split_frame(rest).expect("whole frame");
            blocks.push(compact_formats::CompactBlock::decode(message).expect("message"));
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
        let chain = chain(4);
        let reader = reader(&chain);
        let hash = |height| chain.at(h(height)).hash.0;

        let body = reader.block(h(2)).expect("block 2");
        let [decoded] = decode(&body).try_into().expect("one record");
        assert_eq!(decoded.height, 2);

        // GetBlock unfiltered, unlike GetBlockRange (slot 1 = the every-pool tx)
        assert_eq!(decoded.vtx.len(), 2, "coinbase kept");
        assert_eq!(decoded.vtx[1].vin.len(), 1, "transparent present");
        assert_eq!(decoded.vtx[1].ironwood_actions.len(), 2, "ironwood present");
        assert_eq!(body[0], 0, "gRPC compression flag");
        assert_eq!(framed_len(&body), Some(body.len()), "frame length = the message it carries");

        let [stored] = decode(&reader.block(h(3)).expect("tip")).try_into().expect("one record");
        assert_eq!(stored.hash, hash(3), "tip record = the chain's tip");
        assert_eq!(reader.block(h(4)), None);

        assert_eq!(reader.block_at(h(2), &hash(2)), Ok(body), "by hash = by height");
        let other = reader.block_at(h(2), &hash(3));
        assert_eq!(other, Err(ServeError::HashNotFound), "another block at the located height");
        let past = reader.block_at(h(9), &[0xee; HASH]);
        assert_eq!(past, Err(ServeError::HashNotFound), "a height past the tip");
    }

    /// Range crossing the seam (where an off-by-one hides)
    ///
    /// - every height exactly once, in order, no duplicate or gap at the boundary
    /// - files in one window, the layer per record
    #[test]
    fn a_range_spans_the_durable_records_and_the_layer_without_a_seam() {
        // durable 0 to 3 (both inclusive), layer from 4
        let chain = chain(7);
        let reader = four_durable_three_above(&chain);
        let range = |start, end| {
            RangeCursor::new(reader.clone(), h(start), h(end), h(6), Pools::ALL).expect("range")
        };
        let single = |height| decode(&reader.block(h(height)).expect("block"))[0].height;
        assert_eq!((single(2), single(5)), (2, 5));

        let chunks = drain(range(2, 6));
        assert_eq!(heights(&chunks), [2, 3, 4, 5, 6], "no gap or repeat at the seam");
        let per_chunk: Vec<usize> = chunks.iter().map(|chunk| decode(chunk).len()).collect();
        assert_eq!(per_chunk, [2, 1, 1, 1], "file span whole, layer per block");

        let (above, below) = (drain(range(5, 6)), drain(range(0, 1)));
        assert_eq!((heights(&above), heights(&below)), (vec![5, 6], vec![0, 1]));

        assert_eq!(reader.resident_block(h(5)), reader.block(h(5)), "layer: RAM");
        assert_eq!(reader.resident_block(h(2)), None, "files: not resident");
        let layer_hash = chain.at(h(5)).hash.0;
        assert!(reader.block_at(h(5), &layer_hash).is_ok(), "layer hash");
    }

    /// `start > end` = lightwalletd's descending range: the ascending range's records, top down,
    /// across the seam (layer first)
    #[test]
    fn a_descending_range_serves_the_ascending_records_top_down_across_the_seam() {
        let reader = four_durable_three_above(&chain(7));
        let range = |start, end| {
            RangeCursor::new(reader.clone(), h(start), h(end), h(6), Pools::ALL).expect("range")
        };

        let ascending = drain(range(1, 6));
        let descending = drain(range(6, 1));
        let mut reversed = decode(&ascending.concat());
        reversed.reverse();
        assert_eq!(decode(&descending.concat()), reversed, "6..=1 = 1..=6 reversed");
        let per_chunk: Vec<usize> = descending.iter().map(|chunk| decode(chunk).len()).collect();
        assert_eq!(per_chunk, [1, 1, 1, 3], "layer per block, then one file window");

        // budget under one record: one record per file window, still top down
        let narrow = RangeCursor::with_budget(reader.clone(), h(6), h(1), h(6), Pools::ALL, 1);
        assert_eq!(heights(&drain(narrow.expect("narrow"))), [6, 5, 4, 3, 2, 1]);
    }

    /// Bounds over 0..=6 held (0..=3 durable)
    ///
    /// - top past the served tip = clamped, either direction
    /// - bottom past it = a miss (served tip 4 = a root publication behind its reader)
    #[test]
    fn a_range_clamps_its_top_to_the_served_tip_and_misses_past_it() {
        let reader = four_durable_three_above(&chain(7));
        let served = |heights: &[u64]| Ok(heights.to_vec());
        let missed = |height| Err(ServeError::NotFound { height: h(height) });
        // (start, end, served tip) → heights in walk order
        #[rustfmt::skip]
        let cases = [
            ((1, 4, 6),  served(&[1, 2, 3, 4])),
            ((4, 99, 6), served(&[4, 5, 6])),
            ((99, 4, 6), served(&[6, 5, 4])),
            ((2, 99, 4), served(&[2, 3, 4])),
            ((99, 3, 4), served(&[4, 3])),
            ((5, 6, 4),  missed(5)),
            ((99, 7, 6), missed(7)),
            ((7, 9, 6),  missed(7)),
        ];
        for ((start, end, tip), expected) in cases {
            let range = RangeCursor::new(reader.clone(), h(start), h(end), h(tip), Pools::ALL);
            let served = range.map(|cursor| heights(&drain(cursor)));
            assert_eq!(served, expected, "{start} to {end}, snapshot tip {tip}");
        }
    }

    /// Projected range, either direction and across the seam = its records projected in walk
    /// order, chunked as the unprojected range
    #[test]
    fn a_projected_range_is_its_records_projected_in_walk_order() {
        let reader = four_durable_three_above(&chain(7));
        for (start, end) in [(2, 6), (6, 1), (0, 3)] {
            let range = |pools| {
                let range = RangeCursor::new(reader.clone(), h(start), h(end), h(6), pools);
                drain(range.expect("range"))
            };
            let walk: Vec<u32> = match start <= end {
                true => (start..=end).collect(),
                false => (end..=start).rev().collect(),
            };
            let records: Vec<Bytes> =
                walk.into_iter().map(|height| reader.block(h(height)).expect("held")).collect();
            let projected = project(&records, Pools::default()).expect("walks");
            let (shielded, all) = (range(Pools::default()), range(Pools::ALL));
            assert_eq!(shielded.concat(), projected, "{start} to {end}");
            assert_eq!(shielded.len(), all.len(), "{start} to {end}: same chunks");
        }
    }

    #[test]
    fn a_range_is_walked_in_bounded_windows() {
        let reader = reader(&chain(8));

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
        let long = self::reader(&chain(tip + 1));
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
}
