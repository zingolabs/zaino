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
    use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain};
    use zaino_primitives::types::{Block, ShieldedPool, TransactionId};
    use zaino_sync::{Folds, IndexerDataSink, Subscription};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{fold, schema, FoldError};

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        let store =
            DiskEngine::new(fs.clone()).open(Path::new("/vb"), &schema(NetworkType::Regtest));
        store.expect("open")
    }

    /// Writer over `store`: its final stream, committed view and fee stream
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

    fn step(block: &Arc<Block>, folds: Option<Arc<Folds>>) -> Step<Final> {
        let (height, block) = (block.header().height, Arc::clone(block));
        Step::Apply { height, data: Arc::new(Final { block, folds }) }
    }

    /// Every fee step through `Shutdown`
    async fn drained(consumer: &mut Subscription<BlockFees>) -> Vec<BlockFees> {
        let mut out = Vec::new();
        while let Step::Apply { height, data } = consumer.next().await {
            assert_eq!(height, data.height, "step height = its fees' height");
            out.push(BlockFees::clone(&data));
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
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        // (coinbase txid, txid, spends vout 0 of, pays), each coinbase 50 000
        #[rustfmt::skip]
        let spends = [
            (0x11, 0x20, 0x10, 90_000),
            (0x12, 0x21, 0x20, 80_000),
            (0x13, 0x22, 0x11, 40_000),
            (0x14, 0x23, 0x21, 70_000),
        ];
        for (coinbase, txid, spent, paid) in spends {
            chain.mine(|b| {
                b.coinbase(|c| c.txid([coinbase; 32]).pay(&alice, 50_000)).tx(|t| {
                    t.txid([txid; 32]).spend(outpoint([spent; 32], 0)).pay(&alice, paid).fee(10_000)
                })
            });
        }
        let blocks = chain.blocks(chain.tip());
        let expected: Vec<BlockFees> =
            blocks.iter().map(|block| chain.fees(block.header().hash)).collect();
        let tip_of = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height));

        // commits of 0..=3 (4 only ever committed after a recovery); tag = commits acknowledged
        let fs = SimFs::recording();
        let (sink, mut committed, mut consumer, running) = start(open(&fs), NonZeroUsize::MIN);
        for (acked, block) in (1u64..).zip(&blocks[..4]) {
            sink.send(step(block, None)).await;
            let height = Some(u32::from(block.header().height));
            committed.wait_for(|view| tip_of(view) == height).await.expect("writer alive");
            fs.set_tag(acked);
        }
        sink.shutdown();
        running.await.expect("clean stop");
        let first = &expected[..4];
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
            sink.send(step(&blocks[next as usize], None)).await;
            sink.shutdown();
            running
                .await
                .unwrap_or_else(|error| panic!("{crashed}: commit after recovery: {error}"));
            let resolved = drained(&mut consumer).await;
            assert_eq!(resolved, [expected[next as usize].clone()], "{crashed}");
            assert_eq!(tip_of(&committed.borrow()), Some(next), "{crashed}: committed");
        }
    }

    /// Every prevout resolves wherever it lives (committed, buffered, earlier in its own run)
    /// - 1: spends 0's output and one from earlier in its own block
    /// - 2: enters orchard and sprout; 3: spends 1's outputs with value leaving sprout; 4: spends
    ///   3's and enters ironwood
    ///
    /// - Boot 1: 0..=2 unfolded (fees out), 3, 4 folded (no fees: the NFS folded compact-block)
    /// - Boot 2, compact-block durable at 0: 1..=4 resent unfolded, all held → re-folded, fees
    ///   out again, identical
    /// - Both batch sizes: 1 byte = one block per run, 1 MiB = one run
    #[tokio::test(start_paused = true)]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_held_heights_republish_them() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x11; 32]).pay(&alice, 625_000_000))
                .tx(|t| {
                    t.txid([0x21; 32])
                        .spend(outpoint([0x10; 32], 0))
                        .pay(&alice, 60_000)
                        .pay(&alice, 39_000)
                        .fee(1_000)
                })
                .tx(|t| {
                    t.txid([0x22; 32])
                        .spend(outpoint([0x21; 32], 1))
                        .pay(&alice, 30_000)
                        .value_balance(ShieldedPool::Sapling, -8_000)
                        .fee(1_000)
                })
        });
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x12; 32]).pay(&alice, 625_000_000)).tx(|t| {
                t.txid([0x23; 32])
                    .spend(outpoint([0x21; 32], 0))
                    .value_balance(ShieldedPool::Orchard, -58_500)
                    .sprout_balance(-500)
                    .fee(1_000)
            })
        });
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x13; 32]).pay(&alice, 625_000_000)).tx(|t| {
                t.txid([0x24; 32])
                    .spend(outpoint([0x22; 32], 0))
                    .spend(outpoint([0x11; 32], 0))
                    .pay(&alice, 625_029_500)
                    .sprout_balance(500)
                    .fee(1_000)
            })
        });
        let tip = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x14; 32]).pay(&alice, 625_000_000)).tx(|t| {
                t.txid([0x25; 32])
                    .spend(outpoint([0x24; 32], 0))
                    .pay(&alice, 625_000_000)
                    .value_balance(ShieldedPool::Ironwood, -29_000)
                    .fee(500)
            })
        });
        let blocks = chain.blocks(tip);
        let expected: Vec<BlockFees> =
            blocks.iter().map(|block| chain.fees(block.header().hash)).collect();
        // 3 and 4 as the NFS folds them: onto everything below them
        let mut scratch = open(&SimFs::new());
        let mut folded = Vec::new();
        for block in blocks.iter() {
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
            for block in &blocks[..3] {
                sink.send(step(block, None)).await;
            }
            for (block, folds) in blocks[3..].iter().zip(&folded[3..]) {
                sink.send(step(block, Some(Arc::clone(folds)))).await;
            }
            let four = Some(h(4));
            committed
                .wait_for(|view| view.tip().map(|tip| tip.height) == four)
                .await
                .expect("alive");
            sink.shutdown();
            running.await.expect("clean stop");
            assert_eq!(drained(&mut consumer).await, expected[..3], "batch {batch}: unfolded only");

            let (sink, committed, mut consumer, running) = start(open(&fs), batch);
            for block in &blocks[1..] {
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
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        let one = chain
            .mine(|b| b.tx(|t| t.txid([0x20; 32]).spend(outpoint([0x10; 32], 0)).pay(&alice, 1)));
        // lie: 0x20 spends an output never mined
        let mut txs = chain.block(one.hash).transactions().to_vec();
        txs[1].transparent.inputs[0] = outpoint([0x99; 32], 3);
        let unrecorded = Arc::new(Block::new(chain.block(one.hash).header().clone(), txs));
        sink.send(step(chain.block(chain.genesis().hash), None)).await;
        sink.send(step(&unrecorded, None)).await;

        let message = |joined: Result<_, tokio::task::JoinError>| {
            let payload = joined.expect_err("panicked").into_panic();
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|message| (*message).to_owned()))
        };
        let id = |byte| TransactionId::from([byte; 32]);
        let expected =
            FoldError::MissingPrevout { height: h(1), txid: id(0x20), spent: id(0x99), vout: 3 };
        assert_eq!(message(running.await), Some(format!("value_balance index: {expected}")));
        let consumer = message(downstream.await.map(drop));
        assert_eq!(consumer.as_deref(), Some("sink dropped without Shutdown"));
    }
}
