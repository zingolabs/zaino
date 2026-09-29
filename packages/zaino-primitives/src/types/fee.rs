//! Each transaction's fee, and the block that carries one per transaction

use super::{Block, BlockHash, Height, Zatoshis};

/// What a transaction leaves in the transparent transaction value pool for the miner
/// (protocol.pdf#transactions §3.4)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fee {
    /// Collects fees and subsidy, pays none (protocol.pdf#coinbasetransactions §3.11)
    Coinbase,
    Paid(Zatoshis),
}

/// One [`Fee`] per transaction of the block `hash` names, in block order
///
/// - `hash` = which branch (a consumer pairing it with a block drops a stale one after a reorg)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockFees {
    pub height: Height,
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
