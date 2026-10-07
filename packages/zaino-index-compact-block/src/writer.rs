//! compact_block writer: the final stream → one [`fold`] per block → its store
//!
//! - fees: one [`BlockFees`] off value-balance's sink per unfolded step, held heights included
//!   (value-balance re-folds them: both streams stay in step with either index ahead)

use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::watch;
use zaino_persistence::{IndexKind, SequenceRead, Store, View};
use zaino_primitives::types::{Block, BlockFees};
use zaino_sync::{Committer, Final, Step, Subscription};

use crate::{fold, position, CompactBlockReader, BLOCKS};

const NAME: &str = IndexKind::CompactBlock.name();

pub struct CompactBlockIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: SequenceRead>> CompactBlockIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = buffered bytes per
    /// bulk commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let view = store.view();
        let held = view.tip().map_or(0, |tip| position(tip.height) + 1);
        assert_eq!(view.len(BLOCKS), held, "{NAME}: one record per committed height");
        Self { store: Committer::new(store, batch_bytes) }
    }

    /// For `Nfs::subscribe`: the committed view after every commit
    pub fn committed(&self) -> watch::Receiver<S::View> {
        self.store.committed()
    }

    /// Follows `blocks` and `fees` (value-balance's) through their `Shutdown` (a failure panics)
    pub async fn run(mut self, mut blocks: Subscription<Final>, mut fees: Subscription<BlockFees>) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let mut paid = Vec::with_capacity(run.unfolded.len());
            for (_, block) in &run.unfolded {
                paid.push(next_fees(&mut fees, block).await);
            }
            let applied = move |store: &mut S| {
                let network = store.schema().network;
                let mut paid = paid.into_iter();
                run.apply(store, |store, block| {
                    let height = block.header().height;
                    let fees = paid.find(|fees| fees.height == height).expect("one per step");
                    let parent = CompactBlockReader::new(store.staged(), network);
                    let folded = fold(&parent, block, &fees);
                    folded.unwrap_or_else(|error| panic!("{NAME} index at {height}: {error}"))
                });
            };
            self.store.compute(applied).await;
        }
        let ended = matches!(fees.next().await, Step::Shutdown);
        assert!(ended, "{NAME}: fees past the last block");
    }
}

