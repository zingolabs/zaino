//! value_balance index: each run of delivered blocks folded onto everything held, kept by its own
//! loop
//!
//! - every step republished into the [`FeeSink`], 1:1 (replayed heights too: an index behind this
//!   one still pairs them)
//! - storage tiers = `zaino_persistence::Tiered`

use std::{num::NonZeroUsize, sync::Arc};

use zaino_persistence::{Changes, MapRead, Store, Tiered};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_sync::{blocking, Applied, FeeSink, Offloaded, Published, Step, Subscription, Weight};

use crate::{fold::fold_run, ValueBalanceReader};

/// Records every transparent output and derives one [`BlockFees`](zaino_primitives::types::BlockFees)
/// per block
///
/// - `batch_bytes` = one run's bytes and one bulk commit's
pub struct ValueBalanceIndexWriter<S: Store> {
    tiered: Offloaded<Tiered<S>>,
    batch_bytes: NonZeroUsize,
    published: Published<()>,
}

const NAME: &str = zaino_persistence::IndexKind::ValueBalance.name();

impl<S: Store<View: MapRead>> ValueBalanceIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = final blocks per
    /// bulk commit (one fsync), and one run's bytes
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let tiered = Tiered::new(store, batch_bytes);
        let published = Published::new((), tiered.durable_tip());
        Self { tiered: Offloaded::new(tiered), batch_bytes, published }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.tiered.get().durable_tip()
    }

    /// Tips and gate, for metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<()> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown`, republished into `fees`
    ///
    /// - a failure panics (dropped `fees` = no `Shutdown`: compact-block panics too)
    pub async fn run(mut self, mut blocks: Subscription<Block>, fees: FeeSink) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized, data } => {
                    let run = blocks.run((height, finalized, data), self.batch_bytes);
                    self.apply_run(run, &fees).await
                }
                Step::Finalized { height } => {
                    fees.send(Step::Finalized { height }).await;
                    self.finalize(height).await
                }
                Step::Reorg => self.reorg(&fees).await,
                Step::Shutdown => return self.shutdown(fees).await,
            }
        }
    }

    /// Whole run folded on the blocking pool (one prevout probe), then each block above
    /// `replayed_through` held and every block's fees out (replays too)
    async fn apply_run(&mut self, run: Vec<Applied<Block>>, fees: &FeeSink) {
        // at or below = replay for an index behind this one: on disk already (taken before the
        // run: a commit inside it moves the durable tip)
        let replayed_through = self.durable_tip().map(|tip| tip.height);
        let blocks: Vec<Arc<Block>> = run.iter().map(|(_, _, block)| Arc::clone(block)).collect();
        let tiered = self.tiered.get();
        let parent = ValueBalanceReader::new(tiered.view(), tiered.schema().network);
        let folded = blocking(move || fold_run(&parent, blocks.iter().map(Arc::as_ref)))
            .await
            .unwrap_or_else(|error| panic!("{NAME} index: {error}"));
        for ((height, finalized, block), (changes, block_fees)) in run.into_iter().zip(folded) {
            if Some(height) > replayed_through {
                self.hold(height, finalized, changes, block.weight()).await;
            }
            fees.send(Step::Apply { height, finalized, data: Arc::new(block_fees) }).await;
        }
    }

    /// Final block (bulk sync) staged for the next batch commit; a tip block applied above
    /// durable (staged blocks written first)
    async fn hold(&mut self, height: Height, finalized: bool, changes: Changes, weight: usize) {
        if finalized {
            let full = self.tiered.get_mut().stage(changes, weight);
            self.published.merged(height);
            if full {
                self.finalize(height).await;
            }
            return;
        }
        self.finalize_staged().await;
        self.tiered.get_mut().apply(changes);
        self.publish();
    }

    /// Back to the durable tip (store untouched: commits final-only)
    async fn reorg(&mut self, fees: &FeeSink) {
        self.tiered.get_mut().reorg();
        self.publish();
        self.published.reorged();
        fees.send(Step::Reorg).await;
    }

    async fn shutdown(mut self, fees: FeeSink) {
        self.finalize_staged().await;
        fees.shutdown();
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
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(self.durable_tip().map(|tip| tip.height));
    }

    fn publish(&self) {
        self.published.view((), self.tiered.get().applied());
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, PersistenceEngine};
    use zaino_primitives::testing::{linked, Chain};
    use zaino_primitives::types::{Transaction, TransactionId};
    use zaino_sync::BlockSink;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{
        fold::tests::{coinbase, fees, tx},
        schema, FoldError,
    };

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// `/vb` on `fs` at its committed state
    fn open(fs: Arc<SimFs>, batch: NonZeroUsize) -> ValueBalanceIndexWriter<DiskStore> {
        let store = DiskEngine::new(fs).open(Path::new("/vb"), &schema(NetworkType::Regtest));
        ValueBalanceIndexWriter::new(store.expect("open"), batch)
    }

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    /// `A<h>[f]` / `F<h>` / `R` / `S`: a step's position in the stream, data aside
    fn label<T>(step: &Step<T>) -> String {
        match step {
            Step::Apply { height, finalized, .. } => {
                format!("A{height}{}", if *finalized { "f" } else { "" })
            }
            Step::Finalized { height } => format!("F{height}"),
            Step::Reorg => "R".to_owned(),
            Step::Shutdown => "S".to_owned(),
        }
    }

    /// Four final blocks sent one at a time (a 1-byte batch = one commit each), crashed after every
    /// operation: each state reopens to an acknowledged or attempted commit, and the index run on
    /// it resolves the next block's fees against the outputs it recovered and commits it
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_committed_prefix_whose_outputs_still_resolve() {
        // block h spends an output of block h - 1 (and 3 spends one of 1): each fee needs a
        // recovered output
        let chain = linked(vec![
            vec![coinbase(0x10, 100_000)],
            vec![coinbase(0x11, 50_000), tx(0x20, &[(0x10, 0)], &[90_000], [0; 4])],
            vec![coinbase(0x12, 50_000), tx(0x21, &[(0x20, 0)], &[80_000], [0; 4])],
            vec![coinbase(0x13, 50_000), tx(0x22, &[(0x11, 0)], &[40_000], [0; 4])],
            vec![coinbase(0x14, 50_000), tx(0x23, &[(0x21, 0)], &[70_000], [0; 4])],
        ]);
        let paid = vec![None, Some(10_000)];
        let expected = [vec![None], paid.clone(), paid.clone(), paid.clone(), paid];
        let within = Duration::from_secs(5);

        // commits of 0..=3 (4 only ever committed after a recovery); tag = commits acknowledged
        let fs = SimFs::recording();
        let index = open(fs.clone(), NonZeroUsize::MIN);
        let mut durable = index.published().subscribe_finalized();
        let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let blocks = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(blocks, fee_sink));
        for (acked, block) in (1u64..).zip(&chain[..4]) {
            let height = block.header().height;
            block_sink.send(Step::Apply { height, finalized: true, data: Arc::clone(block) }).await;
            let landed = tokio::time::timeout(within, durable.wait_for(|at| *at == Some(height)));
            landed.await.expect("each block commits").expect("index alive");
            fs.set_tag(acked);
        }
        block_sink.shutdown();
        running.await.expect("clean stop");
        for (height, expected) in (0u32..).zip(&expected[..4]) {
            let Step::Apply { data, .. } = consumer.next().await else {
                panic!("fees of block {height} expected");
            };
            assert_eq!(fees(&data), *expected, "fees of block {height}");
        }
        assert!(matches!(consumer.next().await, Step::Shutdown), "Shutdown after block 3's fees");
        let tip_after = |commits: u64| (commits > 0).then(|| h((commits - 1).min(3) as u32));

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let crashed = &state.label;
            let index = open(state.fs, QUEUE);
            let tip = index.durable_tip().map(|tip| tip.height);
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{crashed}: recovered through {tip:?}");

            let next = tip.map_or(0, |tip| u32::from(tip) + 1);
            let durable = index.published().subscribe_finalized();
            let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
            let mut consumer = fee_sink.subscribe("consumer", QUEUE);
            let blocks = block_sink.subscribe("value_balance", QUEUE);
            let running = tokio::spawn(index.run(blocks, fee_sink));
            let data = Arc::clone(&chain[next as usize]);
            block_sink.send(Step::Apply { height: h(next), finalized: true, data }).await;
            block_sink.shutdown();
            running
                .await
                .unwrap_or_else(|error| panic!("{crashed}: commit after recovery: {error}"));

            let resolved = match consumer.next().await {
                Step::Apply { data, .. } => fees(&data),
                other => panic!("{crashed}: fees of block {next} expected, got {}", label(&other)),
            };
            assert_eq!(resolved, expected[next as usize], "{crashed}: fees of block {next}");
            assert!(
                matches!(consumer.next().await, Step::Shutdown),
                "{crashed}: one block, then stop"
            );
            assert_eq!(*durable.borrow(), Some(h(next)), "{crashed}: written, landed");
        }
    }

    /// Bulk sync whose commit lands inside a run: 0 alone (its bytes carried, under the batch, its
    /// height published as merged),
    /// then 1 to 3 queued as one run whose batch fills at 2. 3 is still final after that commit,
    /// staged and committed at `Shutdown`, and every block's fees go out in order
    #[tokio::test]
    async fn a_commit_inside_a_run_keeps_the_rest_of_the_run_final() {
        let chain = linked(vec![
            vec![coinbase(0x10, 100_000)],
            vec![coinbase(0x11, 50_000), tx(0x20, &[(0x10, 0)], &[90_000], [0; 4])],
            vec![coinbase(0x12, 50_000), tx(0x21, &[(0x20, 0)], &[80_000], [0; 4])],
            vec![coinbase(0x13, 50_000), tx(0x22, &[(0x21, 0)], &[70_000], [0; 4])],
        ]);
        let batch = chain[..3].iter().map(|block| block.weight()).sum::<usize>();
        let index = open(SimFs::new(), NonZeroUsize::new(batch).expect("blocks weigh something"));
        let durable = index.published().subscribe_finalized();
        let mut merged = index.published().subscribe_merged();
        let (mut block_sink, mut fee_sink) = (BlockSink::new("blocks"), FeeSink::new("fees"));
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let blocks = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(blocks, fee_sink));

        let first = Arc::clone(&chain[0]);
        block_sink.send(Step::Apply { height: h(0), finalized: true, data: first }).await;
        assert_eq!(label(&consumer.next().await), "A0f", "0 handled as a run of its own");
        let held = merged.wait_for(|at| *at == Some(h(0)));
        let held = tokio::time::timeout(Duration::from_secs(5), held).await;
        held.expect("0 merged for the next bulk commit").expect("index alive");
        assert_eq!(*durable.borrow(), None, "0's bytes under the batch: carried, not committed");
        for block in &chain[1..] {
            let (height, data) = (block.header().height, Arc::clone(block));
            block_sink.send(Step::Apply { height, finalized: true, data }).await;
        }
        block_sink.shutdown();
        running.await.expect("clean stop");

        let mut steps = Vec::new();
        loop {
            let step = consumer.next().await;
            steps.push(label(&step));
            if matches!(step, Step::Shutdown) {
                break;
            }
        }
        assert_eq!(steps, ["A1f", "A2f", "A3f", "S"]);
        assert_eq!(*durable.borrow(), Some(h(3)), "2 committed mid-run, 3 at shutdown");
        assert_eq!(*merged.borrow(), None, "nothing held once a commit covers it");
    }

    /// Tip 4 (0 to 2 final, both inclusive; 3, 4 non-finalized): every prevout resolves wherever
    /// it lives
    /// - 1: spends 0's output (durable) and one from earlier in its own block (staged)
    /// - 3: spends 1's outputs (durable) with value leaving sprout
    /// - 4: spends 3's output (non-finalized) and enters ironwood
    ///
    /// Restart with a downstream index durable at 0 (BlockSink starts at 1): 1 to 2 (both
    /// inclusive) replay through durable storage alone, 3 onward recorded again, and every item is
    /// identical; the fee stream = the block stream step for step (`Shutdown` last)
    ///
    /// Both batch sizes: 1 byte = one block per run; 1 MiB = the queued chain as one run (1's
    /// spend of 0's output then resolves inside the run, not from a committed segment)
    #[tokio::test]
    #[rustfmt::skip]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_a_replay_republishes_them() {
        //      tx(tag,  spends (tag, vout),      outputs,           shielded balances
        let chain = linked(vec![
            vec![
                coinbase(0x10, 100_000),
            ],
            vec![
                coinbase(0x11, 625_000_000),
                tx(0x21, &[(0x10, 0)],            &[60_000, 39_000], [0; 4]),
                tx(0x22, &[(0x21, 1)],            &[30_000],         [0, -8_000, 0, 0]),
            ],
            vec![
                coinbase(0x12, 625_000_000),
                tx(0x23, &[(0x21, 0)],            &[],               [0, 0, -59_000, 0]),
            ],
            vec![
                coinbase(0x13, 625_000_000),
                tx(0x24, &[(0x22, 0), (0x11, 0)], &[625_029_500],    [500, 0, 0, 0]),
            ],
            vec![
                coinbase(0x14, 625_000_000),
                tx(0x25, &[(0x24, 0)],            &[625_000_000],    [0, 0, 0, -29_000]),
            ],
        ]);
        // per block, per tx (None = coinbase)
        let expected_fees = [
            vec![None],
            vec![None, Some(1_000), Some(1_000)],
            vec![None, Some(1_000)],
            vec![None, Some(1_000)],
            vec![None, Some(500)],
        ];

        for batch in [NonZeroUsize::MIN, QUEUE] {
            let fs = SimFs::new();
            let mut first_boot = Vec::new();
            for (boot, downstream) in [("first", None), ("restart", Some(h(0)))] {
                let index = open(fs.clone(), batch);
                let durable = index.durable_tip().map(|tip| tip.height);
                let finalized = index.published().subscribe_finalized();
                let mut block_sink = BlockSink::new("blocks");
                let mut fee_sink = FeeSink::new("fees");
                let mut consumer = fee_sink.subscribe("consumer", QUEUE);
                let subscription = block_sink.subscribe("value_balance", QUEUE);
                let mut blocks = block_sink.subscribe("downstream", QUEUE);
                let running = tokio::spawn(index.run(subscription, fee_sink));

                // tip 4, depth 2: final through 2; from after the rearmost durable tip
                let start = durable.min(downstream).map_or(0, |tip| u32::from(tip) as usize + 1);
                for block in &chain[start..] {
                    let height = block.header().height;
                    let (finalized, data) = (height <= h(2), Arc::clone(block));
                    block_sink.send(Step::Apply { height, finalized, data }).await;
                }
                block_sink.shutdown();
                running.await.expect("clean stop");

                let (mut block_steps, mut fee_steps, mut published) = (vec![], vec![], vec![]);
                loop {
                    let step = blocks.next().await;
                    block_steps.push(label(&step));
                    if matches!(step, Step::Shutdown) {
                        break;
                    }
                }
                loop {
                    let step = consumer.next().await;
                    fee_steps.push(label(&step));
                    match step {
                        Step::Apply { data, .. } => published.push(data),
                        Step::Shutdown => break,
                        Step::Finalized { .. } | Step::Reorg => {}
                    }
                }
                let context = format!("{boot}, batch {batch}");
                assert_eq!(fee_steps, block_steps, "{context}: fees mirror the block stream");
                assert_eq!(*finalized.borrow(), Some(h(2)), "{context}: final durable, rest not");

                match boot {
                    "first" => {
                        assert_eq!(durable, None, "fresh directory");
                        let steps = ["A0f", "A1f", "A2f", "A3", "A4", "S"];
                        assert_eq!(block_steps, steps, "{context}");
                        let published_fees: Vec<_> = published.iter().map(|b| fees(b)).collect();
                        assert_eq!(published_fees, expected_fees, "{context}: fees per tx");
                        first_boot = published;
                    }
                    _ => {
                        assert_eq!(durable, Some(h(2)), "0 to 2 (both inclusive) committed");
                        assert_eq!(block_steps, ["A1f", "A2f", "A3", "A4", "S"], "{context}");
                        assert_eq!(published, first_boot[1..], "{context}: replay = same fees");
                    }
                }
            }
        }
    }

    /// Tip 3 on fork 0 (2, 3 non-finalized), then fork 1 wins from 2 (its 3 spends an output only
    /// its own 2 created): the fee stream carries the `Reorg` where the block stream did, then
    /// fork 1's fees, resolved against fork 1's outputs
    #[tokio::test]
    async fn a_reorg_drops_the_losing_branch_outputs_and_republishes_the_winner() {
        let mut chain = Chain::with_genesis(vec![coinbase(0x10, 100_000)]);
        let one = chain.mine_with(chain.genesis().hash, vec![coinbase(0x11, 625_000_000)]);
        // both branches fork from `one`: each mines `(2, 3)` from its own transactions
        let mut branch = |txs: [Vec<Transaction>; 2]| {
            let [two, three] = txs;
            let two = chain.mine_with(one.hash, two);
            let three = chain.mine_with(two.hash, three);
            chain.path(three.hash).into_iter().map(Arc::new).collect::<Vec<_>>()
        };
        let losing = branch([
            vec![coinbase(0x12, 625_000_000), tx(0x20, &[(0x10, 0)], &[99_000], [0; 4])],
            vec![coinbase(0x13, 625_000_000), tx(0x30, &[(0x20, 0)], &[98_000], [0; 4])],
        ]);
        let winning = branch([
            vec![coinbase(0x42, 625_000_000), tx(0x60, &[(0x10, 0)], &[90_000], [0; 4])],
            vec![coinbase(0x43, 625_000_000), tx(0x70, &[(0x60, 0)], &[80_000], [0; 4])],
        ]);

        let fs = SimFs::new();
        let index = open(fs, NonZeroUsize::MIN);
        let mut block_sink = BlockSink::new("blocks");
        let mut fee_sink = FeeSink::new("fees");
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let subscription = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(subscription, fee_sink));

        // tip 3, depth 2: 0 and 1 final, 2 and 3 not; reorg → 2 and 3 again, from fork 1
        let apply = |block: &Arc<Block>| {
            let height = block.header().height;
            Step::Apply { height, finalized: height <= h(1), data: Arc::clone(block) }
        };
        for block in &losing {
            block_sink.send(apply(block)).await;
        }
        block_sink.send(Step::Reorg).await;
        for block in &winning[2..] {
            block_sink.send(apply(block)).await;
        }
        block_sink.shutdown();
        running.await.expect("clean stop");

        let mut published = Vec::new();
        loop {
            let step = consumer.next().await;
            let seen = match &step {
                Step::Apply { data, .. } => format!("{} {:?}", label(&step), fees(data)),
                _ => label(&step),
            };
            published.push(seen);
            if matches!(step, Step::Shutdown) {
                break;
            }
        }
        assert_eq!(
            published,
            [
                "A0f [None]",
                "A1f [None]",
                "A2 [None, Some(1000)]",
                "A3 [None, Some(1000)]",
                "R",
                "A2 [None, Some(10000)]",
                "A3 [None, Some(10000)]",
                "S",
            ],
            "losing branch, the reorg where the block stream had it, then the winner's own fees"
        );
    }

    /// Fold error (each kind: `fold::tests`) = the index panics, named; the downstream consumer
    /// never sees `Shutdown` (its sink dropped → it panics too)
    #[tokio::test]
    async fn a_fold_error_panics_the_index_and_its_consumer() {
        let index = open(SimFs::new(), NonZeroUsize::MIN);
        let mut block_sink = BlockSink::new("blocks");
        let mut fee_sink = FeeSink::new("fees");
        let mut consumer = fee_sink.subscribe("consumer", QUEUE);
        let subscription = block_sink.subscribe("value_balance", QUEUE);
        let running = tokio::spawn(index.run(subscription, fee_sink));
        let downstream = tokio::spawn(async move { consumer.next().await });

        let unrecorded = tx(0x20, &[(0x99, 3)], &[1], [0; 4]);
        let data = Arc::clone(&linked(vec![vec![coinbase(0x10, 100_000), unrecorded]])[0]);
        block_sink.send(Step::Apply { height: h(0), finalized: false, data }).await;

        let message = |joined: Result<_, tokio::task::JoinError>| {
            let payload = joined.expect_err("panicked").into_panic();
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_owned()))
        };
        let id = |byte| TransactionId::from([byte; 32]);
        let expected =
            FoldError::MissingPrevout { height: h(0), txid: id(0x20), spent: id(0x99), vout: 3 };
        assert_eq!(message(running.await), Some(format!("value_balance index: {expected}")));
        let consumer = message(downstream.await.map(drop));
        assert_eq!(consumer.as_deref(), Some("sink dropped without Shutdown"));
    }
}
