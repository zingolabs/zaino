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
pub(crate) mod tests {
    use std::path::Path;

    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store};
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        OrchardData, SaplingData, Script, SignedZatoshis, SproutData, TransparentData,
        TransparentOutput,
    };
    use zcash_protocol::consensus::NetworkType;

    use super::*;

    /// `(txid tag, spends (tag, vout), outputs, [sprout, sapling, orchard, ironwood] balances)`
    pub(crate) fn tx(
        tag: u8,
        spends: &[(u8, u32)],
        outputs: &[u64],
        shielded: [i64; 4],
    ) -> Transaction {
        let signed = |value| SignedZatoshis::new(value).expect("in supply");
        Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: TransparentData {
                coinbase: false,
                inputs: spends
                    .iter()
                    .map(|&(prev, vout)| OutPoint { txid: TransactionId::from([prev; 32]), vout })
                    .collect(),
                outputs: outputs
                    .iter()
                    .map(|&value| TransparentOutput {
                        value: Zatoshis::new(value).expect("in supply"),
                        script: Script::new(vec![0x51]),
                    })
                    .collect(),
            },
            sprout: SproutData { value_balance: signed(shielded[0]) },
            sapling: SaplingData { value_balance: signed(shielded[1]), ..Default::default() },
            orchard: OrchardData { value_balance: signed(shielded[2]), ..Default::default() },
            ironwood: OrchardData { value_balance: signed(shielded[3]), ..Default::default() },
        }
    }

    pub(crate) fn coinbase(tag: u8, value: u64) -> Transaction {
        let mut tx = tx(tag, &[], &[value], [0; 4]);
        tx.transparent.coinbase = true;
        tx
    }

    /// Per tx fee, in zats (`None` = coinbase)
    pub(crate) fn fees(block_fees: &BlockFees) -> Vec<Option<u64>> {
        let paid = |fee: &Fee| match fee {
            Fee::Coinbase => None,
            Fee::Paid(fee) => Some(fee.as_u64()),
        };
        block_fees.fees.iter().map(paid).collect()
    }

    /// Block 0 folded onto nothing, then 1 and 2 as one run onto 0: each prevout resolves from
    /// the parent, its own block or an earlier run block; rows = golden bytes; the run = the
    /// same blocks folded one at a time
    #[test]
    #[rustfmt::skip]
    fn fees_resolve_through_the_parent_and_the_run_and_a_run_folds_like_single_blocks() {
        let network = NetworkType::Regtest;
        //      tx(tag,  spends (tag, vout), outputs,          shielded balances
        let chain = linked(vec![
            vec![coinbase(0x10, 100_000)],
            vec![
                coinbase(0x11, 50_000),
                tx(0x21, &[(0x10, 0)],       &[60_000, 39_000], [0; 4]),
                tx(0x22, &[(0x21, 1)],       &[30_000],         [0, -8_000, 0, 0]),
            ],
            vec![
                coinbase(0x12, 50_000),
                tx(0x23, &[(0x21, 0)],       &[],               [0, 0, -59_000, 0]),
            ],
        ]);
        let store = DiskEngine::new(SimFs::new()).open(Path::new("/vb"), &schema(network));
        let mut store = store.expect("open");
        let (genesis, genesis_fees) = fold(&ValueBalanceReader::new(store.staged(), network), &chain[0]).expect("coinbase only");
        assert_eq!(fees(&genesis_fees), [None]);
        store.apply(genesis);

        let parent = ValueBalanceReader::new(store.staged(), network);
        let run = fold_run(&parent, [&*chain[1], &*chain[2]]).expect("every prevout held");
        let run_fees: Vec<_> = run.iter().map(|(_, block_fees)| fees(block_fees)).collect();
        assert_eq!(run_fees, [vec![None, Some(1_000), Some(1_000)], vec![None, Some(1_000)]]);
        let rows: Vec<(&[u8], &[u8])> = run[0].0.inserts(OUTPUTS).collect();
        let row = |tag: u8, vout: u8, value: [u8; 8]| ([[tag; 32].as_slice(), &[0, 0, 0, vout]].concat(), value);
        let golden = [
            row(0x11, 0, [0, 0, 0, 0, 0, 0, 0xc3, 0x50]),
            row(0x21, 0, [0, 0, 0, 0, 0, 0, 0xea, 0x60]),
            row(0x21, 1, [0, 0, 0, 0, 0, 0, 0x98, 0x58]),
            row(0x22, 0, [0, 0, 0, 0, 0, 0, 0x75, 0x30]),
        ];
        let golden: Vec<(&[u8], &[u8])> = golden.iter().map(|(key, value)| (&key[..], &value[..])).collect();
        assert_eq!(rows, golden, "block 1: txid ‖ vout BE → value BE, block order");

        for (block, (run_changes, run_fees)) in chain[1..].iter().zip(&run) {
            let (changes, block_fees) = fold(&ValueBalanceReader::new(store.staged(), network), block).expect("held");
            assert_eq!(&block_fees, run_fees, "{:?}", block.header().height);
            assert_eq!(changes.inserts(OUTPUTS).collect::<Vec<_>>(), run_changes.inserts(OUTPUTS).collect::<Vec<_>>());
            assert_eq!(changes.tip(), run_changes.tip());
            store.apply(changes);
        }
    }

    /// Every rejection names its block and tx; a run never lets a block spend a later one's output
    #[test]
    fn an_unrecorded_prevout_a_forward_spend_or_a_negative_fee_is_a_named_error() {
        let network = NetworkType::Regtest;
        let id = |byte| TransactionId::from([byte; 32]);
        let h = |n| Height::try_from(n).expect("h");
        let cases = [
            (
                "never recorded",
                vec![vec![coinbase(0x10, 100_000), tx(0x20, &[(0x99, 3)], &[1], [0; 4])]],
                FoldError::MissingPrevout {
                    height: h(0),
                    txid: id(0x20),
                    spent: id(0x99),
                    vout: 3,
                },
            ),
            (
                "spends the next block's output",
                vec![
                    vec![coinbase(0x10, 100_000), tx(0x20, &[(0x31, 0)], &[1], [0; 4])],
                    vec![coinbase(0x11, 100_000), tx(0x31, &[(0x10, 0)], &[1], [0; 4])],
                ],
                FoldError::MissingPrevout {
                    height: h(0),
                    txid: id(0x20),
                    spent: id(0x31),
                    vout: 0,
                },
            ),
            (
                "transparent outputs > inputs",
                vec![vec![coinbase(0x10, 100_000), tx(0x20, &[(0x10, 0)], &[100_001], [0; 4])]],
                FoldError::NegativeFee { height: h(0), txid: id(0x20) },
            ),
            (
                "value into sapling from nothing",
                vec![vec![coinbase(0x10, 100_000), tx(0x20, &[], &[], [0, -1, 0, 0])]],
                FoldError::NegativeFee { height: h(0), txid: id(0x20) },
            ),
        ];

        for (case, blocks, expected) in cases {
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/vb"), &schema(network));
            let parent = ValueBalanceReader::new(store.expect("open").staged(), network);
            let chain = linked(blocks);
            let folded = fold_run(&parent, chain.iter().map(|block| &**block)).err();
            assert_eq!(folded, Some(expected), "{case}");
        }
    }
}
