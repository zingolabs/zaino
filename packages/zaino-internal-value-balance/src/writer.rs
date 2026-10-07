//! value_balance writer: the final stream → one [`fold_run`] per run of unfolded steps → its store
//!
//! - one [`BlockFees`] per unfolded step into the [`FeeSink`], held heights re-folded (insert
//!   only: any later state resolves them the same; compact-block may be behind this index)
//! - folded steps: nothing on the sink (the NFS folded compact-block with their fees)

use std::{
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    slice,
    sync::Arc,
};

use tokio::sync::watch;
use zaino_persistence::{Changes, IndexKind, MapRead, Store};
use zaino_primitives::types::{
    Block, BlockFees, Fee, Height, OutPoint, OutputIndex, Transaction, TransactionId, Zatoshis,
};
use zaino_sync::{held, Committer, FeeSink, Final, Step, Subscription};

use crate::{encode_value, ValueBalanceReader, OUTPUTS};

const NAME: &str = IndexKind::ValueBalance.name();

/// Records every transparent output and derives one [`BlockFees`] per bulk block
pub struct ValueBalanceIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: MapRead>> ValueBalanceIndexWriter<S> {
    /// Over `store` (opened with [`TABLES`](crate::TABLES)); `batch_bytes` = buffered bytes per
    /// bulk commit (one fsync), and one run's stream bytes
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        Self { store: Committer::new(store, batch_bytes) }
    }

    /// For `Nfs::subscribe`: the committed view after every commit
    pub fn committed(&self) -> watch::Receiver<S::View> {
        self.store.committed()
    }

    /// Follows `blocks` through `Shutdown`, then ends `sink`
    ///
    /// - a failure panics (dropped `sink` = no `Shutdown`: compact-block panics too)
    pub async fn run(mut self, mut blocks: Subscription<Final>, sink: FeeSink) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let paid = self.store.compute(move |store| {
                let resent = run.unfolded.iter().filter(|(height, _)| held(store, *height));
                let resent: Vec<&Block> = resent.map(|(_, block)| &**block).collect();
                let refolded = fees(&ValueBalanceReader::new(store.staged()), &resent);
                let mut paid = refolded.unwrap_or_else(|error| panic!("{NAME} index: {error}"));
                let fresh = run.apply_batch(store, |store, blocks, out| {
                    let folded = fold_run(&ValueBalanceReader::new(store.staged()), blocks, out);
                    folded.unwrap_or_else(|error| panic!("{NAME} index: {error}"))
                });
                paid.extend(fresh);
                paid
            });
            for block_fees in paid.await {
                sink.send(Step::Apply { height: block_fees.height, data: Arc::new(block_fees) })
                    .await;
            }
        }
        sink.shutdown();
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    /// Spent outpoint this index never recorded (the chain is indexed from genesis: a bug or a
    /// foreign directory, never a gap to tolerate)
    #[error(
        "block {height} tx {txid}: spends {spent}:{vout}, an output this index never recorded"
    )]
    MissingPrevout { height: Height, txid: TransactionId, spent: TransactionId, vout: OutputIndex },

    #[error("block {height} tx {txid}: value sums past the money supply")]
    ValueOverflow { height: Height, txid: TransactionId },

    #[error("block {height} tx {txid}: takes more from the transparent pool than it puts in")]
    NegativeFee { height: Height, txid: TransactionId },
}

/// `block` onto `parent`: its outputs' rows, its fees
pub fn fold<V: MapRead>(
    parent: &ValueBalanceReader<V>,
    block: &Block,
    out: &mut Changes,
) -> Result<BlockFees, FoldError> {
    let mut fees = fold_run(parent, &[block], slice::from_mut(out))?;
    Ok(fees.pop().expect("one block in, one folded out"))
}

