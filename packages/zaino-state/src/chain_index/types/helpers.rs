//! Helper types for chain index operations.
//!
//! This module contains non-database types used for in-memory operations,
//! conversions, and coordination between database types. These types do NOT
//! implement `ZainoVersionedSerde` and are not persisted to disk.
//!
//! Types in this module:
//! - BestChainLocation - Transaction location in best chain
//! - NonBestChainLocation - Transaction location not in best chain
//! - TreeRootData - Commitment tree roots wrapper
//! - BlockMetadata - Block metadata for construction
//! - BlockWithMetadata - Block with associated metadata

use super::db::legacy::*;
use crate::chain_index::types::{BlockContext, ChainWork, CompactDifficulty};

/// Selects how far [`ChainIndex::get_outpoint_spenders`] searches for a spend.
///
/// [`ChainIndex::get_outpoint_spenders`]: crate::chain_index::ChainIndex::get_outpoint_spenders
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainScope {
    /// Finalised DB only. Reorg-stable: never reports a spend that lives only in a
    /// non-finalised block.
    Finalised,
    /// Non-finalised best chain first, then the finalised DB. Reports the latest known
    /// spend but may include spends that a reorg could roll back.
    FullChain,
}

/// The location of a transaction in the best chain
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum BestChainLocation {
    /// the block containing the transaction
    Block(BlockHash, Height),
    /// If the transaction is in the mempool and the mempool
    /// matches the snapshot's chaintip
    /// Return the target height, which is known to be a block above
    /// the provided snapshot's chaintip and is returned for convenience
    Mempool(Height),
}

/// The location of a transaction not in the best chain
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum NonBestChainLocation {
    /// the block containing the transaction
    Block(BlockHash, Height),
    /// if the transaction is in the mempool
    /// but the mempool does not match the
    /// snapshot's chaintip, return the target height if known
    ///
    /// This likely means that the provided
    /// snapshot is out-of-date
    Mempool(Option<Height>),
}

/// Wrapper for optional commitment tree roots from blockchain source
#[derive(Clone)]
pub struct TreeRootData {
    /// Sapling tree root and size
    pub sapling: Option<(zebra_chain::sapling::tree::Root, u64)>,
    /// Orchard tree root and size
    pub orchard: Option<(zebra_chain::orchard::tree::Root, u64)>,
    /// Orchard tree root and size
    pub ironwood: Option<(zebra_chain::orchard::tree::Root, u64)>,
}

impl TreeRootData {
    /// Create new tree root data
    pub fn new(
        sapling: Option<(zebra_chain::sapling::tree::Root, u64)>,
        orchard: Option<(zebra_chain::orchard::tree::Root, u64)>,
        ironwood: Option<(zebra_chain::orchard::tree::Root, u64)>,
    ) -> Self {
        Self {
            sapling,
            orchard,
            ironwood,
        }
    }

    /// Extract with defaults for genesis/sync use case
    ///
    /// Sapling and orchard roots default when absent (genesis). The ironwood component
    /// passes through unchanged: `None` means the block has no ironwood treestate and
    /// must be stored as `None`.
    pub fn extract_with_defaults(
        self,
    ) -> (
        zebra_chain::sapling::tree::Root,
        u64,
        zebra_chain::orchard::tree::Root,
        u64,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    ) {
        let (sapling_root, sapling_size) = self.sapling.unwrap_or_default();
        let (orchard_root, orchard_size) = self.orchard.unwrap_or_default();
        (
            sapling_root,
            sapling_size,
            orchard_root,
            orchard_size,
            self.ironwood,
        )
    }
}

/// Intermediate type to hold block metadata separate from the block itself
#[derive(Debug, Clone)]
pub struct BlockMetadata {
    /// Sapling commitment tree root
    pub sapling_root: zebra_chain::sapling::tree::Root,
    /// Sapling tree size
    pub sapling_size: u32,
    /// Orchard commitment tree root
    pub orchard_root: zebra_chain::orchard::tree::Root,
    /// Orchard tree size
    pub orchard_size: u32,
    /// Ironwood commitment tree root and size; `None` when the block has no ironwood
    /// treestate (below NU6.3 activation, or a network with no NU6.3 activation height)
    pub ironwood: Option<(zebra_chain::orchard::tree::Root, u32)>,
    /// Parent block's chainwork (`None` for genesis).
    pub parent_chainwork: Option<ChainWork>,
    /// Network for block validation
    pub network: zebra_chain::parameters::Network,
}

impl BlockMetadata {
    /// Create new block metadata
    pub fn new(
        sapling_root: zebra_chain::sapling::tree::Root,
        sapling_size: u32,
        orchard_root: zebra_chain::orchard::tree::Root,
        orchard_size: u32,
        ironwood: Option<(zebra_chain::orchard::tree::Root, u32)>,
        parent_chainwork: Option<ChainWork>,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        Self {
            sapling_root,
            sapling_size,
            orchard_root,
            orchard_size,
            ironwood,
            parent_chainwork,
            network,
        }
    }
}

