//! utxo_set writer: the finalized stream → one [`fold_run`] per run → its store
//!
//! - one [`BlockFees`] per step into the [`FeeSink`], held heights' fees read back from `fees`
//!   (compact-block may be behind this index)

use std::{
    collections::{HashMap, HashSet},
    slice,
    sync::Arc,
    time::{Duration, Instant},
};

use zaino_persistence::{BlockChanges, IndexKind, MapRead, SequenceRead, Store};
use zaino_primitives::types::{Block, BlockFees, FeeError};
use zaino_sync::{
    apply, assert_run, blocking, commit, held, ran, FeeSink, IndexHandle, IndexPublisher, Step,
    Subscription,
};
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::OutPoint;

use crate::layout::{encode_fees, encode_outpoint, encode_value};
use crate::{FoldError, UtxoSetReader, FEES, OUTPUTS};

const NAME: &str = IndexKind::UtxoSet.name();

/// Records every transparent output and derives one [`BlockFees`] per bulk block
pub struct UtxoSetIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: SequenceRead + MapRead>> UtxoSetIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema))
    pub fn new(store: S) -> Self {
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    /// For `Nfs::add`: durable view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` through `Shutdown`, then ends `sink`
    ///
    /// - one `fold_run` per run (every block the store lacks: one prevout probe), held blocks'
    ///   fees read back
    /// - commits after the finalized tip + at `Shutdown` (a full buffer commits on its own)
    /// - a failure panics (dropped `sink` = no `Shutdown`: compact-block panics too)
    pub async fn run(self, mut blocks: Subscription<Block>, sink: FeeSink) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            let started = Instant::now();
            let (paid, write);
            (store, paid, write) = blocking(move || {
                let mut write = Duration::ZERO;
                let (resent, fresh): (Vec<&Block>, Vec<&Block>) =
                    run.blocks.iter().map(|block| &**block).partition(|block| held(&store, block));
                let held = UtxoSetReader::new(store.applied());
                let stored = resent.iter().map(|block| held.block_fees(block));
                let stored = stored.collect::<Result<Vec<_>, _>>();
                let mut paid = stored.unwrap_or_else(|error| panic!("{NAME} index: {error}"));
                let mut out: Vec<BlockChanges> =
                    fresh.iter().map(|block| store.changes(block.block_ref())).collect();
                let folded = fold_run(&UtxoSetReader::new(store.applied()), &fresh, &mut out);
                paid.extend(folded.unwrap_or_else(|error| panic!("{NAME} index: {error}")));
                for changes in out {
                    write += apply(&mut store, changes);
                }
                if run.ends_at_finalized_tip {
                    write += commit(&mut store);
                }
                (store, paid, write)
            })
            .await;
            publisher.publish(&store);
            for block_fees in paid {
                sink.send(Step::Apply(Arc::new(block_fees))).await;
            }
            ran(&store, started, write);
        }
        store = blocking(move || {
            commit(&mut store);
            store
        })
        .await;
        publisher.publish(&store);
        sink.shutdown();
    }
}

/// `block` onto `parent`: its rows (outputs it leaves unspent in, prevouts it spends out), its
/// fees record, its fees
pub fn fold<V: SequenceRead + MapRead>(
    parent: &UtxoSetReader<V>,
    block: &Block,
    out: &mut BlockChanges,
) -> Result<BlockFees, FoldError> {
    let mut fees = fold_run(parent, &[block], slice::from_mut(out))?;
    Ok(fees.pop().expect("one block in, one folded out"))
}

