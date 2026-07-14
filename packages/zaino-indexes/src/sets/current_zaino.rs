//! current_zaino index set: all indexes matching zaino-state's V1 schema.
//!
//! 8 BlockLocal×Append indexes covering headers, txids, transparent,
//! sapling, orchard, hash→height, txid→location, and outpoint→spender.

use zaino_primitives::types::{
    Block, BlockHash, BlockTime, CompactDifficulty, OutputIndex, TransactionHash,
};
use zaino_sync::index_set::IndexSet;
use zaino_sync::primitives::BlockHeight;
use zaino_sync::traits::ProvideContext;

use crate::indexes::hash_to_height::{HashToHeightCtx, HashToHeightIndex};
use crate::indexes::headers::{HeaderCtx, HeadersIndex};
use crate::indexes::orchard::{OrchardCtx, OrchardIndex, OrchardTxCompact};
use crate::indexes::sapling::{SaplingCtx, SaplingIndex, SaplingTxCompact};
use crate::indexes::transparent_data::{
    TransparentDataCtx, TransparentDataIndex, TransparentTxCompact,
};
use crate::indexes::transparent_spends::{SpendCtx, TransparentSpendsIndex};
use crate::indexes::txid_location::{TxidLocationCtx, TxidLocationIndex};
use crate::indexes::txids::{TxidsCtx, TxidsIndex};

/// Set-wide context for the full zaino index set.
#[derive(Debug, Clone)]
pub struct CurrentZainoContext {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Previous block hash.
    pub prev_hash: BlockHash,
    /// Block timestamp.
    pub time: BlockTime,
    /// Compact difficulty.
    pub bits: CompactDifficulty,
    /// Transaction ids.
    pub txids: Vec<TransactionHash>,
    /// Transparent spends: (prev_txid, prev_index, spending_txid).
    pub spends: Vec<(TransactionHash, OutputIndex, TransactionHash)>,
    /// Txid locations: (txid, height, tx_index).
    pub txid_locations: Vec<(TransactionHash, BlockHeight, u32)>,
    /// Per-tx transparent data.
    pub transparent_txs: Vec<TransparentTxCompact>,
    /// Per-tx sapling data.
    pub sapling_txs: Vec<SaplingTxCompact>,
    /// Per-tx orchard data.
    pub orchard_txs: Vec<OrchardTxCompact>,
}

/// Build context from a domain Block.
pub fn context_from_block(block: &Block) -> CurrentZainoContext {
    let height = BlockHeight::new(u64::from(block.header.height));

    let txids: Vec<TransactionHash> = block.transactions.iter().map(|tx| tx.txid).collect();

    let spends: Vec<(TransactionHash, OutputIndex, TransactionHash)> = block
        .transactions
        .iter()
        .flat_map(|tx| {
            tx.transparent
                .inputs
                .iter()
                .map(move |inp| (inp.prev_txid, inp.prev_index, tx.txid))
        })
        .collect();

    let txid_locations: Vec<(TransactionHash, BlockHeight, u32)> = block
        .transactions
        .iter()
        .map(|tx| (tx.txid, height, tx.index))
        .collect();

    let transparent_txs: Vec<TransparentTxCompact> = block
        .transactions
        .iter()
        .map(|tx| TransparentTxCompact {
            inputs: tx
                .transparent
                .inputs
                .iter()
                .map(|inp| (inp.prev_txid, inp.prev_index))
                .collect(),
            outputs: tx
                .transparent
                .outputs
                .iter()
                .map(|out| (out.value, out.script.clone()))
                .collect(),
        })
        .collect();

    let sapling_txs: Vec<SaplingTxCompact> = block
        .transactions
        .iter()
        .map(|tx| SaplingTxCompact {
            nullifiers: tx.sapling.spends.iter().map(|s| s.nullifier).collect(),
            outputs: tx
                .sapling
                .outputs
                .iter()
                .map(|o| (o.cmu, o.ephemeral_key, o.enc_ciphertext.clone()))
                .collect(),
        })
        .collect();

    let orchard_txs: Vec<OrchardTxCompact> = block
        .transactions
        .iter()
        .map(|tx| OrchardTxCompact {
            actions: tx
                .orchard
                .actions
                .iter()
                .map(|a| (a.nullifier, a.cmx, a.ephemeral_key, a.enc_ciphertext.clone()))
                .collect(),
        })
        .collect();

    CurrentZainoContext {
        height,
        hash: block.header.hash,
        prev_hash: block.header.prev_hash,
        time: block.header.time,
        bits: block.header.bits,
        txids,
        spends,
        txid_locations,
        transparent_txs,
        sapling_txs,
        orchard_txs,
    }
}

// ---------------------------------------------------------------------------
// ProvideContext projections
// ---------------------------------------------------------------------------

impl ProvideContext<HeaderCtx> for CurrentZainoContext {
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

impl ProvideContext<TxidsCtx> for CurrentZainoContext {
    fn context(&self) -> TxidsCtx {
        TxidsCtx {
            height: self.height,
            txids: self.txids.clone(),
        }
    }
}

impl ProvideContext<HashToHeightCtx> for CurrentZainoContext {
    fn context(&self) -> HashToHeightCtx {
        HashToHeightCtx {
            hash: self.hash,
            height: self.height,
        }
    }
}

impl ProvideContext<SpendCtx> for CurrentZainoContext {
    fn context(&self) -> SpendCtx {
        SpendCtx {
            spends: self.spends.clone(),
        }
    }
}

impl ProvideContext<TxidLocationCtx> for CurrentZainoContext {
    fn context(&self) -> TxidLocationCtx {
        TxidLocationCtx {
            locations: self.txid_locations.clone(),
        }
    }
}

impl ProvideContext<TransparentDataCtx> for CurrentZainoContext {
    fn context(&self) -> TransparentDataCtx {
        TransparentDataCtx {
            height: self.height,
            txs: self.transparent_txs.clone(),
        }
    }
}

impl ProvideContext<SaplingCtx> for CurrentZainoContext {
    fn context(&self) -> SaplingCtx {
        SaplingCtx {
            height: self.height,
            txs: self.sapling_txs.clone(),
        }
    }
}

impl ProvideContext<OrchardCtx> for CurrentZainoContext {
    fn context(&self) -> OrchardCtx {
        OrchardCtx {
            height: self.height,
            txs: self.orchard_txs.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Index set builder
// ---------------------------------------------------------------------------

/// Build the full current-zaino index set (8 indexes).
pub fn index_set() -> IndexSet<CurrentZainoContext> {
    IndexSet::new()
        .with::<HeadersIndex>()
        .with::<TxidsIndex>()
        .with::<HashToHeightIndex>()
        .with::<TransparentSpendsIndex>()
        .with::<TxidLocationIndex>()
        .with::<TransparentDataIndex>()
        .with::<SaplingIndex>()
        .with::<OrchardIndex>()
}