/// [`fold`] of each of `blocks` (oldest first) onto `parent` + the run's earlier blocks, block `i`
/// into `out[i]`
///
/// - prevouts from outside the run: one probe for the whole run (a sandblast tx spends thousands)
pub(crate) fn fold_run<V: MapRead>(
    parent: &ValueBalanceReader<V>,
    blocks: &[&Block],
    out: &mut [Changes],
) -> Result<Vec<BlockFees>, FoldError> {
    Changes::assert_run(parent.view().tip(), blocks, out);
    run_fees(parent, blocks, |at, outpoint, value| {
        out[at].map(OUTPUTS).insert(&outpoint.encode(), &encode_value(value));
    })
}

/// `fold_run`'s fees alone, no rows: `parent` = any state at or past the first block's parent
/// (insert only: a later state, even one holding `blocks`, resolves them the same)
pub fn fees<V: MapRead>(
    parent: &ValueBalanceReader<V>,
    blocks: &[&Block],
) -> Result<Vec<BlockFees>, FoldError> {
    run_fees(parent, blocks, |_, _, _| {})
}

/// Each of `blocks`' fees, prevouts resolved through the run's earlier outputs, else `parent` (one
/// probe); `output` sees each output as `(block index, outpoint, value)`
fn run_fees<V: MapRead>(
    parent: &ValueBalanceReader<V>,
    blocks: &[&Block],
    mut output: impl FnMut(usize, OutPoint, Zatoshis),
) -> Result<Vec<BlockFees>, FoldError> {
    let in_run: HashSet<OutPoint> =
        blocks.iter().flat_map(|block| outputs(block)).map(|(at, _)| at).collect();
    let asked: Vec<OutPoint> = blocks
        .iter()
        .flat_map(|block| block.transactions())
        .flat_map(|tx| tx.transparent.inputs.iter().copied())
        .filter(|prevout| !in_run.contains(prevout))
        .collect();
    let found = asked.iter().zip(parent.values(&asked));
    let mut known: HashMap<OutPoint, Zatoshis> =
        found.filter_map(|(at, value)| Some((*at, value?))).collect();

    let mut folded = Vec::with_capacity(blocks.len());
    for (at, block) in blocks.iter().enumerate() {
        let header = block.header();
        for (outpoint, value) in outputs(block) {
            output(at, outpoint, value);
            known.insert(outpoint, value);
        }
        let fees = block.transactions().iter().map(|tx| fee(header.height, tx, &known));
        let fees = fees.collect::<Result<_, _>>()?;
        folded.push(BlockFees { height: header.height, hash: header.hash, fees });
    }
    Ok(folded)
}

/// Every transparent output `block` creates, in block order
fn outputs(block: &Block) -> impl Iterator<Item = (OutPoint, Zatoshis)> + '_ {
    block.transactions().iter().flat_map(|tx| {
        let outputs = (0..).zip(&tx.transparent.outputs);
        outputs.map(|(vout, output)| (OutPoint { txid: tx.txid, vout }, output.value))
    })
}