/// Value-balance's fees for unfolded `block`
async fn next_fees(fees: &mut Subscription<BlockFees>, block: &Block) -> Arc<BlockFees> {
    let Step::Apply { height, data } = fees.next().await else {
        panic!("{NAME}: fees ended before the blocks");
    };
    assert!(data.belongs_to(block), "{NAME}: fees at {height} for another block");
    data
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use proptest::strategy::Strategy as _;
    use prost::Message as _;
    use tokio::task::JoinHandle;
    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine};
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        CompactCiphertext, Fee, Height, OrchardAction, OrchardData, SaplingData, SaplingOutput,
        Transaction, TransactionId, TransparentData, Zatoshis,
    };
    use zaino_proto::frame::FRAME_HEADER;
    use zaino_proto::proto::compact_formats as cf;
    use zaino_sync::{FeeSink, Folds, IndexerDataSink};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::schema;

    const NETWORK: NetworkType = NetworkType::Regtest;
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        DiskEngine::new(fs.clone()).open(Path::new("/cb"), &schema(NETWORK)).expect("open")
    }

    /// Coinbase, then one tx (txid `[seed; 32]`) committing `sapling` outputs, `orchard` and
    /// `ironwood` actions
    fn txs(seed: u8, sapling: usize, orchard: usize, ironwood: usize) -> Vec<Transaction> {
        let out = SaplingOutput {
            cmu: [1u8; 32].into(),
            ephemeral_key: [2u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
        };
        let action = OrchardAction {
            nullifier: [4u8; 32].into(),
            cmx: [5u8; 32].into(),
            ephemeral_key: [6u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([7u8; CompactCiphertext::LENGTH]),
        };

        vec![
            Transaction {
                txid: TransactionId::from([0xcb; 32]),
                transparent: TransparentData { coinbase: true, ..Default::default() },
                sprout: Default::default(),
                sapling: Default::default(),
                orchard: Default::default(),
                ironwood: Default::default(),
            },
            Transaction {
                txid: TransactionId::from([seed; 32]),
                transparent: Default::default(),
                sprout: Default::default(),
                sapling: SaplingData { outputs: vec![out; sapling], ..Default::default() },
                orchard: OrchardData {
                    actions: vec![action.clone(); orchard],
                    ..Default::default()
                },
                ironwood: OrchardData { actions: vec![action; ironwood], ..Default::default() },
            },
        ]
    }

    /// Fees value-balance states for `block`: coinbase, then `1 000 × (height + 1)` (distinct
    /// per height: a pairing slip = a wrong fee in the record)
    fn fees_of(block: &Block) -> BlockFees {
        let paid = Zatoshis::new(1_000 * (u64::from(block.header().height) + 1)).expect("supply");
        let (height, hash) = (block.header().height, block.header().hash);
        BlockFees { height, hash, fees: vec![Fee::Coinbase, Fee::Paid(paid)] }
    }

    /// Each block's own fold from genesis, as the NFS folds it (the folded steps' payload)
    fn folded(chain: &[Arc<Block>]) -> Vec<Arc<Folds>> {
        let mut scratch = open(&SimFs::new());
        chain
            .iter()
            .map(|block| {
                let parent = CompactBlockReader::new(scratch.staged(), NETWORK);
                let changes = fold(&parent, block, &fees_of(block)).expect("small sizes");
                let mut folds = Folds::default();
                folds.insert(IndexKind::CompactBlock, changes.clone());
                scratch.apply(changes);
                Arc::new(folds)
            })
            .collect()
    }

    /// Writer as zainod runs it: the NFS's final stream, value-balance's fee stream (fees
    /// per unfolded step, [`fees_of`]), its committed view
    struct Running {
        blocks: IndexerDataSink<Final>,
        fees: FeeSink,
        committed: watch::Receiver<DiskView>,
        run: JoinHandle<()>,
    }

    impl Running {
        fn start(store: DiskStore, batch: NonZeroUsize) -> Self {
            let writer = CompactBlockIndexWriter::new(store, batch);
            let committed = writer.committed();
            let (mut blocks, mut fees) = (IndexerDataSink::new("final"), FeeSink::new("fees"));
            let (block_sub, fee_sub) = (blocks.subscribe(NAME, QUEUE), fees.subscribe(NAME, QUEUE));
            let run = tokio::spawn(writer.run(block_sub, fee_sub));
            Self { blocks, fees, committed, run }
        }

        /// `block`, folded (`folds`) or not; its fees too when not (as value-balance sends them)
        async fn send(&self, block: &Arc<Block>, folds: Option<&Arc<Folds>>) {
            let height = block.header().height;
            if folds.is_none() {
                self.fees.send(Step::Apply { height, data: Arc::new(fees_of(block)) }).await;
            }
            let data = Arc::new(Final { block: Arc::clone(block), folds: folds.map(Arc::clone) });
            self.blocks.send(Step::Apply { height, data }).await;
        }

        async fn reached(&mut self, tip: Option<u32>) {
            let at = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height)) == tip;
            self.committed.wait_for(at).await.expect("writer alive");
        }

        async fn stop(self) -> watch::Receiver<DiskView> {
            self.fees.shutdown();
            self.blocks.shutdown();
            self.run.await.expect("stops at Shutdown");
            self.committed
        }
    }

    /// `(sapling, orchard, ironwood)` sizes and per-tx fees of the record at `height`
    fn record(view: &DiskView, height: u32) -> ((u32, u32, u32), Vec<u32>) {
        let reader = CompactBlockReader::new(view.clone(), NETWORK);
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

    /// Tree sizes accumulate across unfolded (writer-folded) and folded steps, and a restart
    /// resending held heights (their fees popped, their records untouched) folds the next block
    /// onto the committed tip record (not zero)
    #[tokio::test(start_paused = true)]
    async fn tree_sizes_and_fees_accumulate_across_folded_steps_and_a_restart() {
        let fs = SimFs::new();
        let chain = linked([txs(0, 2, 1, 0), txs(1, 3, 2, 1), txs(2, 0, 0, 4), txs(3, 1, 1, 1)]);
        let folds = folded(&chain);

        let mut index = Running::start(open(&fs), QUEUE);
        index.send(&chain[0], None).await;
        index.send(&chain[1], None).await;
        index.reached(Some(1)).await;
        index.send(&chain[2], Some(&folds[2])).await;
        index.reached(Some(2)).await;
        let committed = index.stop().await;
        let view = committed.borrow().clone();
        let stored = [0, 1, 2].map(|height| record(&view, height));
        let fees = |height: u32| vec![0, 1_000 * (height + 1)];
        let expected = [((2, 1, 0), fees(0)), ((5, 3, 1), fees(1)), ((5, 3, 5), fees(2))];
        assert_eq!(stored, expected, "cumulative sizes, each block's own fees");

        let mut resumed = Running::start(open(&fs), QUEUE);
        resumed.send(&chain[1], None).await;
        resumed.send(&chain[2], None).await;
        resumed.send(&chain[3], Some(&folds[3])).await;
        resumed.reached(Some(3)).await;
        let committed = resumed.stop().await;
        let view = committed.borrow().clone();
        assert_eq!(record(&view, 2), expected[2], "held: untouched");
        assert_eq!(record(&view, 3), ((6, 4, 6), fees(3)), "folded onto 2's record");
    }

    /// - `Send(n)`: next `n` blocks, unfolded until `Fold`, folded after it
    /// - `Fold`: bulk → tip handoff; `Reopen`: shutdown, reopen, resend from one below the tip
    ///   (held: fees popped, skipped)
    #[derive(Debug, Clone)]
    enum Move {
        Send(usize),
        Fold,
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random blocks through random final streams (bulk, tip, restarts): once every move
        /// commits, each height serves a record whose hash and cumulative tree sizes equal an
        /// independently summed model and whose fees are its own block's, and a full range read
        /// is those records in order
        #[test]
        fn random_histories_serve_records_with_the_models_tree_sizes_and_fees(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    4 => (1usize..=3).prop_map(Move::Send),
                    1 => proptest::strategy::Just(Move::Fold),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
            batch in proptest::prop_oneof![
                proptest::strategy::Just(NonZeroUsize::MIN),
                proptest::strategy::Just(QUEUE),
            ],
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves, batch));
        }
    }

    async fn random_history(
        counts: Vec<(usize, usize, usize)>,
        moves: Vec<Move>,
        batch: NonZeroUsize,
    ) {
        let chain: Vec<Arc<Block>> =
            linked((0u8..).zip(&counts).map(|(seed, &(s, o, i))| txs(seed, s, o, i)));
        let folds = folded(&chain);
        let sizes_through = |height: usize| {
            counts[..=height].iter().fold((0u32, 0u32, 0u32), |(s, o, i), &(ds, d_o, di)| {
                (s + ds as u32, o + d_o as u32, i + di as u32)
            })
        };

        let fs = SimFs::new();
        let mut index = Running::start(open(&fs), batch);
        let (mut sent, mut folding) = (0usize, false);
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Send(count) => {
                    for (block, block_folds) in chain.iter().zip(&folds).skip(sent).take(count) {
                        index.send(block, folding.then_some(block_folds)).await;
                        sent += 1;
                    }
                }
                Move::Fold => folding = true,
                Move::Reopen => {
                    index.stop().await;
                    index = Running::start(open(&fs), batch);
                    folding = false;
                    if let Some(held) = sent.checked_sub(1) {
                        index.send(&chain[held], None).await;
                    }
                }
            }
            index.reached(sent.checked_sub(1).map(|last| last as u32)).await;

            let case = format!("move {at} {next:?}");
            let view = index.committed.borrow().clone();
            let reader = CompactBlockReader::new(view.clone(), NETWORK);
            let mut records = Vec::new();
            for height in 0..sent as u32 {
                let (sizes, fees) = record(&view, height);
                let stored = reader.block(h(height)).expect("record");
                let decoded = cf::CompactBlock::decode(&stored[FRAME_HEADER..]).expect("decodes");
                let hash = <[u8; 32]>::from(chain[height as usize].header().hash);
                assert_eq!(decoded.hash, hash.to_vec(), "{case}: record {height}");
                assert_eq!(sizes, sizes_through(height as usize), "{case}: record {height}");
                assert_eq!(fees, vec![0, 1_000 * (height + 1)], "{case}: its own fees");
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

    /// Four bulk blocks, each its own commit, crashed after every operation: each state reopens
    /// to an acknowledged or the attempted commit, serves those records byte for byte, and the
    /// next block's tree sizes fold onto its tip record (not from zero)
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_committed_prefix_the_next_block_folds_onto() {
        // (sapling, orchard, ironwood) per block: cumulative sizes distinct at every height
        let counts = [(1, 0, 2), (2, 1, 0), (0, 3, 1), (4, 1, 1), (1, 2, 3)];
        let chain = linked((0u8..).zip(counts).map(|(seed, (s, o, i))| txs(seed, s, o, i)));
        let sizes_through = |count: usize| {
            counts[..count].iter().fold((0, 0, 0), |(s, o, i), &(ds, d_o, di)| {
                (s + ds as u32, o + d_o as u32, i + di as u32)
            })
        };
        let fs = SimFs::recording();
        let mut index = Running::start(open(&fs), NonZeroUsize::MIN);
        for (acked, block) in (1u64..).zip(&chain[..4]) {
            index.send(block, None).await;
            index.reached(Some(u32::from(block.header().height))).await;
            fs.set_tag(acked);
        }
        let committed = index.stop().await;
        let reader = CompactBlockReader::new(committed.borrow().clone(), NETWORK);
        let records: Vec<_> = (0..4).map(|n| reader.block(h(n))).collect();

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs);
            let count = store.view().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(4) as usize);
            assert!(acked.contains(&count), "{label}: recovered {count} blocks");
            let reader = CompactBlockReader::new(store.view(), NETWORK);
            let served: Vec<_> = (0..count as u32).map(|n| reader.block(h(n))).collect();
            assert_eq!(served, records[..count], "{label}: byte-identical records");

            let mut index = Running::start(store, NonZeroUsize::MIN);
            index.send(&chain[count], None).await;
            index.reached(Some(count as u32)).await;
            let committed = index.stop().await;
            let (sizes, _) = record(&committed.borrow(), count as u32);
            assert_eq!(sizes, sizes_through(count + 1), "{label}: folded onto the tip record");
        }
    }
}