/// Intermediate type combining a block with its metadata
#[derive(Debug, Clone)]
pub struct BlockWithMetadata<'a> {
    /// The zebra block
    pub block: &'a zebra_chain::block::Block,
    /// Additional metadata needed for IndexedBlock creation
    pub metadata: BlockMetadata,
}

impl<'a> BlockWithMetadata<'a> {
    /// Create a new block with metadata
    pub fn new(block: &'a zebra_chain::block::Block, metadata: BlockMetadata) -> Self {
        Self { block, metadata }
    }

    /// Extract block header data
    fn extract_block_data(&self) -> Result<BlockData, String> {
        let block = self.block;
        let network = &self.metadata.network;

        let bits = CompactDifficulty::try_from_be_bytes(
            block.header.difficulty_threshold.bytes_in_display_order(),
        )
        .map_err(|e| format!("invalid nBits: {e}"))?;

        Ok(BlockData {
            version: block.header.version,
            time: block.header.time.timestamp(),
            merkle_root: block.header.merkle_root.0,
            bits,
            block_commitments: BlockData::commitment_to_bytes(
                block
                    .commitment(network)
                    .map_err(|_| "Block commitment could not be computed".to_string())?,
            ),
            nonce: *block.header.nonce,
            solution: block.header.solution.into(),
        })
    }

    /// Extract and process all transactions in the block
    fn extract_transactions(&self) -> Result<Vec<CompactTxData>, String> {
        let mut transactions = Vec::new();

        for (i, txn) in self.block.transactions.iter().enumerate() {
            let transparent = self.extract_transparent_data(txn)?;
            let sapling = self.extract_sapling_data(txn);
            let orchard = self.extract_orchard_data(txn);
            let ironwood = self.extract_ironwood_data(txn);

            let txdata = CompactTxData::new(
                i as u64,
                txn.hash().into(),
                transparent,
                sapling,
                orchard,
                ironwood,
            );
            transactions.push(txdata);
        }

        Ok(transactions)
    }

