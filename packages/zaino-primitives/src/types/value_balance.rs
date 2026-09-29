//! Per-transaction value flow across pools, and the block that carries one per transaction.

use super::{Block, BlockHash, Height, SignedZatoshis, Transaction, Zatoshis};

/// Value one transaction moves out of each pool (+ = out); Σ = its fee
///
/// - `fee_paid` in `zcash_primitives::transaction` (same terms, same signs)
/// - Coinbase: Σ < 0 (issuance enters the transparent pool)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValueBalance {
    pub transparent: SignedZatoshis,
    pub sprout: SignedZatoshis,
    pub sapling: SignedZatoshis,
    pub orchard: SignedZatoshis,
    pub ironwood: SignedZatoshis,
}

/// Transparent outputs summing past the money supply (consensus-invalid)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("transparent outputs sum past the money supply")]
pub struct OutputsOverflow;

impl ValueBalance {
    /// `spent` = Σ values of the prevouts `tx` spends (coinbase: none)
    pub fn of(tx: &Transaction, spent: Zatoshis) -> Result<Self, OutputsOverflow> {
        let paid = Zatoshis::sum_balances(tx.transparent.outputs.iter().map(|out| out.value))
            .ok_or(OutputsOverflow)?;
        let transparent = SignedZatoshis::new(spent.as_i64() - paid.as_i64())
            .expect("difference of two in-supply amounts within supply magnitude");
        Ok(Self {
            transparent,
            sprout: tx.sprout.value_balance,
            sapling: tx.sapling.value_balance,
            orchard: tx.orchard.value_balance,
            ironwood: tx.ironwood.value_balance,
        })
    }

    /// Σ over the pools; `None` = coinbase (valid non-coinbase txs never pay < 0)
    pub fn fee(&self) -> Option<Zatoshis> {
        // |each| <= supply (2.1e15) → five of them fit i64
        let total = [self.transparent, self.sprout, self.sapling, self.orchard, self.ironwood]
            .into_iter()
            .map(i64::from)
            .sum::<i64>();
        u64::try_from(total).ok().and_then(|fee| Zatoshis::new(fee).ok())
    }
}

/// One [`ValueBalance`] per transaction of the block `hash` names, in block order
///
/// - `hash` = which branch (a consumer pairing it with a block drops a stale one after a reorg)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockValueBalances {
    pub height: Height,
    pub hash: BlockHash,
    pub balances: Vec<ValueBalance>,
}

impl BlockValueBalances {
    /// Bytes held in memory: inline + the balances' capacity
    pub fn footprint(&self) -> usize {
        size_of::<Self>() + size_of::<ValueBalance>() * self.balances.capacity()
    }

    pub fn belongs_to(&self, block: &Block) -> bool {
        self.hash == block.header().hash && self.balances.len() == block.transactions().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        transaction::{OrchardData, SaplingData, SproutData},
        Script, TransactionId, TransparentData, TransparentOutput,
    };

    fn zats(value: u64) -> Zatoshis {
        Zatoshis::new(value).expect("in supply")
    }

    fn signed(value: i64) -> SignedZatoshis {
        SignedZatoshis::new(value).expect("in supply")
    }

    fn tx(outputs: &[u64], sprout: i64, sapling: i64, orchard: i64, ironwood: i64) -> Transaction {
        Transaction {
            txid: TransactionId::from([1; 32]),
            transparent: TransparentData {
                inputs: Vec::new(),
                outputs: outputs
                    .iter()
                    .map(|value| TransparentOutput {
                        value: zats(*value),
                        script: Script::new(vec![0x51]),
                    })
                    .collect(),
            },
            sprout: SproutData { value_balance: signed(sprout) },
            sapling: SaplingData { value_balance: signed(sapling), ..Default::default() },
            orchard: OrchardData { value_balance: signed(orchard), ..Default::default() },
            ironwood: OrchardData { value_balance: signed(ironwood), ..Default::default() },
        }
    }

    /// Fee = Σ every pool's flow, each pool's sign respected; coinbase (issuance) has none
    #[test]
    fn fee_sums_every_pool_and_coinbase_has_none() {
        let cases = [
            // (spent, outputs, sprout, sapling, orchard, ironwood) → fee
            ((10_000, vec![9_000], 0, 0, 0, 0), Some(1_000)),
            ((0, vec![], 0, 0, 5_000, 0), Some(5_000)),
            ((0, vec![40_000], 0, 50_000, 0, 0), Some(10_000)),
            ((20_000, vec![], 0, -15_000, 0, -4_000), Some(1_000)),
            ((0, vec![7_000], 10_000, 0, 0, 0), Some(3_000)),
            ((0, vec![], 0, 0, 0, 0), Some(0)),
            // coinbase: no prevouts, subsidy + fees paid out
            ((0, vec![312_500_000, 12_500], 0, 0, 0, 0), None),
        ];

        for ((spent, outputs, sprout, sapling, orchard, ironwood), fee) in cases {
            let tx = tx(&outputs, sprout, sapling, orchard, ironwood);
            let balance = ValueBalance::of(&tx, zats(spent)).expect("in supply");
            let paid = outputs.iter().sum::<u64>() as i64;
            assert_eq!(balance.transparent, signed(spent as i64 - paid), "spent − paid");
            assert_eq!(balance.fee(), fee.map(zats), "{spent} spent, outputs {outputs:?}");
        }

        let overflowing = tx(&[Zatoshis::MAX.as_u64(), 1], 0, 0, 0, 0);
        assert_eq!(ValueBalance::of(&overflowing, Zatoshis::ZERO), Err(OutputsOverflow));
    }
}