/// [`fold`] of each of `blocks` (oldest first) onto `parent` + the run's earlier blocks, block `i`
/// into `out[i]`
///
/// - prevouts from outside the run: one probe for the whole run (a sandblast tx spends thousands)
/// - per tx in chain order: inputs spent out of `unspent`, then its outputs in (a second spend,
///   or a spend of a later tx's output = `MissingPrevout`)
/// - output created + spent in one block = neither row (one delta never inserts and removes a key)
pub(crate) fn fold_run<V: SequenceRead + MapRead>(
    parent: &UtxoSetReader<V>,
    blocks: &[&Block],
    out: &mut [BlockChanges],
) -> Result<Vec<BlockFees>, FoldError> {
    assert_run(parent.view().tip(), blocks, out);
    let in_run: HashSet<OutPoint> =
        blocks.iter().flat_map(|block| outputs(block)).map(|(at, _)| at).collect();
    let asked: Vec<OutPoint> = blocks
        .iter()
        .flat_map(|block| block.transactions())
        .filter(|tx| !tx.transparent.coinbase)
        .flat_map(|tx| tx.transparent.inputs.iter().cloned())
        .filter(|prevout| !in_run.contains(prevout))
        .collect();
    let values = parent.values(&asked);
    let found = asked.into_iter().zip(values);
    let mut unspent: HashMap<OutPoint, Zatoshis> =
        found.filter_map(|(at, value)| Some((at, value?))).collect();

    let mut folded = Vec::with_capacity(blocks.len());
    for (block, out) in blocks.iter().zip(out) {
        let height = block.header().height;
        let mut created_here = HashSet::new();
        let mut fees = Vec::with_capacity(block.transactions().len());
        for tx in block.transactions() {
            let mut spent = Zatoshis::ZERO;
            if !tx.transparent.coinbase {
                for prevout in &tx.transparent.inputs {
                    let missing = FoldError::MissingPrevout {
                        height,
                        txid: tx.txid,
                        spent: *prevout.txid(),
                        vout: prevout.n(),
                    };
                    let value = unspent.remove(prevout).ok_or(missing)?;
                    spent = (spent + value)
                        .ok_or(FoldError::ValueOverflow { height, txid: tx.txid })?;
                    if !created_here.remove(prevout) {
                        out.map(OUTPUTS).remove(&encode_outpoint(prevout));
                    }
                }
            }
            for (vout, output) in (0..).zip(&tx.transparent.outputs) {
                let outpoint = OutPoint::new(tx.txid.into(), vout);
                created_here.insert(outpoint.clone());
                unspent.insert(outpoint, output.value);
            }
            fees.push(tx.fee(spent).map_err(|refused| match refused {
                FeeError::Negative => FoldError::NegativeFee { height, txid: tx.txid },
                FeeError::Overflow => FoldError::ValueOverflow { height, txid: tx.txid },
            })?);
        }
        for (outpoint, value) in outputs(block).filter(|(at, _)| created_here.contains(at)) {
            out.map(OUTPUTS).insert(&encode_outpoint(&outpoint), &encode_value(value));
        }
        out.sequence(FEES).append(&encode_fees(&fees));
        folded.push(BlockFees { height, hash: block.header().hash, fees });
    }
    Ok(folded)
}

