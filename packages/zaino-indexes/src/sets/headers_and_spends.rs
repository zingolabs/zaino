//! Headers + transparent spends index set.
//!
//! Set-wide context carries header fields + transparent spend data.

use zaino_primitives::types::{
    Block, BlockHash, BlockTime, CompactDifficulty, OutputIndex, TransactionHash,
};
use zaino_sync::index_set::IndexSet;
use zaino_sync::primitives::BlockHeight;
use zaino_sync::traits::ProvideContext;

use crate::indexes::headers::{HeaderCtx, HeadersIndex};
use crate::indexes::transparent_spends::{SpendCtx, TransparentSpendsIndex};

/// Set-wide context: header fields + transparent spends.
#[derive(Debug, Clone)]
pub struct HeadersAndSpendsContext {
    /// Block height (sync-engine type).
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty (nBits).
    pub bits: CompactDifficulty,
    /// All transparent spends: (prev_txid, prev_index, spending_txid).
    pub spends: Vec<(TransactionHash, OutputIndex, TransactionHash)>,
}

/// Build context from a domain Block.
pub fn context_from_block(block: &Block) -> HeadersAndSpendsContext {
    let spends: Vec<(TransactionHash, OutputIndex, TransactionHash)> = block
        .transactions
        .iter()
        .flat_map(|tx| {
            tx.transparent
                .inputs
                .iter()
                .map(move |input| (input.prev_txid, input.prev_index, tx.txid))
        })
        .collect();

    HeadersAndSpendsContext {
        height: BlockHeight::new(u64::from(block.header.height)),
        hash: block.header.hash,
        prev_hash: block.header.prev_hash,
        time: block.header.time,
        bits: block.header.bits,
        spends,
    }
}

impl ProvideContext<HeaderCtx> for HeadersAndSpendsContext {
    fn context(&self) -> HeaderCtx {
        HeaderCtx {
            height: self.height,
            hash: self.hash,
            prev_hash: self.prev_hash,
            time: self.time,
            bits: self.bits,
        }
    }
}

impl ProvideContext<SpendCtx> for HeadersAndSpendsContext {
    fn context(&self) -> SpendCtx {
        SpendCtx {
            spends: self.spends.clone(),
        }
    }
}

/// Build the headers + transparent spends index set.
pub fn index_set() -> IndexSet<HeadersAndSpendsContext> {
    IndexSet::new()
        .with::<HeadersIndex>()
        .with::<TransparentSpendsIndex>()
}
