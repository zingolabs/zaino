//! compact_block writer: the final stream → one [`fold`] per block → its store
//!
//! - fees: one [`BlockFees`] off value-balance's sink per step, held heights included
//!   (value-balance re-folds them: both streams stay in step with either index ahead)

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use zaino_persistence::{BlockChanges, IndexKind, SequenceRead, Store, View};
use zaino_primitives::types::{Block, BlockFees, TreeSizeOutOfRange};
use zaino_sync::{
    apply, assert_next, blocking, commit, held, ran, IndexHandle, IndexPublisher, Step,
    Subscription,
};

use crate::{encode_compact_block, position, CompactBlockReader, BLOCKS};

const NAME: &str = IndexKind::CompactBlock.name();

pub struct CompactBlockIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: SequenceRead>> CompactBlockIndexWriter<S> {
    /// Over `store` (opened with [`TABLES`](crate::TABLES))
    pub fn new(store: S) -> Self {
        let view = store.committed();
        let held = view.tip().map_or(0, |tip| position(tip.height) + 1);
        assert_eq!(view.sequence(BLOCKS).count(), held, "{NAME}: one record per committed height");
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    /// For `Nfs::add`: committed view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` and `fees` (value-balance's) through their `Shutdown` (a failure panics)
    ///
    /// - commits after the final tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>, mut fees: Subscription<BlockFees>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            let started = Instant::now();
            let mut paid = Vec::with_capacity(run.blocks.len());
            for (_, block) in &run.blocks {
                paid.push(next_fees(&mut fees, block).await);
            }
            let write;
            (store, write) = blocking(move || {
                let mut write = Duration::ZERO;
                for ((height, block), fees) in run.blocks.iter().zip(&paid) {
                    if !held(&store, *height) {
                        let mut changes = store.changes(block.at());
                        let parent = CompactBlockReader::new(store.staged());
                        let folded = fold(&parent, block, fees, &mut changes);
                        folded.unwrap_or_else(|error| panic!("{NAME} index at {height}: {error}"));
                        write += apply(&mut store, changes);
                    }
                }
                if run.finalized {
                    write += commit(&mut store);
                }
                (store, write)
            })
            .await;
            publisher.publish(&store);
            ran(&store, started, write);
        }
        store = blocking(move || {
            commit(&mut store);
            store
        })
        .await;
        publisher.publish(&store);
        let ended = matches!(fees.next().await, Step::Shutdown);
        assert!(ended, "{NAME}: fees past the last block");
    }
}

/// Value-balance's fees for `block`
async fn next_fees(fees: &mut Subscription<BlockFees>, block: &Block) -> Arc<BlockFees> {
    let Step::Apply { height, data } = fees.next().await else {
        panic!("{NAME}: fees ended before the blocks");
    };
    assert!(data.belongs_to(block), "{NAME}: fees at {height} for another block");
    data
}

