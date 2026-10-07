//! compact_block index: one [`fold`] per block over the held view, kept by its own loop
//!
//! - storage tiers = `zaino_persistence::Tiered`
//! - fees: one [`BlockFees`] step off value-balance's sink per block step, awaited after it

use std::{num::NonZeroUsize, sync::Arc};

use zaino_persistence::{Changes, IndexKind, LayeredView, SequenceRead, Store, Tiered, View};
use zaino_primitives::types::{Block, BlockFees, BlockRef, Height};
use zaino_sync::{Offloaded, Published, Step, Subscription, Weight};

use crate::{fold, position, CompactBlockReader, BLOCKS};

pub struct CompactBlockIndexWriter<S: Store> {
    tiered: Offloaded<Tiered<S>>,
    published: Published<CompactBlockReader<LayeredView<S::View>>>,
}

const NAME: &str = IndexKind::CompactBlock.name();

/// One block step + value-balance's fee step for it
enum Paired {
    Apply { height: Height, finalized: bool, block: Arc<Block>, fees: Arc<BlockFees> },
    Finalized { height: Height },
    Reorg,
    Shutdown,
}

/// Panics unless `block` and `fees` = the same step of the same block
fn pair(block: Step<Block>, fees: Step<BlockFees>) -> Paired {
    match (block, fees) {
        (
            Step::Apply { height, finalized, data: block },
            Step::Apply { height: fee_height, finalized: fee_finalized, data: fees },
        ) => {
            assert_eq!(
                (height, finalized),
                (fee_height, fee_finalized),
                "compact_block: fees out of step"
            );
            assert!(fees.belongs_to(&block), "compact_block: fees at {height} for another block");
            Paired::Apply { height, finalized, block, fees }
        }
        (Step::Finalized { height }, Step::Finalized { height: fee_height }) => {
            assert_eq!(height, fee_height, "compact_block: fee Finalized out of step");
            Paired::Finalized { height }
        }
        (Step::Reorg, Step::Reorg) => Paired::Reorg,
        (Step::Shutdown, Step::Shutdown) => Paired::Shutdown,
        _ => panic!("compact_block: block and fee steps out of step"),
    }
}

