//! Each transaction's fee, and the block that carries one per transaction

use zcash_protocol::value::{ZatBalance, Zatoshis};

use super::{Block, Transaction};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::{BlockHash, ShieldedPool};

/// Left in the transparent value pool for the miner (protocol.pdf §3.4)
///
/// - `Coinbase`: collects fees + subsidy, pays none (protocol.pdf §3.11)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fee {
    Coinbase,
    Paid(Zatoshis),
}

/// A transaction that leaves no fee
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FeeError {
    #[error("pays out more than it takes in (a negative fee)")]
    Negative,
    #[error("a value sum past the money supply")]
    Overflow,
}

impl Transaction {
    /// Value left in the transparent pool (protocol.pdf §3.4), `spent` = Σ its prevouts' values
    ///
    /// - Σ transparent inputs − Σ transparent outputs + each shielded pool's value balance
    pub fn fee(&self, spent: Zatoshis) -> Result<Fee, FeeError> {
        if self.transparent.coinbase {
            return Ok(Fee::Coinbase);
        }
        let remaining = self.value_left(spent).ok_or(FeeError::Overflow)?;
        // MUST be nonnegative (protocol.pdf §3.4 consensus rule)
        let remaining = u64::try_from(remaining).map_err(|_| FeeError::Negative)?;
        Ok(Fee::Paid(Zatoshis::from_u64(remaining).map_err(|_| FeeError::Overflow)?))
    }

    /// `spent` − Σ outputs + each pool's value balance, negative = overspent (`None`: outputs past
    /// the supply)
    pub fn value_left(&self, spent: Zatoshis) -> Option<i64> {
        let outputs = self.transparent.outputs.iter().map(|out| out.value);
        let paid = outputs.sum::<Option<Zatoshis>>()?;
        let signed = |zats: Zatoshis| i64::from(ZatBalance::from(zats));
        let shielded = ShieldedPool::ALL.map(|pool| i64::from(self.value_balance(pool)));
        // every term within ±MAX_MONEY (ZIP 209) → Σ of six fits i64
        let pools = i64::from(self.sprout.value_balance) + shielded.iter().sum::<i64>();
        Some(signed(spent) - signed(paid) + pools)
    }
}

/// One [`Fee`] per transaction of the block `hash` names, in block order
///
/// - `hash` = which branch (a consumer pairing it with a block drops a stale one after a reorg)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFees {
    pub height: BlockHeight,
    pub hash: BlockHash,
    pub fees: Vec<Fee>,
}

impl BlockFees {
    /// Bytes held in memory: inline + the fees' capacity
    pub fn footprint(&self) -> usize {
        size_of::<Self>() + size_of::<Fee>() * self.fees.capacity()
    }

    pub fn belongs_to(&self, block: &Block) -> bool {
        self.hash == block.header().hash && self.fees.len() == block.transactions().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OrchardData, SaplingData, SproutData, TransparentData, TransparentOutput};
    use zcash_protocol::value::MAX_MONEY;
    use zcash_protocol::TxId;
    use zcash_transparent::address::Script;

    /// Σ prevouts − Σ outputs + each pool's value balance (sprout, sapling, orchard, ironwood:
    /// one zatoshi apiece, so a dropped or flipped term shows)
    ///
    /// - coinbase pays none
    /// - below 0 = `Negative`, past the supply = `Overflow`
    #[test]
    fn a_fee_is_what_inputs_and_pools_leave_and_never_below_zero_or_past_the_supply() {
        let tx = |coinbase: bool, outputs: &[u64], pools: [i64; 4]| {
            let output = |value: u64| TransparentOutput {
                value: Zatoshis::from_u64(value).expect("in supply"),
                script: Script::default(),
            };
            let signed = |n: i64| ZatBalance::from_i64(n).expect("in supply");
            Transaction {
                txid: TxId::from_bytes([1; 32]),
                transparent: TransparentData {
                    coinbase,
                    inputs: Vec::new(),
                    outputs: outputs.iter().copied().map(output).collect(),
                },
                sprout: SproutData { value_balance: signed(pools[0]) },
                sapling: SaplingData { value_balance: signed(pools[1]), ..Default::default() },
                orchard: OrchardData { value_balance: signed(pools[2]), ..Default::default() },
                ironwood: OrchardData { value_balance: signed(pools[3]), ..Default::default() },
            }
        };
        let paid = |n: u64| Ok(Fee::Paid(Zatoshis::from_u64(n).expect("in supply")));
        let supply = MAX_MONEY;
        let rows: [(&str, Transaction, u64, Result<Fee, FeeError>); 7] = [
            ("transparent only", tx(false, &[6_000, 3_000], [0; 4]), 10_000, paid(1_000)),
            ("every pool adds", tx(false, &[9_000], [1, 10, 100, 1_000]), 10_000, paid(2_111)),
            ("a pool takes", tx(false, &[9_000], [0, -500, 0, 0]), 10_000, paid(500)),
            ("pays exactly in", tx(false, &[10_000], [0; 4]), 10_000, paid(0)),
            ("pays more", tx(false, &[10_001], [0; 4]), 10_000, Err(FeeError::Negative)),
            ("coinbase", tx(true, &[50_000], [0; 4]), 0, Ok(Fee::Coinbase)),
            ("past the supply", tx(false, &[], [1, 0, 0, 0]), supply, Err(FeeError::Overflow)),
        ];
        for (row, tx, spent, expected) in rows {
            let spent = Zatoshis::from_u64(spent).expect("in supply");
            assert_eq!(tx.fee(spent), expected, "{row}");
        }
    }
}
