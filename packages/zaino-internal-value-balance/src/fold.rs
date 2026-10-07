//! Block → its outputs' rows + its fees (pure: prevouts resolved through the parent reader)
//!
//! - parent = any state at or past the block's parent (insert only: a later one resolves the same)

use std::collections::{HashMap, HashSet};

use zaino_persistence::{Changes, MapRead};
use zaino_primitives::types::{
    Block, BlockFees, BlockRef, Fee, Height, OutPoint, OutputIndex, Transaction, TransactionId,
    Zatoshis,
};

use crate::{encode_value, schema, ValueBalanceReader, OUTPUTS};

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

pub fn fold<V: MapRead>(
    parent: &ValueBalanceReader<V>,
    block: &Block,
) -> Result<(Changes, BlockFees), FoldError> {
    let mut folded = fold_run(parent, [block])?;
    Ok(folded.pop().expect("one block in, one folded out"))
}

/// [`fold`] of each of `blocks` (oldest first) onto `parent` + the run's earlier blocks
///
/// - prevouts from outside the run: one probe for the whole run (a sandblast tx spends thousands)
pub(crate) fn fold_run<'a, V: MapRead>(
    parent: &ValueBalanceReader<V>,
    blocks: impl IntoIterator<Item = &'a Block>,
) -> Result<Vec<(Changes, BlockFees)>, FoldError> {
    let blocks: Vec<&Block> = blocks.into_iter().collect();
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

    let schema = schema(parent.network());
    let mut folded = Vec::with_capacity(blocks.len());
    for block in blocks {
        let header = block.header();
        let mut changes =
            Changes::new(BlockRef { hash: header.hash, height: header.height }, &schema);
        for (at, value) in outputs(block) {
            changes.insert(OUTPUTS, &at.encode(), &encode_value(value));
            known.insert(at, value);
        }
        let fees = block.transactions().iter().map(|tx| fee(header.height, tx, &known));
        let fees = fees.collect::<Result<_, _>>()?;
        folded.push((changes, BlockFees { height: header.height, hash: header.hash, fees }));
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
    use std::path::Path;
    use std::sync::Arc;

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store};
    use zaino_primitives::testing::{h, outpoint, p2pkh, MockChain};
    use zaino_primitives::types::{ShieldedPool, SignedZatoshis};
    use zcash_protocol::consensus::NetworkType;

    use super::*;

    /// Block 0 folded onto nothing, then 1 and 2 as one run onto 0: each prevout resolves from
    /// the parent, its own block or an earlier run block; fees = the stated ones; rows = golden
    /// bytes; the run = the same blocks folded one at a time
    #[test]
    fn fees_resolve_through_the_parent_and_the_run_and_a_run_folds_like_single_blocks() {
        let network = NetworkType::Regtest;
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
        let chain_fees = |block: &Arc<Block>| chain.fees(block.header().hash);
        let blocks = chain.blocks(two);
        let store = DiskEngine::new(SimFs::new()).open(Path::new("/vb"), &schema(network));
        let mut store = store.expect("open");
        let (genesis, genesis_fees) =
            fold(&ValueBalanceReader::new(store.staged(), network), &blocks[0])
                .expect("coinbase only");
        assert_eq!(genesis_fees.fees, [Fee::Coinbase]);
        store.apply(genesis);

        let parent = ValueBalanceReader::new(store.staged(), network);
        let run = fold_run(&parent, [&*blocks[1], &*blocks[2]]).expect("every prevout held");
        let run_fees: Vec<BlockFees> =
            run.iter().map(|(_, block_fees)| block_fees.clone()).collect();
        assert_eq!(run_fees, [chain_fees(&blocks[1]), chain_fees(&blocks[2])]);
        let rows: Vec<(&[u8], &[u8])> = run[0].0.inserts(OUTPUTS).collect();
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

        for (block, (run_changes, run_fees)) in blocks[1..].iter().zip(&run) {
            let (changes, block_fees) =
                fold(&ValueBalanceReader::new(store.staged(), network), block).expect("held");
            assert_eq!(&block_fees, run_fees, "{:?}", block.header().height);
            assert_eq!(
                changes.inserts(OUTPUTS).collect::<Vec<_>>(),
                run_changes.inserts(OUTPUTS).collect::<Vec<_>>()
            );
            assert_eq!(changes.tip(), run_changes.tip());
            store.apply(changes);
        }
    }

    /// Every rejection names its block and tx; a run never lets a block spend a later one's output
    ///
    /// - valid blocks out of order: 2 onto a parent missing 1, then 2 before 1 in one run
    /// - lies (one field of a mined tx edited): outputs past the inputs, sapling past the inputs
    #[test]
    fn an_unrecorded_prevout_a_forward_spend_or_a_negative_fee_is_a_named_error() {
        let network = NetworkType::Regtest;
        let alice = p2pkh([0xaa; 20]);
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.coinbase(|c| c.txid([0x10; 32]).pay(&alice, 100_000)));
        let two = chain.mine(|b| {
            b.tx(|t| {
                t.txid([0x20; 32]).spend(outpoint([0x10; 32], 0)).pay(&alice, 99_000).fee(1_000)
            })
        });
        let three = chain.mine(|b| {
            b.tx(|t| {
                t.txid([0x30; 32])
                    .spend(outpoint([0x20; 32], 0))
                    .value_balance(ShieldedPool::Sapling, -99_000)
                    .fee(0)
            })
        });
        let block = |at: BlockRef| Arc::clone(chain.block(at.hash));
        let lie = |at: BlockRef, edit: fn(&mut Transaction)| {
            let mut txs = chain.block(at.hash).transactions().to_vec();
            edit(&mut txs[1]);
            Arc::new(Block::new(chain.block(at.hash).header().clone(), txs))
        };
        let overpaid = lie(two, |tx| {
            tx.transparent.outputs[0].value = Zatoshis::new(100_001).expect("in supply");
        });
        let overshielded = lie(three, |tx| {
            tx.sapling.value_balance = SignedZatoshis::new(-99_001).expect("in supply");
        });
        let id = |byte| TransactionId::from([byte; 32]);
        let unrecorded =
            || FoldError::MissingPrevout { height: h(2), txid: id(0x20), spent: id(0x10), vout: 0 };
        let cases = [
            ("never recorded", vec![block(two)], unrecorded()),
            ("spends a later run block's output", vec![block(two), block(one)], unrecorded()),
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
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/vb"), &schema(network));
            let parent = ValueBalanceReader::new(store.expect("open").staged(), network);
            let folded = fold_run(&parent, blocks.iter().map(|block| &**block)).err();
            assert_eq!(folded, Some(expected), "{case}");
        }
    }
}