/// `block` + its `fees` onto `parent`: its one record
///
/// - sizes after it = parent tip record's `chainMetadata` + what it commits ([`Block`] carries
///   none; `z_gettreestate` = one round trip per block, unaffordable in a full sync)
/// - `Err` = a tree past `u32` (#549)
pub fn fold<V: SequenceRead>(
    parent: &CompactBlockReader<V>,
    block: &Block,
    fees: &BlockFees,
    out: &mut BlockChanges,
) -> Result<(), TreeSizeOutOfRange> {
    assert_next(out, parent.tip(), block);
    let sizes = parent.tip_sizes().advance(block)?;
    out.sequence(BLOCKS).append(&encode_compact_block(block, fees, &sizes));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, panic::AssertUnwindSafe, path::Path};

    use proptest::strategy::Strategy as _;
    use prost::Message as _;
    use tokio::task::JoinHandle;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, Overlay, PersistenceEngine, Schema,
    };
    use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain};
    use zaino_primitives::types::{Height, TreeSize, TreeSizes};
    use zaino_proto::frame::FRAME_HEADER;
    use zaino_proto::proto::compact_formats as cf;
    use zaino_sync::{FeeSink, IndexerDataSink};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{FORMAT, TABLES};

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const SCHEMA: Schema =
        Schema::new(IndexKind::CompactBlock, FORMAT, NetworkType::Regtest, TABLES);

    /// `write_buffer` = MIN: every applied block committed by the store itself
    fn open(fs: &Arc<SimFs>, write_buffer: NonZeroUsize) -> DiskStore {
        DiskEngine::new(fs.clone(), zaino_persistence::LsmConfig::default())
            .open(Path::new("/cb"), &SCHEMA, write_buffer)
            .expect("open")
    }

    /// Genesis ..= tip, each block beside the fees value-balance derives for it
    fn with_fees(chain: &MockChain) -> Vec<(Arc<Block>, BlockFees)> {
        let blocks = chain.blocks(chain.tip()).into_iter();
        blocks.map(|block| (Arc::clone(&block), chain.fees(block.header().hash))).collect()
    }

    /// Genesis + three blocks folded one onto the next: each record = the block encoded with the
    /// running sizes; a parent record claiming near-`u32::MAX` seeds the next fold (sizes read,
    /// not carried), so one block more overflows; a gap or a fork panics
    #[test]
    fn sizes_advance_from_the_parent_record_and_a_non_parent_panics() {
        let sizes = |sapling: u32, orchard: u32, ironwood: u32| TreeSizes {
            sapling: TreeSize::from(sapling),
            orchard: TreeSize::from(orchard),
            ironwood: TreeSize::from(ironwood),
        };
        // coinbases commit (sapling, orchard, ironwood) = (2, 1, 0), (3, 0, 4), (0, 5, 1)
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| {
            b.coinbase(|c| c.sapling_output(1).sapling_output(2).orchard_action([1; 32], 1))
        });
        let two = chain.mine(|b| {
            b.coinbase(|c| {
                let c = c.sapling_output(3).sapling_output(4).sapling_output(5);
                let c = c.ironwood_action([1; 32], 1).ironwood_action([2; 32], 2);
                c.ironwood_action([3; 32], 3).ironwood_action([4; 32], 4)
            })
        });
        let three = chain.mine(|b| {
            b.coinbase(|c| {
                let c = c.orchard_action([2; 32], 2).orchard_action([3; 32], 3);
                let c = c.orchard_action([4; 32], 4).orchard_action([5; 32], 5);
                c.orchard_action([6; 32], 6).ironwood_action([5; 32], 5)
            })
        });
        // height 3 on a sibling of `two`
        let cousin = chain.fork(h(1)).mine_empty(2).tip();
        let fees = |block: &Block| chain.fees(block.header().hash);
        let folded = |store: &DiskStore, block: &Block| {
            let mut changes = store.changes(block.at());
            let parent = CompactBlockReader::new(store.staged());
            fold(&parent, block, &fees(block), &mut changes).map(|()| changes)
        };
        let mut through_three = open(&SimFs::new(), NonZeroUsize::MAX);
        for (at, after) in [
            (chain.genesis(), sizes(0, 0, 0)),
            (one, sizes(2, 1, 0)),
            (two, sizes(5, 1, 4)),
            (three, sizes(5, 6, 5)),
        ] {
            let block = chain.block(at.hash);
            let changes = folded(&through_three, block).expect("far below u32");
            let records: Vec<&[u8]> = changes.appends(BLOCKS).collect();
            let expected = encode_compact_block(block, &fees(block), &after);
            assert_eq!(records, [&expected[..]], "{at:?}: one record, running sizes");
            through_three.apply(changes);
            assert_eq!(CompactBlockReader::new(through_three.staged()).tip_sizes(), after);
        }

        // `one`'s record written claiming sapling = u32::MAX - 1: `two`'s 3 outputs overflow
        let mut seeded = open(&SimFs::new(), NonZeroUsize::MAX);
        seeded.apply(folded(&seeded, chain.block(chain.genesis().hash)).expect("bare"));
        let mut changes = Overlay::empty(&SCHEMA).changes(one);
        let near_full = sizes(u32::MAX - 1, 0, 0);
        let block = chain.block(one.hash);
        changes.sequence(BLOCKS).append(&encode_compact_block(block, &fees(block), &near_full));
        seeded.apply(changes);
        let overflow = folded(&seeded, chain.block(two.hash)).err();
        assert_eq!(overflow, Some(TreeSizeOutOfRange { got: u64::from(u32::MAX) + 2 }));

        // parents: `through_three` = 0..=3, `seeded` = 0..=1, `through_two` = 0..=2
        let mut through_two = open(&SimFs::new(), NonZeroUsize::MAX);
        for at in [chain.genesis(), one, two] {
            through_two.apply(folded(&through_two, chain.block(at.hash)).expect("small"));
        }
        for (case, parent, at) in [
            ("gap", &seeded, three),
            ("fork at the same height", &through_two, cousin),
            ("below the tip", &through_three, two),
        ] {
            let block = chain.block(at.hash);
            let refused = std::panic::catch_unwind(AssertUnwindSafe(|| folded(parent, block)));
            let payload = refused.expect_err(case);
            let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
            let named = message.starts_with("compact_block: ");
            assert!(
                named && message.contains("does not extend the parent tip"),
                "{case}: {message}"
            );
        }
    }

    /// Writer as zainod runs it: the final stream, value-balance's fee stream (one per step), its
    /// handle
    struct Running {
        blocks: IndexerDataSink<Block>,
        fees: FeeSink,
        handle: IndexHandle<DiskView>,
        run: JoinHandle<()>,
    }

    impl Running {
        fn start(store: DiskStore) -> Self {
            let writer = CompactBlockIndexWriter::new(store);
            let handle = writer.handle();
            let (mut blocks, mut fees) = (IndexerDataSink::new(), FeeSink::new());
            let (block_sub, fee_sub) = (blocks.subscribe(NAME, QUEUE), fees.subscribe(NAME, QUEUE));
            let run = tokio::spawn(writer.run(block_sub, fee_sub));
            Self { blocks, fees, handle, run }
        }

        /// `block` + its `fees` (as value-balance sends them)
        async fn send(&self, (block, fees): &(Arc<Block>, BlockFees)) {
            let height = block.header().height;
            self.fees.send(Step::Apply { height, data: Arc::new(fees.clone()) }).await;
            self.blocks.send(Step::Apply { height, data: Arc::clone(block) }).await;
        }

        /// [`send`](Self::send) as the chain's final tip: the writer commits after it
        async fn finalize(&self, (block, fees): &(Arc<Block>, BlockFees)) {
            let height = block.header().height;
            self.fees.send(Step::Apply { height, data: Arc::new(fees.clone()) }).await;
            self.blocks.send(Step::Finalized { height, data: Arc::clone(block) }).await;
        }

        async fn reached(&mut self, tip: Option<u32>) {
            while self.handle.tip().map(|tip| u32::from(tip.height)) != tip {
                assert!(self.handle.changed().await, "writer alive");
            }
        }

        async fn stop(self) -> DiskView {
            self.fees.shutdown();
            self.blocks.shutdown();
            self.run.await.expect("stops at Shutdown");
            self.handle.view()
        }
    }

    /// `(sapling, orchard, ironwood)` sizes and per-tx fees of the record at `height`
    fn record(view: &DiskView, height: u32) -> ((u32, u32, u32), Vec<u32>) {
        let reader = CompactBlockReader::new(view.clone());
        let record = reader.block(h(height)).expect("record");
        let decoded = cf::CompactBlock::decode(&record[FRAME_HEADER..]).expect("decodes");
        let meta = decoded.chain_metadata.expect("carries metadata");
        let sizes = (
            meta.sapling_commitment_tree_size,
            meta.orchard_commitment_tree_size,
            meta.ironwood_commitment_tree_size,
        );
        (sizes, decoded.vtx.iter().map(|tx| tx.fee).collect())
    }

    /// Tree sizes accumulate block by block, and a restart resending held heights (their fees
    /// popped, their records untouched) folds the next block onto the committed tip record (not
    /// zero)
    #[tokio::test]
    async fn tree_sizes_and_fees_accumulate_across_blocks_and_a_restart() {
        let fs = SimFs::new();
        let miner = p2pkh([0xc0; 20]);
        let mut mock = MockChain::regtest();
        // block h: coinbase pays 1 000 × h, spent whole by one tx (fee distinct per height: a
        // pairing slip = a wrong fee in the record) committing (sapling, orchard, ironwood)
        for (at, (sapling, orchard, ironwood)) in
            (1u8..).zip([(2, 1, 0), (3, 2, 1), (0, 0, 4), (1, 1, 1)])
        {
            let fee = 1_000 * u64::from(at);
            mock.mine(|b| {
                b.coinbase(|c| c.txid([0xc0 + at; 32]).pay(&miner, fee)).tx(|t| {
                    let t = t.spend(outpoint([0xc0 + at; 32], 0)).fee(fee);
                    let t = (0..sapling).fold(t, |t, i| t.sapling_output(u32::from(16 * at + i)));
                    let t = (0..orchard).fold(t, |t, i| t.orchard_action([16 * at + i; 32], 1));
                    (0..ironwood).fold(t, |t, i| t.ironwood_action([16 * at + i; 32], 1))
                })
            });
        }
        let chain = with_fees(&mock);

        let mut index = Running::start(open(&fs, NonZeroUsize::MAX));
        for block in &chain[..3] {
            index.send(block).await;
        }
        index.finalize(&chain[3]).await;
        index.reached(Some(3)).await;
        let view = index.stop().await;
        let stored = [0, 1, 2, 3].map(|height| record(&view, height));
        let fees = |height: u32| vec![0, 1_000 * height];
        let expected = [
            ((0, 0, 0), vec![0]),
            ((2, 1, 0), fees(1)),
            ((5, 3, 1), fees(2)),
            ((5, 3, 5), fees(3)),
        ];
        assert_eq!(stored, expected, "cumulative sizes, each block's own fees");

        let mut resumed = Running::start(open(&fs, NonZeroUsize::MAX));
        for block in &chain[2..4] {
            resumed.send(block).await;
        }
        resumed.finalize(&chain[4]).await;
        resumed.reached(Some(4)).await;
        let view = resumed.stop().await;
        assert_eq!(record(&view, 3), expected[3], "held: untouched");
        assert_eq!(record(&view, 4), ((6, 4, 6), fees(4)), "folded onto 3's record");
    }

    /// - `Send(n)`: next `n` blocks
    /// - `Reopen`: shutdown, reopen, resend from one below the tip (held: fees popped, skipped)
    #[derive(Debug, Clone)]
    enum Move {
        Send(usize),
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random blocks through random final streams (runs, restarts): once every move
        /// commits, each height serves a record whose hash and cumulative tree sizes equal an
        /// independently summed model and whose fees are its own block's, and a full range read
        /// is those records in order
        #[test]
        fn random_histories_serve_records_with_the_models_tree_sizes_and_fees(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    4 => (1usize..=3).prop_map(Move::Send),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
            write_buffer in proptest::prop_oneof![
                proptest::strategy::Just(NonZeroUsize::MIN),
                proptest::strategy::Just(NonZeroUsize::MAX),
            ],
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves, write_buffer));
        }
    }

    async fn random_history(
        counts: Vec<(usize, usize, usize)>,
        moves: Vec<Move>,
        write_buffer: NonZeroUsize,
    ) {
        // genesis, then block h: coinbase pays 1 000 × h, spent whole by one tx committing
        // `counts[h - 1]` (sapling, orchard, ironwood)
        let miner = p2pkh([0xc0; 20]);
        let mut mock = MockChain::regtest();
        for (at, &(sapling, orchard, ironwood)) in (1u8..).zip(&counts) {
            let fee = 1_000 * u64::from(at);
            let [sapling, orchard, ironwood] =
                [sapling, orchard, ironwood].map(|count| u8::try_from(count).expect("≤ 3"));
            mock.mine(|b| {
                b.coinbase(|c| c.txid([0xc0 + at; 32]).pay(&miner, fee)).tx(|t| {
                    let t = t.spend(outpoint([0xc0 + at; 32], 0)).fee(fee);
                    let t = (0..sapling).fold(t, |t, i| t.sapling_output(u32::from(16 * at + i)));
                    let t = (0..orchard).fold(t, |t, i| t.orchard_action([16 * at + i; 32], 1));
                    (0..ironwood).fold(t, |t, i| t.ironwood_action([16 * at + i; 32], 1))
                })
            });
        }
        let chain = with_fees(&mock);
        let sizes_at = |height: usize| {
            counts[..height].iter().fold((0u32, 0u32, 0u32), |(s, o, i), &(ds, d_o, di)| {
                (s + ds as u32, o + d_o as u32, i + di as u32)
            })
        };

        let fs = SimFs::new();
        let mut index = Running::start(open(&fs, write_buffer));
        let mut sent = 0usize;
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Send(count) => {
                    let burst: Vec<_> = chain.iter().skip(sent).take(count).collect();
                    for (at, block) in burst.iter().enumerate() {
                        match at + 1 == burst.len() {
                            true => index.finalize(block).await,
                            false => index.send(block).await,
                        }
                        sent += 1;
                    }
                }
                Move::Reopen => {
                    index.stop().await;
                    index = Running::start(open(&fs, write_buffer));
                    if let Some(held) = sent.checked_sub(1) {
                        index.send(&chain[held]).await;
                    }
                }
            }
            index.reached(sent.checked_sub(1).map(|last| last as u32)).await;

            let case = format!("move {at} {next:?}");
            let view = index.handle.view();
            let reader = CompactBlockReader::new(view.clone());
            let mut records = Vec::new();
            for height in 0..sent as u32 {
                let (sizes, fees) = record(&view, height);
                let stored = reader.block(h(height)).expect("record");
                let decoded = cf::CompactBlock::decode(&stored[FRAME_HEADER..]).expect("decodes");
                let hash = <[u8; 32]>::from(chain[height as usize].0.header().hash);
                assert_eq!(decoded.hash, hash.to_vec(), "{case}: record {height}");
                assert_eq!(sizes, sizes_at(height as usize), "{case}: record {height}");
                let own = match height {
                    0 => vec![0],
                    paid => vec![0, 1_000 * paid],
                };
                assert_eq!(fees, own, "{case}: its own fees");
                records.extend_from_slice(&stored);
            }
            if let Some(last) = sent.checked_sub(1) {
                let (window, reach) = reader.range(Height::GENESIS, h(last as u32), usize::MAX);
                assert_eq!(reach, h(last as u32), "{case}");
                assert_eq!(window.concat(), records[..window.concat().len()], "{case}: in order");
            }
        }
        index.stop().await;
    }

    /// Genesis + four bulk blocks, each its own commit, crashed after every operation: each state
    /// reopens to an acknowledged or the attempted commit, serves those records byte for byte,
    /// and the next block's tree sizes fold onto its tip record (not from zero)
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_committed_prefix_the_next_block_folds_onto() {
        // (sapling, orchard, ironwood) per block from 1: cumulative sizes distinct at every height
        let counts = [(1u8, 0u8, 2u8), (2, 1, 0), (0, 3, 1), (4, 1, 1), (1, 2, 3)];
        let mut mock = MockChain::regtest();
        for (at, (sapling, orchard, ironwood)) in (1u8..).zip(counts) {
            mock.mine(|b| {
                b.tx(|t| {
                    let t = (0..sapling).fold(t, |t, i| t.sapling_output(u32::from(16 * at + i)));
                    let t = (0..orchard).fold(t, |t, i| t.orchard_action([16 * at + i; 32], 1));
                    (0..ironwood).fold(t, |t, i| t.ironwood_action([16 * at + i; 32], 1))
                })
            });
        }
        let chain = with_fees(&mock);
        let sizes_at = |height: usize| {
            counts[..height].iter().fold((0, 0, 0), |(s, o, i), &(ds, d_o, di)| {
                (s + u32::from(ds), o + u32::from(d_o), i + u32::from(di))
            })
        };
        let fs = SimFs::recording();
        let mut index = Running::start(open(&fs, NonZeroUsize::MIN));
        for (acked, block) in (1u64..).zip(&chain[..5]) {
            index.send(block).await;
            index.reached(Some(u32::from(block.0.header().height))).await;
            fs.set_tag(acked);
        }
        let reader = CompactBlockReader::new(index.stop().await);
        let records: Vec<_> = (0..5).map(|n| reader.block(h(n))).collect();

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs, NonZeroUsize::MIN);
            let count = store.committed().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5) as usize);
            assert!(acked.contains(&count), "{label}: recovered {count} blocks");
            let reader = CompactBlockReader::new(store.committed());
            let served: Vec<_> = (0..count as u32).map(|n| reader.block(h(n))).collect();
            assert_eq!(served, records[..count], "{label}: byte-identical records");

            let mut index = Running::start(store);
            index.send(&chain[count]).await;
            index.reached(Some(count as u32)).await;
            let (sizes, _) = record(&index.stop().await, count as u32);
            assert_eq!(sizes, sizes_at(count), "{label}: folded onto the tip record");
        }
    }
}
