//! compact_block writer: the finalized stream → one [`fold`] per block → its store
//!
//! - fees: one [`BlockFees`] off utxo-set's sink per step, held heights included
//!   (utxo-set republishes them from its `fees` record: streams in step with either index ahead)

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use zaino_persistence::{BlockChanges, SequenceRead, Store};
use zaino_primitives::types::{Block, BlockFees};
use zaino_sync::IndexKind;
use zaino_sync::{
    apply, assert_next, blocking, commit, held, ran, IndexHandle, IndexPublisher, Step,
    Subscription,
};

use crate::{encode_compact_block, CompactBlockReader, BLOCKS};

const NAME: &str = IndexKind::CompactBlock.name();

pub struct CompactBlockIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: SequenceRead>> CompactBlockIndexWriter<S> {
    pub fn new(store: S) -> Self {
        zaino_sync::assert_dense(&store.durable(), BLOCKS);
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` and `fees` (utxo-set's) through their `Shutdown` (a failure panics)
    ///
    /// - commits after the finalized tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>, mut fees: Subscription<BlockFees>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            let started = Instant::now();
            let mut paid = Vec::with_capacity(run.blocks.len());
            for block in &run.blocks {
                paid.push(next_fees(&mut fees, block).await);
            }
            let write;
            (store, write) = blocking(move || {
                let mut write = Duration::ZERO;
                for (block, fees) in run.blocks.iter().zip(&paid) {
                    if !held(&store, block) {
                        let mut changes = store.changes(block.block_ref());
                        let parent = CompactBlockReader::new(store.applied());
                        fold(&parent, block, fees, &mut changes);
                        write += apply(&mut store, changes);
                    }
                }
                if run.ends_at_finalized_tip {
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

/// Utxo-set's fees for `block`
async fn next_fees(fees: &mut Subscription<BlockFees>, block: &Block) -> Arc<BlockFees> {
    let Step::Apply(fees) = fees.next().await else {
        panic!("{NAME}: fees ended before the blocks");
    };
    assert!(fees.belongs_to(block), "{NAME}: fees at {} for another block", fees.height);
    fees
}

/// `block` + its `fees` onto `parent`: its one record
///
/// - sizes after it = parent tip record's `chainMetadata` + what it commits ([`Block`] carries
///   none, `z_gettreestate` = one round trip per block = unaffordable in a full sync)
pub fn fold<V: SequenceRead>(
    parent: &CompactBlockReader<V>,
    block: &Block,
    fees: &BlockFees,
    out: &mut BlockChanges,
) {
    assert_next(out, parent.view().tip(), block);
    let sizes = parent.tip_sizes().advance(block);
    out.sequence(BLOCKS).append(&encode_compact_block(block, fees, &sizes));
}
#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use proptest::prelude::{prop_assert, prop_assert_eq, Strategy as _, TestCaseError};
    use prost::Message as _;
    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema, View};
    use zaino_primitives::types::{PerPool, TreeSize, TreeSizes};
    use zaino_proto::frame::FRAME_HEADER;
    use zaino_proto::proto::compact_formats;
    use zaino_source::testing::{h, p2pkh, MockChain};
    use zaino_sync::testing::{
        block_on_paused, moves, open, step, write_buffers, FinalizedStreamFixture, Move, Writer,
        QUEUE,
    };
    use zaino_sync::FeeSink;
    use zcash_protocol::{
        consensus::{NetworkType, H0},
        BlockHash,
    };

    use super::*;
    use crate::project::{record_hash, record_sizes};

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    fn sizes(sapling: u32, orchard: u32, ironwood: u32) -> TreeSizes {
        PerPool { sapling, orchard, ironwood }.map(TreeSize::from)
    }

    /// Genesis ..= tip + each block's fees as utxo-set derives them
    fn blocks_and_fees(chain: &MockChain) -> (Vec<Arc<Block>>, Vec<BlockFees>) {
        let blocks = chain.blocks(chain.tip());
        let fees = blocks.iter().map(|block| chain.fees(block.header().hash)).collect();
        (blocks, fees)
    }

    /// Genesis + three blocks folded one onto the next: each record = the block encoded with the
    /// running sizes
    #[test]
    fn sizes_advance_from_the_parent_record() {
        let mut chain = MockChain::regtest();
        let one = chain
            .mine(|b| b.coinbase(|c| c.sapling_output(1).sapling_output(2).orchard_action(1, 1)));
        let two = chain.mine(|b| {
            b.coinbase(|c| {
                let c = c.sapling_output(3).sapling_output(4).sapling_output(5);
                let c = c.ironwood_action(1, 1).ironwood_action(2, 2);
                c.ironwood_action(3, 3).ironwood_action(4, 4)
            })
        });
        let three = chain.mine(|b| {
            b.coinbase(|c| {
                let c = c.orchard_action(2, 2).orchard_action(3, 3);
                let c = c.orchard_action(4, 4).orchard_action(5, 5);
                c.orchard_action(6, 6).ironwood_action(5, 5)
            })
        });
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);

        for (at, after) in [
            (chain.genesis(), sizes(0, 0, 0)),
            (one, sizes(2, 1, 0)),
            (two, sizes(5, 1, 4)),
            (three, sizes(5, 6, 5)),
        ] {
            let (block, fees) = (chain.block(at.hash), chain.fees(at.hash));
            let mut changes = store.changes(block.block_ref());
            fold(&CompactBlockReader::new(store.applied()), block, &fees, &mut changes);
            let records: Vec<&[u8]> = changes.appends(BLOCKS).collect();
            let expected = encode_compact_block(block, &fees, &after);
            assert_eq!(records, [&expected[..]], "{at:?}: one record, encoded with {after:?}");
            store.apply(changes);
            let tip = CompactBlockReader::new(store.applied()).tip_sizes();
            assert_eq!(tip, after, "{at:?}: tip sizes after its fold");
        }
    }

    /// Parent record claiming `u32::MAX` sapling leaves + one more leaf = past a depth-32 tree
    /// (sizes read off the record, not carried)
    #[test]
    #[should_panic(expected = "Sapling commitment tree past 2^32 leaves")]
    fn a_fold_onto_a_full_sapling_tree_record_panics() {
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.coinbase(|c| c.sapling_output(1)));
        let (genesis, one) = (chain.block(chain.genesis().hash), chain.block(one.hash));
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let mut changes = store.changes(genesis.block_ref());
        let full = encode_compact_block(
            genesis,
            &chain.fees(genesis.header().hash),
            &sizes(u32::MAX, 0, 0),
        );
        changes.sequence(BLOCKS).append(&full);
        store.apply(changes);

        let mut changes = store.changes(one.block_ref());
        let parent = CompactBlockReader::new(store.applied());
        fold(&parent, one, &chain.fees(one.header().hash), &mut changes);
    }

    /// Writer as zainod runs it: the finalized stream + utxo-set's fee stream, one fee per step
    struct Running {
        writer: Writer<DiskView>,
        fee_sink: FeeSink,
        fees: Vec<BlockFees>,
    }

    impl Running {
        /// `fees` = the chain's, by height
        fn start(store: DiskStore, fees: &[BlockFees]) -> Self {
            let index = CompactBlockIndexWriter::new(store);
            let mut fee_sink = FeeSink::new();
            let paid = fee_sink.subscribe(NAME, QUEUE);
            let writer =
                Writer::spawn(index.handle(), QUEUE, move |blocks| index.run(blocks, paid));
            Self { writer, fee_sink, fees: fees.to_vec() }
        }

        /// `step` behind its block's fees (as utxo-set sends them)
        async fn send(&self, step: Step<Block>) {
            let (Step::Apply(block) | Step::FinalizedTip(block)) = &step else {
                panic!("Shutdown = `stop`");
            };
            let height = block.header().height;
            let fees = self.fees.iter().find(|fees| fees.height == height);
            let fees = Arc::new(fees.expect("fees for every height").clone());
            self.fee_sink.send(Step::Apply(fees)).await;
            self.writer.send(step).await;
        }

        async fn stop(self) -> DiskView {
            self.fee_sink.shutdown();
            self.writer.stop().await.expect("stops at Shutdown").durable_view()
        }
    }

    /// One stored record, decoded to what the model predicts
    #[derive(Debug, PartialEq)]
    struct Stored {
        hash: BlockHash,
        sizes: TreeSizes,
        fees: Vec<u32>,
    }

    fn stored(record: &[u8]) -> Stored {
        let decoded =
            compact_formats::CompactBlock::decode(&record[FRAME_HEADER..]).expect("decodes");
        Stored {
            hash: record_hash(record).expect("carries its hash"),
            sizes: record_sizes(record).expect("carries its chainMetadata"),
            fees: decoded.vtx.iter().map(|tx| tx.fee).collect(),
        }
    }

    /// Commitments one block adds, per pool (0..=3 each)
    fn commits() -> impl proptest::strategy::Strategy<Value = PerPool<u32>> {
        (0u32..=3, 0u32..=3, 0u32..=3).prop_map(|(sapling, orchard, ironwood)| PerPool {
            sapling,
            orchard,
            ironwood,
        })
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random blocks through random finalized streams (runs, restarts, a resent held block's
        /// fees popped + block skipped), once every move committed:
        ///
        /// - each height's record: hash + cumulative tree sizes = independently summed model
        /// - each height's fees = its own block's
        /// - full range read = those records in order
        #[test]
        fn random_histories_serve_records_with_the_models_tree_sizes_and_fees(
            per_block in proptest::collection::vec(commits(), 1..10),
            moves in moves(),
            write_buffer in write_buffers(),
        ) {
            block_on_paused(random_history(per_block, moves, write_buffer))?;
        }
    }

    async fn random_history(
        per_block: Vec<PerPool<u32>>,
        moves: Vec<Move>,
        write_buffer: NonZeroUsize,
    ) -> Result<(), TestCaseError> {
        // block h: coinbase pays 1 000 × h, spent whole (tx 0 output 0) by one tx paying it all as
        // fee and committing `per_block[h - 1]`
        let miner = p2pkh([0xc0; 20]);
        let mut mock = MockChain::regtest();
        for (at, adds) in (1u32..).zip(&per_block) {
            let fee = 1_000 * u64::from(at);
            mock.mine(|b| {
                b.coinbase(|c| c.pay(&miner, fee)).tx(|t| {
                    let t = t.spend_in_block(0, 0).fee(fee);
                    let t = (0..adds.sapling).fold(t, |t, i| t.sapling_output(16 * at + i));
                    let t = (0..adds.orchard).fold(t, |t, i| t.orchard_action(16 * at + i, 1));
                    (0..adds.ironwood).fold(t, |t, i| t.ironwood_action(16 * at + i, 1))
                })
            });
        }
        let (blocks, fees_by_height) = blocks_and_fees(&mock);
        let model = |height: usize| Stored {
            hash: blocks[height].header().hash,
            sizes: per_block[..height]
                .iter()
                .fold(PerPool::<u32>::default(), |sum, adds| PerPool {
                    sapling: sum.sapling + adds.sapling,
                    orchard: sum.orchard + adds.orchard,
                    ironwood: sum.ironwood + adds.ironwood,
                })
                .map(TreeSize::from),
            fees: match height {
                0 => vec![0],
                paid => vec![0, 1_000 * u32::try_from(paid).expect("< 10 blocks")],
            },
        };

        let fs = SimFs::new();
        let mut stream = FinalizedStreamFixture::new(&blocks);
        let mut index = Running::start(open(&fs, &SCHEMA, write_buffer), &fees_by_height);
        for (at, next) in moves.into_iter().enumerate() {
            if next == Move::Reopen {
                index.stop().await;
                index = Running::start(open(&fs, &SCHEMA, write_buffer), &fees_by_height);
            }
            for step in stream.steps(next) {
                index.send(step).await;
            }
            index.writer.durable_at(stream.tip()).await;

            let case = format!("move {at} {next:?}");
            let reader = CompactBlockReader::new(index.writer.handle().durable_view());
            let mut records = Vec::new();
            for (height, block) in blocks[..stream.sent()].iter().enumerate() {
                let record = reader.block(block.header().height);
                prop_assert!(record.is_some(), "{case}: no record at {height}");
                let record = record.expect("checked above");
                prop_assert_eq!(stored(&record), model(height), "{}: record {}", case, height);
                records.extend_from_slice(&record);
            }
            if let Some(last) = stream.tip() {
                let (window, reach) = reader.range(H0, last, usize::MAX);
                let window = window.concat();
                prop_assert_eq!(reach, last, "{}: range reach", case);
                prop_assert!(
                    window[..] == records[..window.len()],
                    "{case}: range 0..={last:?} = the records read one by one, in order"
                );
            }
        }
        index.stop().await;
        Ok(())
    }

    /// Genesis + five bulk blocks, each its own commit, crashed after every operation: each state
    /// reopens to an acknowledged or the attempted commit, serves those records byte for byte,
    /// and the next block's tree sizes fold onto its tip record (not from zero)
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_durable_prefix_the_next_block_folds_onto() {
        // cumulative sizes distinct at every height
        let per_block = [(1, 0, 2), (2, 1, 0), (0, 3, 1), (4, 1, 1), (1, 2, 3)]
            .map(|(sapling, orchard, ironwood)| PerPool { sapling, orchard, ironwood });
        let mut mock = MockChain::regtest();
        for (at, adds) in (1u32..).zip(per_block) {
            mock.mine(|b| {
                b.tx(|t| {
                    let t = (0..adds.sapling).fold(t, |t, i| t.sapling_output(16 * at + i));
                    let t = (0..adds.orchard).fold(t, |t, i| t.orchard_action(16 * at + i, 1));
                    (0..adds.ironwood).fold(t, |t, i| t.ironwood_action(16 * at + i, 1))
                })
            });
        }
        let (blocks, fees_by_height) = blocks_and_fees(&mock);
        let sizes_after = |count: u32| {
            let added = |pool: fn(&PerPool<u32>) -> u32| {
                (0..count).zip(&per_block).map(|(_, adds)| pool(adds)).sum()
            };
            sizes(added(|p| p.sapling), added(|p| p.orchard), added(|p| p.ironwood))
        };
        let (fs, recording) = SimFs::recording();
        let mut index = Running::start(open(&fs, &SCHEMA, NonZeroUsize::MIN), &fees_by_height);
        for (acked, block) in (1u64..).zip(&blocks[..5]) {
            index.send(step(block)).await;
            index.writer.durable_at(block.header().height).await;
            fs.set_tag(acked);
        }
        let reader = CompactBlockReader::new(index.stop().await);
        let records: Vec<_> = (0..5).map(|n| reader.block(h(n))).collect();

        let states = recording.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MIN);
            let recovered = store.durable().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let acknowledged = state.tag.min(5);
            let attempted = (acknowledged + 1).min(5);
            let case =
                format!("{}: recovered {recovered} blocks, next height {recovered}", state.label);
            assert!(
                [acknowledged, attempted].contains(&u64::from(recovered)),
                "{case}: acknowledged {acknowledged}, attempted {attempted}"
            );
            let reader = CompactBlockReader::new(store.durable());
            for (height, acked) in (0..recovered).zip(&records) {
                let served = reader.block(h(height));
                assert!(served == *acked, "{case}: record {height} byte-identical");
            }

            let next = mock.block(mock.at(h(recovered)).hash);
            let mut index = Running::start(store, &fees_by_height);
            index.send(step(next)).await;
            index.writer.durable_at(h(recovered)).await;
            let next = CompactBlockReader::new(index.stop().await).block(h(recovered));
            let next = stored(&next.expect("next block's record"));
            assert_eq!(next.sizes, sizes_after(recovered), "{case}: folded onto the tip record");
        }
    }
}