    /// Extract transparent transaction data (inputs and outputs)
    fn extract_transparent_data(
        &self,
        txn: &zebra_chain::transaction::Transaction,
    ) -> Result<TransparentCompactTx, String> {
        let inputs: Vec<TxInCompact> = txn
            .inputs()
            .iter()
            .map(|input| match input.outpoint() {
                Some(outpoint) => TxInCompact::new(outpoint.hash.0, outpoint.index),
                None => TxInCompact::null_prevout(),
            })
            .collect();

        let outputs = txn
            .outputs()
            .iter()
            .map(|output| {
                let value = u64::from(output.value);
                let script_bytes = output.lock_script.as_raw_bytes();

                let addr = AddrScript::from_script(script_bytes).unwrap_or_else(|| {
                    let mut fallback = [0u8; 20];
                    let usable = script_bytes.len().min(20);
                    fallback[..usable].copy_from_slice(&script_bytes[..usable]);
                    AddrScript::new(fallback, ScriptType::NonStandard as u8)
                });

                TxOutCompact::new(value, *addr.hash(), addr.script_type())
                    .ok_or_else(|| "TxOutCompact conversion failed".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(TransparentCompactTx::new(inputs, outputs))
    }

    /// Returns the first 52 bytes of a 580-byte encrypted note ciphertext: the prefix a
    /// compact block carries, sufficient for trial decryption.
    fn compact_ciphertext_prefix(enc_ciphertext: [u8; 580]) -> [u8; 52] {
        std::array::from_fn(|i| enc_ciphertext[i])
    }

    /// Builds the Orchard-shaped compact transaction shared by the Orchard and Ironwood
    /// pools from a value balance and the pool's actions.
    fn extract_orchard_shaped_data<'t>(
        value_balance: i64,
        actions: impl Iterator<Item = &'t zebra_chain::orchard::Action>,
    ) -> OrchardCompactTx {
        OrchardCompactTx::new(
            (value_balance != 0).then_some(value_balance),
            actions
                .map(|action| {
                    CompactOrchardAction::new(
                        <[u8; 32]>::from(action.nullifier),
                        <[u8; 32]>::from(action.cm_x),
                        <[u8; 32]>::from(action.ephemeral_key),
                        Self::compact_ciphertext_prefix(<[u8; 580]>::from(action.enc_ciphertext)),
                    )
                })
                .collect(),
        )
    }

    /// Extract sapling transaction data
    fn extract_sapling_data(
        &self,
        txn: &zebra_chain::transaction::Transaction,
    ) -> SaplingCompactTx {
        let sapling_value = {
            let value = i64::from(txn.sapling_value_balance().sapling_amount());
            (value != 0).then_some(value)
        };

        SaplingCompactTx::new(
            sapling_value,
            txn.sapling_nullifiers()
                .map(|nf| CompactSaplingSpend::new(*nf.0))
                .collect(),
            txn.sapling_outputs()
                .map(|output| {
                    CompactSaplingOutput::new(
                        output.cm_u.to_bytes(),
                        <[u8; 32]>::from(output.ephemeral_key),
                        Self::compact_ciphertext_prefix(<[u8; 580]>::from(output.enc_ciphertext)),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Extract orchard transaction data
    fn extract_orchard_data(
        &self,
        txn: &zebra_chain::transaction::Transaction,
    ) -> OrchardCompactTx {
        Self::extract_orchard_shaped_data(
            i64::from(txn.orchard_value_balance().orchard_amount()),
            txn.orchard_actions(),
        )
    }

    /// Extract ironwood transaction data
    fn extract_ironwood_data(
        &self,
        txn: &zebra_chain::transaction::Transaction,
    ) -> OrchardCompactTx {
        Self::extract_orchard_shaped_data(
            i64::from(txn.ironwood_value_balance().ironwood_amount()),
            txn.ironwood_actions(),
        )
    }

    /// Create a [`BlockContext`] from block and metadata.
    fn create_block_context(&self) -> Result<BlockContext, String> {
        let block = self.block;
        let hash = BlockHash::from(block.hash());
        let parent_hash = BlockHash::from(block.header.previous_block_hash);
        let height = block
            .coinbase_height()
            .map(|height| Height(height.0))
            .ok_or_else(|| String::from("Any valid block has a coinbase height"))?;

        let bits = CompactDifficulty::try_from_be_bytes(
            block.header.difficulty_threshold.bytes_in_display_order(),
        )
        .map_err(|e| format!("invalid nBits: {e}"))?;
        let block_work = bits.to_work();
        let chainwork = match self.metadata.parent_chainwork {
            Some(parent) => parent
                .add(&block_work)
                .map_err(|e| format!("chainwork overflow: {e}"))?,
            None => block_work,
        };

        Ok(BlockContext::new(hash, parent_hash, chainwork, height))
    }
}

impl BlockMetadata {
    /// Create the stored commitment tree data for this block's metadata.
    fn create_commitment_tree_data(&self) -> super::db::CommitmentTreeData {
        let commitment_tree_roots = super::db::CommitmentTreeRoots::new(
            <[u8; 32]>::from(self.sapling_root),
            <[u8; 32]>::from(self.orchard_root),
            self.ironwood.map(|(root, _)| <[u8; 32]>::from(root)),
        );

        let commitment_tree_size = super::db::CommitmentTreeSizes::new(
            self.sapling_size,
            self.orchard_size,
            self.ironwood.map_or(0, |(_, size)| size),
        );

        super::db::CommitmentTreeData::new(commitment_tree_roots, commitment_tree_size)
    }
}

// Clean TryFrom implementation using the intermediate types
impl TryFrom<BlockWithMetadata<'_>> for IndexedBlock {
    type Error = String;

    fn try_from(block_with_metadata: BlockWithMetadata<'_>) -> Result<Self, Self::Error> {
        let data = block_with_metadata.extract_block_data()?;
        let transactions = block_with_metadata.extract_transactions()?;
        let context = block_with_metadata.create_block_context()?;
        let commitment_tree_data = block_with_metadata.metadata.create_commitment_tree_data();

        Ok(IndexedBlock {
            context,
            data,
            transactions,
            commitment_tree_data,
        })
    }
}

#[cfg(test)]
mod create_commitment_tree_data {
    use super::*;

    /// Regression test: a block whose source reported no ironwood treestate (pre-NU6.3,
    /// or a network with no NU6.3 activation height) must store its ironwood root as
    /// `None` — the encoding the v1.2.1->v1.3.0 migration and the CommitmentTreeRoots V1
    /// decode produce for the same state. The write path instead erased the `Option` via
    /// `extract_with_defaults` and stored `Some([0; 32])`, so a freshly synced database
    /// and a migrated database encoded identical pre-activation heights differently.
    ///
    #[test]
    fn absent_ironwood_root_is_stored_as_none() {
        let (sapling_root, sapling_size, orchard_root, orchard_size, ironwood) =
            TreeRootData::new(None, None, None).extract_with_defaults();
        let metadata = BlockMetadata::new(
            sapling_root,
            sapling_size as u32,
            orchard_root,
            orchard_size as u32,
            ironwood.map(|(root, size)| (root, size as u32)),
            None,
            zebra_chain::parameters::Network::Mainnet,
        );

        let commitment_tree_data = metadata.create_commitment_tree_data();

        assert_eq!(
            commitment_tree_data.roots().ironwood(),
            &None,
            "ironwood root must be stored as None when the source reported no ironwood treestate"
        );
    }
}
