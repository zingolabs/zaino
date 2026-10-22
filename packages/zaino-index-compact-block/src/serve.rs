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
    use zaino_persistence::{fs::SimFs, DiskView, Overlay, Schema, Store};
    use zaino_proto::frame::{framed_len, split_frame};
    use zaino_proto::proto::compact_formats;
    use zaino_source::testing::{all_at, h, p2pkh, MockChain};
    use zaino_sync::testing::open;
    use zcash_protocol::consensus::NetworkType;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    /// `chain` as an NFS publication reads it: heights `0..durable` committed, the rest folded
    /// into the layer above them
    fn publication(chain: &MockChain, durable: u32) -> CompactBlockReader<OverlayView<DiskView>> {
        let (blocks, durable) = (chain.blocks(chain.tip()), durable as usize);
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        for block in &blocks[..durable] {
            let mut changes = store.changes(block.block_ref());
            let parent = CompactBlockReader::new(store.applied());
            fold(&parent, block, &chain.fees(block.header().hash), &mut changes);
            store.apply(changes);
        }
        store.commit().expect("SimFs commit");
        let mut layer = Overlay::empty(store.schema());
        for block in &blocks[durable..] {
            let mut changes = store.schema().changes(block.block_ref());
            let parent = CompactBlockReader::new(OverlayView::new(store.durable(), layer.clone()));
            fold(&parent, block, &chain.fees(block.header().hash), &mut changes);
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

    fn per_chunk(chunks: &[Bytes]) -> Vec<usize> {
        chunks.iter().map(|chunk| decode(chunk).len()).collect()
    }

    #[test]
    fn single_block_reads_carry_every_pool_and_confirm_the_located_hash() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().upgrades(all_at(h(0)));
        let one = chain.mine(|b| b.coinbase(|c| c.pay(&alice, 17_345)));
        let paid = chain.output(one, 0, 0);
        let two = chain.mine(|b| {
            b.tx(|t| {
                let t = t.spend(paid).pay(&alice, 12_345).fee(5_000);
                let t = t.sapling_spend(1).sapling_output(1).orchard_action(1, 1);
                t.ironwood_action(1, 1).ironwood_action(2, 2)
            })
        });
        let three = chain.mine_empty(1);
        let reader = publication(&chain, 4);

        let body = reader.block(h(2)).expect("block 2");
        let [decoded] = decode(&body).try_into().expect("one record");
        assert_eq!(decoded.height, 2, "record at height 2");
        // GetBlock unfiltered, unlike GetBlockRange (tx 1 = the every-pool tx)
        assert_eq!(decoded.vtx.len(), 2, "coinbase kept");
        assert_eq!(decoded.vtx[1].vin.len(), 1, "transparent present");
        assert_eq!(decoded.vtx[1].ironwood_actions.len(), 2, "ironwood present");
        assert_eq!(body[0], 0, "gRPC compression flag");
        assert_eq!(framed_len(&body), Some(body.len()), "frame length = the message it carries");
        let [tip] = decode(&reader.block(h(3)).expect("tip")).try_into().expect("one record");
        assert_eq!(tip.hash, three.hash.0, "tip record = the chain's tip");
        assert_eq!(reader.block(h(4)), None, "past the tip");

        assert_eq!(reader.block_at(h(2), &two.hash.0), Ok(body), "by hash = by height");
        let other = reader.block_at(h(2), &three.hash.0);
        assert_eq!(other, Err(ServeError::HashNotFound), "another block at the located height");
        let past = reader.block_at(h(9), &[0xee; HASH]);
        assert_eq!(past, Err(ServeError::HashNotFound), "a height past the tip");
    }

    #[test]
    fn only_layer_blocks_are_resident_and_a_layer_block_resolves_by_hash() {
        let mut chain = MockChain::regtest();
        let six = chain.mine_empty(6);
        let reader = publication(&chain, 4);

        assert_eq!(reader.resident_block(h(5)), reader.block(h(5)), "layer: RAM");
        assert_eq!(reader.resident_block(h(2)), None, "files: not resident");
        let by_hash = reader.block_at(h(6), &six.hash.0);
        assert_eq!(by_hash, Ok(reader.block(h(6)).expect("held")), "layer block by hash");
    }

    #[derive(Debug)]
    struct Range {
        start: u32,
        end: u32,
        tip: u32,
        budget: usize,
    }

    #[derive(Debug, PartialEq)]
    struct Served {
        heights: Vec<u64>,
        per_chunk: Vec<usize>,
    }

    /// Every height in range once, in walk order (`start > end` = descending, lightwalletd's), top
    /// clamped to the served tip, chunked = one file window per `budget` + one chunk per layer
    /// block (the seam = where an off-by-one hides)
    #[test]
    fn a_range_serves_each_height_once_in_walk_order_in_windows_and_layer_blocks() {
        const ONE_WINDOW: usize = WINDOW_BYTES;
        const PER_RECORD: usize = 1;
        let mut chain = MockChain::regtest();
        chain.mine_empty(6);
        let reader = publication(&chain, 4); // 0..=3 files, 4..=6 layer
        #[rustfmt::skip]
        let cases = [
            (Range { start: 2,  end: 6,  tip: 6, budget: ONE_WINDOW }, &[2, 3, 4, 5, 6][..], &[2, 1, 1, 1][..]),
            (Range { start: 5,  end: 6,  tip: 6, budget: ONE_WINDOW }, &[5, 6],              &[1, 1]),
            (Range { start: 0,  end: 3,  tip: 6, budget: ONE_WINDOW }, &[0, 1, 2, 3],        &[4]),
            (Range { start: 0,  end: 3,  tip: 6, budget: PER_RECORD }, &[0, 1, 2, 3],        &[1, 1, 1, 1]),
            (Range { start: 6,  end: 1,  tip: 6, budget: ONE_WINDOW }, &[6, 5, 4, 3, 2, 1],  &[1, 1, 1, 3]),
            (Range { start: 6,  end: 1,  tip: 6, budget: PER_RECORD }, &[6, 5, 4, 3, 2, 1],  &[1, 1, 1, 1, 1, 1]),
            (Range { start: 1,  end: 4,  tip: 6, budget: ONE_WINDOW }, &[1, 2, 3, 4],        &[3, 1]),
            (Range { start: 4,  end: 99, tip: 6, budget: ONE_WINDOW }, &[4, 5, 6],           &[1, 1, 1]),
            (Range { start: 99, end: 4,  tip: 6, budget: ONE_WINDOW }, &[6, 5, 4],           &[1, 1, 1]),
            // served tip 4 = a root publication behind its reader
            (Range { start: 2,  end: 99, tip: 4, budget: ONE_WINDOW }, &[2, 3, 4],           &[2, 1]),
            (Range { start: 99, end: 3,  tip: 4, budget: ONE_WINDOW }, &[4, 3],              &[1, 1]),
        ];
        for (range, heights_served, chunks_of) in cases {
            let Range { start, end, tip, budget } = range;
            let cursor = RangeCursor::with_budget(
                reader.clone(),
                h(start),
                h(end),
                h(tip),
                Pools::ALL,
                budget,
            );
            let chunks = drain(cursor.unwrap_or_else(|error| panic!("{range:?}: {error}")));

            let expected =
                Served { heights: heights_served.to_vec(), per_chunk: chunks_of.to_vec() };
            let served = Served { heights: heights(&chunks), per_chunk: per_chunk(&chunks) };
            assert_eq!(served, expected, "{range:?}");
            let records: Vec<Bytes> = heights_served
                .iter()
                .map(|&at| reader.block(h(at as u32)).expect("held"))
                .collect();
            let expected = project(&records, Pools::ALL).expect("walks");
            assert!(chunks.concat() == expected, "{range:?}: chunks = those heights' records");
        }
    }

    #[test]
    fn a_range_starting_past_the_served_tip_misses_its_bottom() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(6);
        let reader = publication(&chain, 4);
        // (start, end, served tip) → missed height = the range's bottom
        #[rustfmt::skip]
        let cases = [
            ((5, 6, 4),  5),
            ((99, 7, 6), 7),
            ((7, 9, 6),  7),
        ];
        for ((start, end, tip), bottom) in cases {
            let range = RangeCursor::new(reader.clone(), h(start), h(end), h(tip), Pools::ALL);
            let missed = range.map(|_| "a cursor");
            let expected = Err(ServeError::NotFound { height: h(bottom) });
            assert_eq!(missed, expected, "{start} to {end}, served tip {tip}");
        }
    }

    /// Projected range, either direction and across the seam = its records projected in walk
    /// order, chunked as the unprojected range
    #[test]
    fn a_projected_range_is_its_records_projected_in_walk_order() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest().upgrades(all_at(h(0)));
        for at in 1..7 {
            chain.mine(|b| {
                b.coinbase(|c| c.pay(&alice, 17_345)).tx(|t| {
                    let t = t.spend_in_block(0, 0).pay(&alice, 12_345).fee(5_000);
                    t.sapling_output(at).orchard_action(at, at).ironwood_action(at, at)
                })
            });
        }
        let reader = publication(&chain, 4);

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
            assert!(shielded.concat() == projected, "{start} to {end}: projected records");
            assert_eq!(shielded.len(), all.len(), "{start} to {end}: same chunks");
        }
    }

    #[test]
    fn a_file_window_holds_at_most_window_records_either_direction() {
        let tip = WINDOW_RECORDS + 1;
        let mut chain = MockChain::regtest();
        chain.mine_empty(tip);
        let reader = publication(&chain, tip + 1);
        let range = |start, end| {
            drain(
                RangeCursor::new(reader.clone(), h(start), h(end), h(tip), Pools::ALL)
                    .expect("range"),
            )
        };

        let (up, down) = (range(0, tip), range(tip, 0));
        let capped = vec![WINDOW_RECORDS as usize, 2];
        assert_eq!(per_chunk(&up), capped, "0 to {tip}: records per window");
        assert_eq!(per_chunk(&down), capped, "{tip} to 0: records per window");
        let top_down: Vec<u64> = (0..=u64::from(tip)).rev().collect();
        assert_eq!(heights(&down), top_down, "no gap or repeat at the window edge");
    }
}