/// Every transparent output `block` creates, in block order
fn outputs(block: &Block) -> impl Iterator<Item = (OutPoint, Zatoshis)> + '_ {
    block.transactions().iter().flat_map(|tx| {
        let outputs = (0..).zip(&tx.transparent.outputs);
        outputs.map(|(vout, output)| (OutPoint::new(tx.txid.into(), vout), output.value))
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema, View};
    use zaino_primitives::testing::{h, p2pkh, MockChain};
    use zaino_primitives::types::{BlockRef, Fee, Transaction};
    use zaino_sync::testing::{finalized, open, step, Writer, QUEUE};
    use zcash_protocol::consensus::NetworkType;
    use zcash_protocol::value::ZatBalance;
    use zcash_protocol::{ShieldedPool, TxId};

    use super::*;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    /// Block 0 folded onto nothing, then 1 and 2 as one run onto 0
    ///
    /// - each prevout resolves from the parent, its own block or an earlier run block
    /// - fees = the stated ones
    /// - rows = golden bytes: block 1 inserts what it leaves unspent (0x21:1 created + spent in
    ///   it: neither row), removes genesis's 0x10:0
    /// - block 2 removes the run's 0x21:0
    /// - the run = the same blocks folded one at a time (rows + fees records) = their fees read
    ///   back once held
    #[test]
    fn fees_resolve_through_the_parent_and_the_run_and_a_run_folds_like_single_blocks() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x11; 32]).pay(&alice, 50_000))
                .tx(|t| {
                    t.txid([0x21; 32])
                        .spend(OutPoint::new([0x10; 32], 0))
                        .pay(&alice, 60_000)
                        .pay(&alice, 39_000)
                        .fee(1_000)
                })
                .tx(|t| {
                    t.txid([0x22; 32])
                        .spend(OutPoint::new([0x21; 32], 1))
                        .pay(&alice, 30_000)
                        .value_balance(ShieldedPool::Sapling, -8_000)
                        .fee(1_000)
                })
        });
        let two = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x12; 32]).pay(&alice, 50_000)).tx(|t| {
                t.txid([0x23; 32])
                    .spend(OutPoint::new([0x21; 32], 0))
                    .value_balance(ShieldedPool::Orchard, -59_000)
                    .fee(1_000)
            })
        });
        let blocks = chain.blocks(two);
        let expected = [chain.fees(blocks[1].header().hash), chain.fees(blocks[2].header().hash)];
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let mut genesis = store.changes(blocks[0].block_ref());
        let genesis_fees = fold(&UtxoSetReader::new(store.applied()), &blocks[0], &mut genesis);
        assert_eq!(genesis_fees.expect("coinbase only").fees, [Fee::Coinbase]);
        store.apply(genesis);

        let run = [&*blocks[1], &*blocks[2]];
        let mut out: Vec<BlockChanges> =
            run.iter().map(|block| store.changes(block.block_ref())).collect();
        let parent = UtxoSetReader::new(store.applied());
        let run_fees = fold_run(&parent, &run, &mut out).expect("every prevout held");
        assert_eq!(run_fees, expected);
        let rows: Vec<(&[u8], &[u8])> = out[0].inserts(OUTPUTS).collect();
        let row = |tag: u8, vout: u8, value: [u8; 8]| {
            ([[tag; 32].as_slice(), &[0, 0, 0, vout]].concat(), value)
        };
        let golden = [
            row(0x11, 0, [0x50, 0xc3, 0, 0, 0, 0, 0, 0]),
            row(0x21, 0, [0x60, 0xea, 0, 0, 0, 0, 0, 0]),
            row(0x22, 0, [0x30, 0x75, 0, 0, 0, 0, 0, 0]),
        ];
        let golden: Vec<(&[u8], &[u8])> =
            golden.iter().map(|(key, value)| (&key[..], &value[..])).collect();
        assert_eq!(rows, golden, "block 1: txid ‖ vout BE → value LE, block order");
        let removed = |changes: &BlockChanges| -> Vec<Vec<u8>> {
            changes.removes(OUTPUTS).map(<[u8]>::to_vec).collect()
        };
        let spent = [[[0x10; 32].as_slice(), &[0, 0, 0, 0]].concat()];
        assert_eq!(removed(&out[0]), spent, "block 1 spends genesis's");
        let spent = [[[0x21; 32].as_slice(), &[0, 0, 0, 0]].concat()];
        assert_eq!(removed(&out[1]), spent, "block 2 spends block 1's");

        for ((block, run_changes), run_fees) in run.iter().zip(&out).zip(&run_fees) {
            let mut changes = store.changes(block.block_ref());
            let block_fees = fold(&UtxoSetReader::new(store.applied()), block, &mut changes);
            let height = block.header().height;
            assert_eq!(&block_fees.expect("held"), run_fees, "{height:?}");
            let rows = |changes: &BlockChanges| {
                let inserted = changes.inserts(OUTPUTS).map(|(key, value)| [key, value].concat());
                let fees = changes.appends(FEES).map(<[u8]>::to_vec);
                (inserted.collect::<Vec<_>>(), removed(changes), fees.collect())
            };
            let (single, in_run): (_, (_, _, Vec<_>)) = (rows(&changes), rows(run_changes));
            assert_eq!(single, in_run, "{height:?}: inserts, removes, fees record");
            assert_eq!(in_run.2, [encode_fees(&run_fees.fees)], "{height:?}: one fees record");
            store.apply(changes);
        }
        let held = UtxoSetReader::new(store.applied());
        let stored: Vec<_> = run.iter().map(|block| held.block_fees(block)).collect();
        assert_eq!(stored, expected.map(Ok), "held: fees read back as folded");
    }

    /// Every rejection names its block and tx
    ///
    /// - run never lets a block spend a later one's output, nor an output twice
    /// - lies (one field of a mined tx edited): a prevout never mined, a prevout three mints, one
    ///   prevout twice in a tx, two's prevout again in three, outputs past the inputs, sapling past
    ///   the inputs
    #[test]
    fn an_unrecorded_prevout_a_forward_spend_or_a_negative_fee_is_a_named_error() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        let two = chain.mine(|b| {
            b.tx(|t| {
                t.txid([0x20; 32])
                    .spend(OutPoint::new([0x10; 32], 0))
                    .pay(&alice, 99_000)
                    .fee(1_000)
            })
        });
        let three = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x31; 32]).pay(&alice, 1_000)).tx(|t| {
                t.txid([0x30; 32])
                    .spend(OutPoint::new([0x20; 32], 0))
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
        let spends = |spent: OutPoint| lie(two, &|tx| tx.transparent.inputs[0] = spent.clone());
        let spends_twice = lie(two, &|tx| tx.transparent.inputs.push(OutPoint::new([0x10; 32], 0)));
        let respends = lie(three, &|tx| tx.transparent.inputs.push(OutPoint::new([0x10; 32], 0)));
        let overpaid = lie(two, &|tx| {
            tx.transparent.outputs[0].value = Zatoshis::from_u64(100_001).expect("in supply");
        });
        let overshielded = lie(three, &|tx| {
            tx.sapling.value_balance = ZatBalance::from_i64(-99_001).expect("in supply");
        });
        let genesis = block(chain.genesis());
        let id = |byte| TxId::from_bytes([byte; 32]);
        let missing = |spent, vout| FoldError::MissingPrevout {
            height: h(2),
            txid: id(0x20),
            spent: id(spent),
            vout,
        };
        let cases = [
            (
                "never mined",
                vec![block(one), spends(OutPoint::new([0x99; 32], 3))],
                missing(0x99, 3),
            ),
            (
                "spends a later run block's output",
                vec![block(one), spends(OutPoint::new([0x31; 32], 0)), block(three)],
                missing(0x31, 0),
            ),
            ("one prevout twice in a tx", vec![block(one), spends_twice], missing(0x10, 0)),
            (
                "a prevout an earlier run block spent",
                vec![block(one), block(two), respends],
                FoldError::MissingPrevout {
                    height: h(3),
                    txid: id(0x30),
                    spent: id(0x10),
                    vout: 0,
                },
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
            let store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
            let run: Vec<&Block> = [&genesis].into_iter().chain(&blocks).map(|b| &**b).collect();
            let mut out: Vec<BlockChanges> =
                run.iter().map(|block| store.changes(block.block_ref())).collect();
            let folded = fold_run(&UtxoSetReader::new(store.applied()), &run, &mut out);
            assert_eq!(folded.err(), Some(expected), "{case}");
        }
    }

    /// - fees read back for a held block = as folded
    /// - height with no record, or a record != one fee per the block's transactions = named error
    #[test]
    fn stored_fees_read_back_and_a_missing_or_mismatched_record_is_a_named_error() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        let one = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x11; 32]).pay(&alice, 50_000)).tx(|t| {
                t.txid([0x20; 32])
                    .spend(OutPoint::new([0x10; 32], 0))
                    .pay(&alice, 99_000)
                    .fee(1_000)
            })
        });
        let two = chain.mine(|b| b.coinbase(|c| c.txid([0x12; 32]).pay(&alice, 50_000)));
        let blocks = chain.blocks(one);
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        for block in &blocks {
            let mut changes = store.changes(block.block_ref());
            fold(&UtxoSetReader::new(store.applied()), block, &mut changes).expect("folds");
            store.apply(changes);
        }
        let mut txs = blocks[1].transactions().to_vec();
        txs.push(txs[1].clone());
        let longer = Block::new(blocks[1].header().clone(), txs);

        let held = UtxoSetReader::new(store.applied());
        assert_eq!(held.block_fees(&blocks[1]), Ok(chain.fees(one.hash)), "as folded");
        let missing = held.block_fees(chain.block(two.hash));
        assert_eq!(missing, Err(FoldError::StoredFeesMissing { height: h(2) }));
        let mismatched = held.block_fees(&longer);
        assert_eq!(mismatched, Err(FoldError::StoredFeesUnreadable { height: h(1) }));
    }

    /// Writer over `store` (`queue` bytes = one run's) + the fee stream it publishes
    fn start(store: DiskStore, queue: NonZeroUsize) -> (Writer<DiskView>, Subscription<BlockFees>) {
        let writer = UtxoSetIndexWriter::new(store);
        let mut fee_sink = FeeSink::new();
        let consumer = fee_sink.subscribe("consumer", QUEUE);
        (
            Writer::spawn(writer.handle(), queue, move |blocks| writer.run(blocks, fee_sink)),
            consumer,
        )
    }

    /// Every fee step through `Shutdown`
    async fn drained(consumer: &mut Subscription<BlockFees>) -> Vec<BlockFees> {
        let mut out = Vec::new();
        while let Step::Apply(data) = consumer.next().await {
            out.push(BlockFees::clone(&data));
        }
        out
    }

    /// Four bulk blocks, each its own commit (write_buffer = 1 byte), crashed after every operation:
    /// each state reopens to an acknowledged or attempted commit, and the writer on it resolves
    /// the next block's fees against the outputs it recovered and commits it
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_durable_prefix_whose_outputs_still_resolve() {
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
                    t.txid([txid; 32])
                        .spend(OutPoint::new([spent; 32], 0))
                        .pay(&alice, paid)
                        .fee(10_000)
                })
            });
        }
        let blocks = chain.blocks(chain.tip());
        let expected: Vec<BlockFees> =
            blocks.iter().map(|block| chain.fees(block.header().hash)).collect();
        let tip_of = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height));

        // - commits of 0..=3 (4 only ever committed after a recovery)
        // - tag = commits acknowledged
        let (fs, recording) = SimFs::recording();
        let (mut writer, mut consumer) = start(open(&fs, &SCHEMA, NonZeroUsize::MIN), QUEUE);
        for (acked, block) in (1u64..).zip(&blocks[..4]) {
            writer.send(step(block)).await;
            writer.durable_at(block.header().height).await;
            fs.set_tag(acked);
        }
        writer.stop().await.expect("clean stop");
        let first = &expected[..4];
        assert_eq!(drained(&mut consumer).await, first, "one fee step per block, Shutdown last");
        let tip_after = |commits: u64| (commits > 0).then(|| (commits - 1).min(3) as u32);

        let states = recording.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let crashed = &state.label;
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MAX);
            let tip = tip_of(&store.durable());
            let acked = [tip_after(state.tag), tip_after(state.tag + 1)];
            assert!(acked.contains(&tip), "{crashed}: recovered through {tip:?}");

            let next = tip.map_or(0, |tip| tip + 1);
            let (writer, mut consumer) = start(store, QUEUE);
            writer.send(step(&blocks[next as usize])).await;
            let handle = writer
                .stop()
                .await
                .unwrap_or_else(|error| panic!("{crashed}: commit after recovery: {error}"));
            let resolved = drained(&mut consumer).await;
            assert_eq!(resolved, [expected[next as usize].clone()], "{crashed}");
            assert_eq!(tip_of(&handle.durable_view()), Some(next), "{crashed}: durable");
        }
    }

    /// Every prevout resolves wherever it lives (durable, buffered, earlier in its own run)
    ///
    /// - 1: spends 0's output and one from earlier in its own block
    /// - 2: enters orchard and sprout
    /// - 3: spends 1's outputs with value leaving sprout
    /// - 4: spends 3's and enters ironwood
    ///
    /// - Boot 1: 0..=4 (4 `FinalizedTip`), one fee step each
    /// - Boot 2, compact-block durable at 0: 1..=4 resent, all held → re-folded, fees out again,
    ///   identical
    /// - Both queue sizes: 1 byte = one block per run, 1 MiB = one run
    #[tokio::test]
    async fn fees_resolve_every_prevout_wherever_it_lives_and_held_heights_republish_them() {
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x11; 32]).pay(&alice, 625_000_000))
                .tx(|t| {
                    t.txid([0x21; 32])
                        .spend(OutPoint::new([0x10; 32], 0))
                        .pay(&alice, 60_000)
                        .pay(&alice, 39_000)
                        .fee(1_000)
                })
                .tx(|t| {
                    t.txid([0x22; 32])
                        .spend(OutPoint::new([0x21; 32], 1))
                        .pay(&alice, 30_000)
                        .value_balance(ShieldedPool::Sapling, -8_000)
                        .fee(1_000)
                })
        });
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x12; 32]).pay(&alice, 625_000_000)).tx(|t| {
                t.txid([0x23; 32])
                    .spend(OutPoint::new([0x21; 32], 0))
                    .value_balance(ShieldedPool::Orchard, -58_500)
                    .sprout_balance(-500)
                    .fee(1_000)
            })
        });
        chain.mine(|b| {
            b.coinbase(|c| c.txid([0x13; 32]).pay(&alice, 625_000_000)).tx(|t| {
                t.txid([0x24; 32])
                    .spend(OutPoint::new([0x22; 32], 0))
                    .spend(OutPoint::new([0x11; 32], 0))
                    .pay(&alice, 625_029_500)
                    .sprout_balance(500)
                    .fee(1_000)
            })
        });
        let tip = chain.mine(|b| {
            b.coinbase(|c| c.txid([0x14; 32]).pay(&alice, 625_000_000)).tx(|t| {
                t.txid([0x25; 32])
                    .spend(OutPoint::new([0x24; 32], 0))
                    .pay(&alice, 625_000_000)
                    .value_balance(ShieldedPool::Ironwood, -29_000)
                    .fee(500)
            })
        });
        let blocks = chain.blocks(tip);
        let expected: Vec<BlockFees> =
            blocks.iter().map(|block| chain.fees(block.header().hash)).collect();
        for queue in [NonZeroUsize::MIN, QUEUE] {
            let fs = SimFs::new();
            let (mut writer, mut consumer) = start(open(&fs, &SCHEMA, NonZeroUsize::MAX), queue);
            for block in &blocks[..4] {
                writer.send(step(block)).await;
            }
            writer.send(finalized(&blocks[4])).await;
            writer.durable_at(h(4)).await;
            writer.stop().await.expect("clean stop");
            assert_eq!(drained(&mut consumer).await, expected, "queue {queue}: one per block");

            let (writer, mut consumer) = start(open(&fs, &SCHEMA, NonZeroUsize::MAX), queue);
            for block in &blocks[1..] {
                writer.send(step(block)).await;
            }
            let handle = writer.stop().await.expect("clean stop");
            assert_eq!(drained(&mut consumer).await, expected[1..], "queue {queue}: republished");
            assert_eq!(handle.durable_height(), Some(h(4)), "held: nothing new");
        }
    }

    /// Fold error (each kind: the fold tests) → writer panic naming its index + block
    #[tokio::test]
    #[should_panic(expected = "utxo_set index: block 1 tx")]
    async fn a_fold_error_panics_the_writer_naming_the_index() {
        let (writer, _consumer) = start(open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX), QUEUE);
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest()
            .genesis_with(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        let one = chain.mine(|b| {
            b.tx(|t| t.txid([0x20; 32]).spend(OutPoint::new([0x10; 32], 0)).pay(&alice, 1))
        });
        // lie: 0x20 spends an output never mined
        let mut txs = chain.block(one.hash).transactions().to_vec();
        txs[1].transparent.inputs[0] = OutPoint::new([0x99; 32], 3);
        let unrecorded = Arc::new(Block::new(chain.block(one.hash).header().clone(), txs));
        writer.send(step(chain.block(chain.genesis().hash))).await;
        writer.send(step(&unrecorded)).await;
        let Err(panicked) = writer.stop().await else { panic!("writer stopped cleanly") };
        std::panic::resume_unwind(panicked.into_panic());
    }
}