impl<S: Store<View: SequenceRead>> CompactBlockIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = final blocks per bulk
    /// commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let tiered = Tiered::new(store, batch_bytes);
        let view = tiered.view();
        let held = view.tip().map_or(0, |tip| position(tip.height) + 1);
        assert_eq!(view.len(BLOCKS), held, "{NAME}: one record per committed height");
        let reader = CompactBlockReader::new(view, tiered.schema().network);
        let published = Published::new(reader, tiered.durable_tip());
        Self { tiered: Offloaded::new(tiered), published }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.tiered.get().durable_tip()
    }

    /// Reader, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<CompactBlockReader<LayeredView<S::View>>> {
        &self.published
    }

    /// Follows `blocks` and `fees` (value-balance's, step for step) through their `Shutdown` (a
    /// failure panics: its dropped queues fail the rest)
    pub async fn run(mut self, mut blocks: Subscription<Block>, mut fees: Subscription<BlockFees>) {
        loop {
            match pair(blocks.next().await, fees.next().await) {
                Paired::Apply { height, finalized: true, block, fees } => {
                    self.apply_final(height, &block, &fees).await
                }
                Paired::Apply { block, fees, .. } => self.apply_tip(&block, &fees).await,
                Paired::Finalized { height } => self.finalize(height).await,
                Paired::Reorg => self.reorg(),
                Paired::Shutdown => return self.finalize_staged().await,
            }
        }
    }

    /// Final block (bulk sync): staged for the next batch commit
    async fn apply_final(&mut self, height: Height, block: &Block, fees: &BlockFees) {
        // replay for an index behind this one: already on disk
        if Some(height) <= self.durable_tip().map(|tip| tip.height) {
            return;
        }
        let changes = self.changes(block, fees);
        let full = self.tiered.get_mut().stage(changes, block.weight() + fees.weight());
        self.published.merged(height);
        if full {
            self.finalize(height).await;
        }
    }

    async fn apply_tip(&mut self, block: &Block, fees: &BlockFees) {
        self.finalize_staged().await;
        let changes = self.changes(block, fees);
        self.tiered.get_mut().apply(changes);
        self.publish();
    }

    /// Back to the durable tip (the winning branch folds onto its record from there)
    fn reorg(&mut self) {
        self.tiered.get_mut().reorg();
        self.publish();
        self.published.reorged();
    }

    /// Staged bulk → disk (before a tip block builds on it, and at `Shutdown`)
    async fn finalize_staged(&mut self) {
        if let Some(staged) = self.tiered.get().staged() {
            self.finalize(staged.height).await;
        }
    }

    /// Every held block through `through` → disk
    async fn finalize(&mut self, through: Height) {
        self.tiered.blocking(move |tiered| tiered.finalize(through)).await;
        // view first: a reader woken by the durable tip pins the view that includes it
        self.publish();
        self.published.durable(self.durable_tip().map(|tip| tip.height));
    }

    /// `block` folded onto everything held (encoded once: one copy for serving and the commit)
    fn changes(&self, block: &Block, fees: &BlockFees) -> Changes {
        let folded = fold(&self.reader(), block, fees);
        folded.unwrap_or_else(|error| panic!("{NAME} index at {}: {error}", block.header().height))
    }

    fn reader(&self) -> CompactBlockReader<LayeredView<S::View>> {
        let tiered = self.tiered.get();
        CompactBlockReader::new(tiered.view(), tiered.schema().network)
    }

    fn publish(&self) {
        self.published.view(self.reader(), self.tiered.get().applied());
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use proptest::strategy::Strategy as _;
    use prost::Message as _;
    use tokio::{sync::watch, task::JoinHandle};
    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine};
    use zaino_primitives::testing::{linked, Chain};
    use zaino_primitives::types::{
        BlockRef, CompactCiphertext, Fee, OrchardAction, OrchardData, SaplingData, SaplingOutput,
        Transaction, TransactionId, TransparentData, Zatoshis,
    };
    use zaino_proto::proto::compact_formats as cf;
    use zaino_sync::{BlockSink, FeeSink, Served};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::schema;
    use zaino_proto::frame::FRAME_HEADER;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// Fee block `height`'s paying tx carries (distinct per height: a pairing slip = a wrong fee)
    fn fee_at(height: u32) -> u32 {
        1_000 * (height + 1)
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

    fn apply(finalized: bool, block: Arc<Block>) -> Step<Block> {
        Step::Apply { height: block.header().height, finalized, data: block }
    }

    /// Tree sizes the store holds at `height`
    fn stored_sizes(fs: &Arc<SimFs>, height: u32) -> (u32, u32, u32) {
        let reopened = open(fs).published().served().pin_any();
        assert_eq!(reopened.resident_block(h(height)), None, "{height}: read off the store");
        let record = reopened.block(h(height)).expect("record");
        let meta = cf::CompactBlock::decode(&record[FRAME_HEADER..])
            .expect("decodes")
            .chain_metadata
            .expect("carries metadata");
        (
            meta.sapling_commitment_tree_size,
            meta.orchard_commitment_tree_size,
            meta.ironwood_commitment_tree_size,
        )
    }

    /// `/cb` on `fs` at its committed state, every final block its own commit
    fn open(fs: &Arc<SimFs>) -> CompactBlockIndexWriter<DiskStore> {
        let store =
            DiskEngine::new(fs.clone()).open(Path::new("/cb"), &schema(NetworkType::Regtest));
        CompactBlockIndexWriter::new(store.expect("open"), NonZeroUsize::MIN)
    }

    /// The index over `/cb` running as zainod runs it: a producer's block sink, value-balance's
    /// fee sink (fees stated per block: coinbase, then [`fee_at`]), and what it publishes
    struct Running {
        blocks: BlockSink,
        fees: FeeSink,
        served: Served<CompactBlockReader<LayeredView<DiskView>>>,
        applied: watch::Receiver<Option<BlockRef>>,
        durable: watch::Receiver<Option<Height>>,
        run: JoinHandle<()>,
    }

    impl Running {
        /// `batch_bytes` = 1: every final block writes as it arrives
        fn start(fs: &Arc<SimFs>) -> Self {
            let index = open(fs);
            let published = index.published();
            let (served, applied, durable) = (
                published.served(),
                published.subscribe_applied(),
                published.subscribe_finalized(),
            );
            let (mut blocks, mut fees) = (BlockSink::new("blocks"), FeeSink::new("fees"));
            let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
            let (block_sub, fee_sub) = (blocks.subscribe(NAME, queue), fees.subscribe(NAME, queue));
            let run = tokio::spawn(index.run(block_sub, fee_sub));
            Self { blocks, fees, served, applied, durable, run }
        }

        /// `step` to the block sink, value-balance's copy of it to the fee sink
        async fn send(&mut self, step: Step<Block>) {
            let fees = match &step {
                Step::Apply { height, finalized, data } => {
                    let paid = Zatoshis::new(fee_at(u32::from(*height)).into()).expect("supply");
                    let fees = BlockFees {
                        height: *height,
                        hash: data.header().hash,
                        fees: vec![Fee::Coinbase, Fee::Paid(paid)],
                    };
                    Step::Apply { height: *height, finalized: *finalized, data: Arc::new(fees) }
                }
                Step::Finalized { height } => Step::Finalized { height: *height },
                Step::Reorg => Step::Reorg,
                Step::Shutdown => unreachable!("`stop` ends both sinks"),
            };
            self.blocks.send(step).await;
            self.fees.send(fees).await;
        }

        /// Both tips published at these heights (the view published with them)
        async fn settled(&mut self, applied: Option<Height>, durable: Option<Height>) {
            let tips = async {
                self.durable.wait_for(|at| *at == durable).await.expect("index alive");
                let at = |tip: &Option<BlockRef>| tip.map(|tip| tip.height) == applied;
                self.applied.wait_for(at).await.expect("index alive");
            };
            let waited = tokio::time::timeout(Duration::from_secs(5), tips).await;
            let now_applied = self.applied.borrow().map(|tip| tip.height);
            let now_durable = *self.durable.borrow();
            waited.unwrap_or_else(|_| {
                panic!("tips {now_applied:?} / {now_durable:?}, want {applied:?} / {durable:?}")
            });
        }

        /// Shutdown down both sinks, the index drained through it
        async fn stop(self) {
            self.blocks.shutdown();
            self.fees.shutdown();
            self.run.await.expect("followed through Shutdown");
        }
    }

    /// Tree sizes accumulate across bulk and tip blocks; after a reopen and after a reorg, the next
    /// block folds onto the durable tip record (not zero, not the losing branch)
    #[tokio::test]
    async fn tree_sizes_accumulate_and_survive_a_restart_and_a_reorg() {
        let fs = SimFs::new();
        let mut index = Running::start(&fs);
        let through = |n| Some(h(n));

        // 0..=2 one branch; a losing and a winning 3 both on 2
        let mut chain = Chain::with_genesis(txs(0, 2, 1, 0));
        let one = chain.mine_with(chain.genesis().hash, txs(1, 3, 2, 1));
        let two = chain.mine_with(one.hash, txs(2, 0, 0, 4));
        let (losing, winning) = (
            chain.mine_with(two.hash, txs(3, 9, 9, 9)),
            chain.mine_with(two.hash, txs(3, 1, 1, 1)),
        );
        let block = |at: BlockRef| Arc::new(chain.block(at.hash).clone());

        // 0, 1 final on arrival (bulk sync); 2 applied at the tip, not yet final
        index.send(apply(true, block(chain.genesis()))).await;
        index.send(apply(true, block(one))).await;
        index.send(apply(false, block(two))).await;
        index.settled(through(2), through(1)).await;
        let view = index.served.pin_any();
        assert!(view.resident_block(h(1)).is_none(), "bulk sync never touches non-finalized");
        assert!(view.resident_block(h(2)).is_some(), "applied, not durable: non-finalized");

        index.send(Step::Finalized { height: h(2) }).await;
        index.settled(through(2), through(2)).await;
        let view = index.served.pin_any();
        assert!(view.resident_block(h(2)).is_none(), "written block leaves non-finalized");
        index.stop().await;
        let stored = [0, 1, 2].map(|height| stored_sizes(&fs, height));
        assert_eq!(stored, [(2, 1, 0), (5, 3, 1), (5, 3, 5)], "cumulative, not per-block");

        // reopened: resumes at 2; a losing 3, a reorg, the winning 3 folds onto the right totals
        let mut resumed = Running::start(&fs);
        resumed.settled(through(2), through(2)).await;
        resumed.send(apply(false, block(losing))).await;
        resumed.settled(through(3), through(2)).await;
        resumed.send(Step::Reorg).await;
        resumed.settled(through(2), through(2)).await;
        let view = resumed.served.pin_any();
        assert!(view.resident_block(h(3)).is_none(), "losing branch gone");
        resumed.send(apply(false, block(winning))).await;
        resumed.send(Step::Finalized { height: h(3) }).await;
        resumed.settled(through(3), through(3)).await;
        resumed.stop().await;
        // folded onto 2's record across restart + reorg: not zero, not the losing branch's 9s
        assert_eq!(stored_sizes(&fs, 3), (6, 4, 6));
    }

    #[derive(Debug, Clone)]
    enum Move {
        Apply,
        Finalize(usize),
        Reorg,
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random blocks through a producer's random step sequences (bulk, tip applies,
        /// finalizes, reorgs with replay, restarts): after every move each applied height serves
        /// a record whose hash and cumulative tree sizes equal an independently summed model and
        /// whose fees are its own block's, and a full durable read is those records in order
        #[test]
        fn random_histories_serve_records_with_the_models_tree_sizes_and_fees(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => proptest::strategy::Just(Move::Apply),
                    2 => (1usize..=4).prop_map(Move::Finalize),
                    1 => proptest::strategy::Just(Move::Reorg),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves));
        }
    }

    async fn random_history(counts: Vec<(usize, usize, usize)>, moves: Vec<Move>) {
        let chain: Vec<Arc<Block>> =
            linked((0u8..).zip(&counts).map(|(seed, &(s, o, i))| txs(seed, s, o, i)));
        let sizes_through = |height: usize| {
            counts[..=height].iter().fold((0u32, 0u32, 0u32), |(s, o, i), &(ds, d_o, di)| {
                (s + ds as u32, o + d_o as u32, i + di as u32)
            })
        };
        let tip = |count: usize| count.checked_sub(1).map(|last| h(last as u32));

        let fs = SimFs::new();
        let mut index = Running::start(&fs);
        // the producer's side: blocks final (= durable once written), blocks sent in all
        let (mut finals, mut sent) = (0usize, 0usize);
        for (at, step) in moves.iter().enumerate() {
            match *step {
                Move::Apply if sent < chain.len() => {
                    index.send(apply(false, Arc::clone(&chain[sent]))).await;
                    sent += 1;
                }
                // window open: its oldest become final; none: the next blocks arrive final (bulk)
                Move::Finalize(n) if sent > finals => {
                    let buried = (finals + n).min(sent);
                    for height in finals..buried {
                        index.send(Step::Finalized { height: h(height as u32) }).await;
                    }
                    finals = buried;
                }
                Move::Finalize(n) => {
                    for next in (sent..chain.len()).take(n) {
                        index.send(apply(true, Arc::clone(&chain[next]))).await;
                        (sent, finals) = (sent + 1, finals + 1);
                    }
                }
                // a producer reorgs only a window it opened, then replays from its first height
                Move::Reorg if sent > finals => {
                    index.send(Step::Reorg).await;
                    sent = finals;
                }
                Move::Reopen => {
                    index.stop().await;
                    index = Running::start(&fs);
                    sent = finals;
                }
                _ => {}
            }
            let case = format!("move {at} {step:?}");
            index.settled(tip(sent), tip(finals)).await;

            // every applied height served from one tier, durable records in order
            let view = index.served.pin_any();
            let sent_tip = sent.checked_sub(1).map(|last| {
                let header = chain[last].header();
                BlockRef { hash: header.hash, height: header.height }
            });
            assert_eq!(view.tip(), sent_tip, "{case}");
            let mut expected_window = Vec::new();
            for height in tip(sent).into_iter().flat_map(|last| Height::GENESIS.up_to(last)) {
                let record =
                    view.block(height).unwrap_or_else(|| panic!("{case}: no record at {height}"));
                let decoded = cf::CompactBlock::decode(&record[FRAME_HEADER..]).expect("decodes");
                let meta = decoded.chain_metadata.expect("metadata");
                let n = u32::from(height);
                let sizes = (
                    meta.sapling_commitment_tree_size,
                    meta.orchard_commitment_tree_size,
                    meta.ironwood_commitment_tree_size,
                );
                let fees: Vec<_> = decoded.vtx.iter().map(|tx| tx.fee).collect();
                let record_case = format!("{case}: record {height}");
                let hash = <[u8; 32]>::from(chain[n as usize].header().hash);
                assert_eq!(decoded.hash, hash.to_vec(), "{record_case}");
                assert_eq!(sizes, sizes_through(n as usize), "{record_case}");
                assert_eq!(fees, vec![0, fee_at(n)], "{record_case}: coinbase unset, its own fee");
                expected_window.extend_from_slice(&record);
            }
            if let Some(last) = tip(finals) {
                let (window, reach) = view.range(Height::GENESIS, last, usize::MAX);
                assert_eq!(reach, last, "{case}");
                let window = window.concat();
                assert_eq!(window, expected_window[..window.len()], "{case}: records in order");
            }
        }
        index.stop().await;
    }

    /// Four final blocks, each its own commit, crashed after every operation: each state reopens
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
        let mut index = Running::start(&fs);
        for (acked, block) in (1u64..).zip(&chain[..4]) {
            let height = Some(block.header().height);
            index.send(apply(true, Arc::clone(block))).await;
            index.settled(height, height).await;
            fs.set_tag(acked);
        }
        let view = index.served.pin_any();
        let records: Vec<_> = (0..4).map(|n| view.block(h(n))).collect();
        index.stop().await;

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let mut index = Running::start(&state.fs);
            let count = index.durable.borrow().map_or(0, |tip| u32::from(tip) as usize + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(4) as usize);
            assert!(acked.contains(&count), "{label}: recovered {count} blocks");
            let view = index.served.pin_any();
            let served: Vec<_> = (0..count as u32).map(|n| view.block(h(n))).collect();
            assert_eq!(served, records[..count], "{label}: byte-identical records");

            let next = h(count as u32);
            index.send(apply(true, Arc::clone(&chain[count]))).await;
            index.settled(Some(next), Some(next)).await;
            let record = index.served.pin_any().block(next).expect("committed after recovery");
            let decoded = cf::CompactBlock::decode(&record[FRAME_HEADER..]).expect("decodes");
            let meta = decoded.chain_metadata.expect("carries metadata");
            let sizes = (
                meta.sapling_commitment_tree_size,
                meta.orchard_commitment_tree_size,
                meta.ironwood_commitment_tree_size,
            );
            assert_eq!(sizes, sizes_through(count + 1), "{label}: folded onto the tip record");
            index.stop().await;
        }
    }
}