/// `tx`'s value left in the transparent transaction value pool (protocol.pdf#transactions §3.4)
///
/// - Σ transparent inputs − Σ transparent outputs + each shielded pool's value balance
fn fee(
    height: Height,
    tx: &Transaction,
    known: &HashMap<OutPoint, Zatoshis>,
) -> Result<Fee, FoldError> {
    if tx.transparent.coinbase {
        return Ok(Fee::Coinbase);
    }
    let overflow = || FoldError::ValueOverflow { height, txid: tx.txid };
    let spent = tx.transparent.inputs.iter().try_fold(Zatoshis::ZERO, |spent, prevout| {
        let value = known.get(prevout).ok_or(FoldError::MissingPrevout {
            height,
            txid: tx.txid,
            spent: prevout.txid,
            vout: prevout.vout,
        })?;
        spent.checked_add(*value).ok_or_else(overflow)
    })?;
    let paid = Zatoshis::sum_balances(tx.transparent.outputs.iter().map(|out| out.value))
        .ok_or_else(overflow)?;

    // every term within ±MAX_MONEY (zip-0209) → Σ of six fits i64
    let remaining = spent.as_i64() - paid.as_i64()
        + i64::from(tx.sprout.value_balance)
        + i64::from(tx.sapling.value_balance)
        + i64::from(tx.orchard.value_balance)
        + i64::from(tx.ironwood.value_balance);
    // MUST be nonnegative (protocol.pdf#transactions §3.4 consensus rule)
    let remaining =
        u64::try_from(remaining).map_err(|_| FoldError::NegativeFee { height, txid: tx.txid })?;
    Ok(Fee::Paid(Zatoshis::new(remaining).map_err(|_| overflow())?))
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        path::Path,
    };

    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine, Schema, View,
    };
    use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain};
    use zaino_primitives::types::{BlockRef, ShieldedPool, SignedZatoshis};
    use zaino_sync::{Folds, IndexerDataSink, Subscription};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{FORMAT, TABLES};

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const SCHEMA: Schema =
        Schema::new(IndexKind::ValueBalance, FORMAT, NetworkType::Regtest, TABLES);

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        DiskEngine::new(fs.clone()).open(Path::new("/vb"), &SCHEMA).expect("open")
    }

    /// Block 0 folded onto nothing, then 1 and 2 as one run onto 0: each prevout resolves from
    /// the parent, its own block or an earlier run block; fees = the stated ones; rows = golden
    /// bytes; the run = the same blocks folded one at a time = their fees re-folded once held
    #[test]
    fn fees_resolve_through_the_parent_and_the_run_and_a_run_folds_like_single_blocks() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x11; 32]).pay(&alice, 50_000))
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
        let two = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x12; 32]).pay(&alice, 50_000)).tx(|t| {
                t.txid([0x23; 32])
                    .spend(outpoint([0x21; 32], 0))
                    .value_balance(ShieldedPool::Orchard, -59_000)
                    .fee(1_000)
            })
        });
        let blocks = chain.blocks(two);
        let expected = [chain.fees(blocks[1].header().hash), chain.fees(blocks[2].header().hash)];
        let mut store = open(&SimFs::new());
        let mut genesis = store.changes(blocks[0].at());
        let genesis_fees = fold(&ValueBalanceReader::new(store.staged()), &blocks[0], &mut genesis);
        assert_eq!(genesis_fees.expect("coinbase only").fees, [Fee::Coinbase]);
        store.apply(genesis);

        let run = [&*blocks[1], &*blocks[2]];
        let mut out: Vec<Changes> = run.iter().map(|block| store.changes(block.at())).collect();
        let parent = ValueBalanceReader::new(store.staged());
        let run_fees = fold_run(&parent, &run, &mut out).expect("every prevout held");
        assert_eq!(run_fees, expected);
        let rows: Vec<(&[u8], &[u8])> = out[0].inserts(OUTPUTS).collect();
        let row = |tag: u8, vout: u8, value: [u8; 8]| {
            ([[tag; 32].as_slice(), &[0, 0, 0, vout]].concat(), value)
        };
        let golden = [
            row(0x11, 0, [0, 0, 0, 0, 0, 0, 0xc3, 0x50]),
            row(0x21, 0, [0, 0, 0, 0, 0, 0, 0xea, 0x60]),
            row(0x21, 1, [0, 0, 0, 0, 0, 0, 0x98, 0x58]),
            row(0x22, 0, [0, 0, 0, 0, 0, 0, 0x75, 0x30]),
        ];
        let golden: Vec<(&[u8], &[u8])> =
            golden.iter().map(|(key, value)| (&key[..], &value[..])).collect();
        assert_eq!(rows, golden, "block 1: txid ‖ vout BE → value BE, block order");

        for ((block, run_changes), run_fees) in run.iter().zip(&out).zip(&run_fees) {
            let mut changes = store.changes(block.at());
            let block_fees = fold(&ValueBalanceReader::new(store.staged()), block, &mut changes);
            let height = block.header().height;
            assert_eq!(&block_fees.expect("held"), run_fees, "{height:?}");
            let rows = |changes: &Changes| {
                let rows = changes.inserts(OUTPUTS).map(|(key, value)| [key, value].concat());
                rows.collect::<Vec<_>>()
            };
            assert_eq!(rows(&changes), rows(run_changes), "{height:?}");
            store.apply(changes);
        }
        let refolded = fees(&ValueBalanceReader::new(store.staged()), &run);
        assert_eq!(refolded, Ok(expected.to_vec()), "held: same fees off a later parent");
    }

    /// Every rejection names its block and tx; a run never lets a block spend a later one's
    /// output; a run off the parent tip panics, naming the index
    ///
    /// - lies (one field of a mined tx edited): a prevout never mined, a prevout three mints,
    ///   outputs past the inputs, sapling past the inputs
    #[test]
    fn an_unrecorded_prevout_a_forward_spend_or_a_negative_fee_is_a_named_error() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        let two = chain.mine(|b| {
            b.tx(|t| {
                t.txid([0x20; 32]).spend(outpoint([0x10; 32], 0)).pay(&alice, 99_000).fee(1_000)
            })
        });
        let three = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x31; 32]).pay(&alice, 1_000)).tx(|t| {
                t.txid([0x30; 32])
                    .spend(outpoint([0x20; 32], 0))
                    .value_balance(ShieldedPool::Sapling, -99_000)
                    .fee(0)
            })
        });
        let block = |at: BlockRef| Arc::clone(chain.block(at.hash));
        let lie = |at: BlockRef, edit: &dyn Fn(&mut Transaction)| {
            let mut txs = chain.block(at.hash).transactions().to_vec();
            edit(&mut txs[1]);
            Arc::new(Block::new(chain.block(at.hash).header().clone(), txs))
        };
        let spends = |spent: OutPoint| lie(two, &move |tx| tx.transparent.inputs[0] = spent);
        let overpaid = lie(two, &|tx| {
            tx.transparent.outputs[0].value = Zatoshis::new(100_001).expect("in supply");
        });
        let overshielded = lie(three, &|tx| {
            tx.sapling.value_balance = SignedZatoshis::new(-99_001).expect("in supply");
        });
        let genesis = block(chain.genesis());
        let id = |byte| TransactionId::from([byte; 32]);
        let missing = |spent, vout| FoldError::MissingPrevout {
            height: h(2),
            txid: id(0x20),
            spent: id(spent),
            vout,
        };
        let cases = [
            ("never mined", vec![block(one), spends(outpoint([0x99; 32], 3))], missing(0x99, 3)),
            (
                "spends a later run block's output",
                vec![block(one), spends(outpoint([0x31; 32], 0)), block(three)],
                missing(0x31, 0),
            ),
            (
                "transparent outputs > inputs",
                vec![block(one), overpaid],
                FoldError::NegativeFee { height: h(2), txid: id(0x20) },
            ),
            (
                "value into sapling past the inputs",
                vec![block(one), block(two), overshielded],
                FoldError::NegativeFee { height: h(3), txid: id(0x30) },
            ),
        ];

        for (case, blocks, expected) in cases {
            let store = open(&SimFs::new());
            let run: Vec<&Block> = [&genesis].into_iter().chain(&blocks).map(|b| &**b).collect();
            let mut out: Vec<Changes> = run.iter().map(|block| store.changes(block.at())).collect();
            let folded = fold_run(&ValueBalanceReader::new(store.staged()), &run, &mut out);
            assert_eq!(folded.err(), Some(expected), "{case}");
        }

        let (store, two) = (open(&SimFs::new()), block(two));
        let run = [&*genesis, &*two];
        let mut out: Vec<Changes> = run.iter().map(|block| store.changes(block.at())).collect();
        let parent = ValueBalanceReader::new(store.staged());
        let gap = catch_unwind(AssertUnwindSafe(|| fold_run(&parent, &run, &mut out)));
        let payload = gap.expect_err("a run skipping 1");
        let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
        assert!(message.starts_with("value_balance: block"), "{message}");
        assert!(message.contains("does not extend the parent tip"), "{message}");
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
            let mut changes = scratch.changes(block.at());
            let parent = ValueBalanceReader::new(scratch.staged());
            fold(&parent, block, &mut changes).expect("every prevout held");
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
