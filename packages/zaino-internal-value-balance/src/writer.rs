//! value_balance writer: the final stream → one [`fold_run`] per run of unfolded steps → its store
//!
//! - one [`BlockFees`] per unfolded step into the [`FeeSink`], held heights re-folded (insert
//!   only: any later state resolves them the same; compact-block may be behind this index)
//! - folded steps: nothing on the sink (the NFS folded compact-block with their fees)

use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::watch;
use zaino_persistence::{MapRead, Store};
use zaino_primitives::types::BlockFees;
use zaino_sync::{held, Committer, FeeSink, Final, Step, Subscription};

use crate::{fold::fold_run, ValueBalanceReader};

const NAME: &str = zaino_persistence::IndexKind::ValueBalance.name();

/// Records every transparent output and derives one [`BlockFees`] per bulk block
pub struct ValueBalanceIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: MapRead>> ValueBalanceIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = buffered bytes per
    /// bulk commit (one fsync), and one run's stream bytes
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        Self { store: Committer::new(store, batch_bytes) }
    }

    /// For `Nfs::subscribe`: the committed view after every commit
    pub fn committed(&self) -> watch::Receiver<S::View> {
        self.store.committed()
    }

    /// Follows `blocks` through `Shutdown`, then ends `fees`
    ///
    /// - a failure panics (dropped `fees` = no `Shutdown`: compact-block panics too)
    pub async fn run(mut self, mut blocks: Subscription<Final>, fees: FeeSink) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let paid = self.store.compute(move |store| {
                let parent = ValueBalanceReader::new(store.staged(), store.schema().network);
                let unfolded = run.unfolded.iter().map(|(_, block)| &**block);
                let folded = fold_run(&parent, unfolded);
                let folded = folded.unwrap_or_else(|error| panic!("{NAME} index: {error}"));
                let mut paid: Vec<BlockFees> = Vec::with_capacity(folded.len());
                for ((height, _), (changes, block_fees)) in run.unfolded.iter().zip(folded) {
                    if !held(store, *height) {
                        store.apply(changes);
                    }
                    paid.push(block_fees);
                }
                run.apply_folded(store);
                paid
            });
            for block_fees in paid.await {
                fees.send(Step::Apply { height: block_fees.height, data: Arc::new(block_fees) })
                    .await;
            }
        }
        fees.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, View,
    };
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{Block, Height, TransactionId};
    use zaino_sync::{Folds, IndexerDataSink, Subscription};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{
        fold,
        fold::tests::{coinbase, fees, tx},
        schema, FoldError,
    };

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        let store =
            DiskEngine::new(fs.clone()).open(Path::new("/vb"), &schema(NetworkType::Regtest));
        store.expect("open")
    }

    /// The writer over `store`: its final stream, committed view and fee stream
    fn start(
        store: DiskStore,
        batch: NonZeroUsize,
    ) -> (
        IndexerDataSink<Final>,
        watch::Receiver<DiskView>,
        Subscription<BlockFees>,
        tokio::task::JoinHandle<()>,
    ) {
        let writer = ValueBalanceIndexWriter::new(store, batch);
        let committed = writer.committed();
        let (mut sink, mut fee_sink) = (IndexerDataSink::new("final"), FeeSink::new("fees"));
        let consumer = fee_sink.subscribe("consumer", QUEUE);
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE), fee_sink));
        (sink, committed, consumer, running)
    }

    fn step(block: &Block, folds: Option<Arc<Folds>>) -> Step<Final> {
        let (height, block) = (block.header().height, Arc::new(block.clone()));
        Step::Apply { height, data: Arc::new(Final { block, folds }) }
    }

    /// Every fee step through `Shutdown`: `(height, fees per tx)`
    async fn drained(consumer: &mut Subscription<BlockFees>) -> Vec<(u32, Vec<Option<u64>>)> {
        let mut out = Vec::new();
        while let Step::Apply { height, data } = consumer.next().await {
            out.push((u32::from(height), fees(&data)));
        }
        out
    }

    /// Four bulk blocks, each its own commit (batch = 1 byte), crashed after every operation:
    /// each state reopens to an acknowledged or attempted commit, and the writer on it resolves
    /// the next block's fees against the outputs it recovered and commits it
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
        let tip_of = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height));

        // commits of 0..=3 (4 only ever committed after a recovery); tag = commits acknowledged
        let fs = SimFs::recording();
        let (sink, mut committed, mut consumer, running) = start(open(&fs), NonZeroUsize::MIN);
        for (acked, block) in (1u64..).zip(&chain[..4]) {
            sink.send(step(block, None)).await;
            let height = Some(u32::from(block.header().height));
            committed.wait_for(|view| tip_of(view) == height).await.expect("writer alive");
            fs.set_tag(acked);
        }
        sink.shutdown();
        running.await.expect("clean stop");
        let first: Vec<_> = (0..4).zip(expected[..4].iter().cloned()).collect();
        assert_eq!(drained(&mut consumer).await, first, "one fee step per block, Shutdown last");
        let tip_after = |commits: u64| (commits > 0).then(|| (commits - 1).min(3) as u32);

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let crashed = &state.label;
            let store = open(&state.fs);
            let tip = tip_of(&store.view());
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{crashed}: recovered through {tip:?}");

            let next = tip.map_or(0, |tip| tip + 1);
            let (sink, committed, mut consumer, running) = start(store, QUEUE);
            sink.send(step(&chain[next as usize], None)).await;
            sink.shutdown();
            running
                .await
                .unwrap_or_else(|error| panic!("{crashed}: commit after recovery: {error}"));
            let resolved = drained(&mut consumer).await;
            assert_eq!(resolved, [(next, expected[next as usize].clone())], "{crashed}");
            assert_eq!(tip_of(&committed.borrow()), Some(next), "{crashed}: committed");
        }
    }

    /// Every prevout resolves wherever it lives (committed, buffered, earlier in its own run)
    /// - 1: spends 0's output and one from earlier in its own block
    /// - 3: spends 1's outputs with value leaving sprout; 4: spends 3's and enters ironwood
    ///
    /// Boot 1: 0..=2 unfolded (fees out), 3, 4 folded (no fees: the NFS folded compact-block).
    /// Boot 2, compact-block durable at 0: 1..=4 resent unfolded, all held: re-folded, fees out
    /// again, identical. Both batch sizes: 1 byte = one block per run, 1 MiB = one run
    #[tokio::test(start_paused = true)]
    #[rustfmt::skip]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_held_heights_republish_them() {
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
        let expected: Vec<(u32, Vec<Option<u64>>)> = (0..).zip([
            vec![None],
            vec![None, Some(1_000), Some(1_000)],
            vec![None, Some(1_000)],
            vec![None, Some(1_000)],
            vec![None, Some(500)],
        ]).collect();
        // 3 and 4 as the NFS folds them: onto everything below them
        let mut scratch = open(&SimFs::new());
        let mut folded = Vec::new();
        for block in chain.iter() {
            let parent = ValueBalanceReader::new(scratch.staged(), NetworkType::Regtest);
            let (changes, _) = fold(&parent, block).expect("every prevout held");
            let mut folds = Folds::default();
            folds.insert(IndexKind::ValueBalance, changes.clone());
            folded.push(Arc::new(folds));
            scratch.apply(changes);
        }

        for batch in [NonZeroUsize::MIN, QUEUE] {
            let fs = SimFs::new();
            let (sink, mut committed, mut consumer, running) = start(open(&fs), batch);
            for block in &chain[..3] {
                sink.send(step(block, None)).await;
            }
            for (block, folds) in chain[3..].iter().zip(&folded[3..]) {
                sink.send(step(block, Some(Arc::clone(folds)))).await;
            }
            let four = Some(Height::try_from(4u32).expect("h"));
            committed.wait_for(|view| view.tip().map(|tip| tip.height) == four).await.expect("alive");
            sink.shutdown();
            running.await.expect("clean stop");
            assert_eq!(drained(&mut consumer).await, expected[..3], "batch {batch}: unfolded only");

            let (sink, committed, mut consumer, running) = start(open(&fs), batch);
            for block in &chain[1..] {
                sink.send(step(block, None)).await;
            }
            sink.shutdown();
            running.await.expect("clean stop");
            assert_eq!(drained(&mut consumer).await, expected[1..], "batch {batch}: republished");
            assert_eq!(committed.borrow().tip().map(|tip| tip.height), four, "held: nothing new");
        }
    }

    /// Fold error (each kind: `fold::tests`) = the writer panics, named; the fee consumer never
    /// sees `Shutdown` (its sink dropped → it panics too)
    #[tokio::test]
    async fn a_fold_error_panics_the_writer_and_its_consumer() {
        let (sink, _committed, mut consumer, running) = start(open(&SimFs::new()), QUEUE);
        let downstream = tokio::spawn(async move { consumer.next().await });
        let unrecorded = tx(0x20, &[(0x99, 3)], &[1], [0; 4]);
        let chain = linked(vec![vec![coinbase(0x10, 100_000), unrecorded]]);
        sink.send(step(&chain[0], None)).await;

        let message = |joined: Result<_, tokio::task::JoinError>| {
            let payload = joined.expect_err("panicked").into_panic();
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_owned()))
        };
        let id = |byte| TransactionId::from([byte; 32]);
        let h0 = Height::GENESIS;
        let expected =
            FoldError::MissingPrevout { height: h0, txid: id(0x20), spent: id(0x99), vout: 3 };
        assert_eq!(message(running.await), Some(format!("value_balance index: {expected}")));
        let consumer = message(downstream.await.map(drop));
        assert_eq!(consumer.as_deref(), Some("sink dropped without Shutdown"));
    }
}
