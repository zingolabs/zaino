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

use primitive_types::U256;

use super::db::legacy::*;
use crate::ChainWork;

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
    // TODO: in this case, returning a consensus branch
    // ID would be useful
    Block(BlockHash),
    /// if the transaction is in the mempool
    /// but the mempool does not match the
    /// snapshot's chaintip, return the target height if known
    ///
    /// This likely means that the provided
    /// snapshot is out-of-date
    Mempool(Option<Height>),
}

impl TryFrom<&IndexedBlock> for NonBestChainLocation {
    type Error = ();

    fn try_from(value: &IndexedBlock) -> Result<Self, Self::Error> {
        match value.height() {
            Some(_) => Err(()),
            None => Ok(NonBestChainLocation::Block(*value.hash())),
        }
    }
}
impl TryFrom<&IndexedBlock> for BestChainLocation {
    type Error = ();

    fn try_from(value: &IndexedBlock) -> Result<Self, Self::Error> {
        match value.height() {
            None => Err(()),
            Some(height) => Ok(BestChainLocation::Block(*value.hash(), height)),
        }
    }
}

/// Wrapper for optional commitment tree roots from blockchain source
#[derive(Clone)]
pub struct TreeRootData {
    /// Sapling tree root and size
    pub sapling: Option<(zebra_chain::sapling::tree::Root, u64)>,
    /// Orchard tree root and size
    pub orchard: Option<(zebra_chain::orchard::tree::Root, u64)>,
}

impl TreeRootData {
    /// Create new tree root data
    pub fn new(
        sapling: Option<(zebra_chain::sapling::tree::Root, u64)>,
        orchard: Option<(zebra_chain::orchard::tree::Root, u64)>,
    ) -> Self {
        Self { sapling, orchard }
    }

    /// Extract with defaults for genesis/sync use case
    pub fn extract_with_defaults(
        self,
    ) -> (
        zebra_chain::sapling::tree::Root,
        u64,
        zebra_chain::orchard::tree::Root,
        u64,
    ) {
        let (sapling_root, sapling_size) = self.sapling.unwrap_or_default();
        let (orchard_root, orchard_size) = self.orchard.unwrap_or_default();
        (sapling_root, sapling_size, orchard_root, orchard_size)
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
    /// Parent block's chainwork
    pub parent_chainwork: ChainWork,
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
        parent_chainwork: ChainWork,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        Self {
            sapling_root,
            sapling_size,
            orchard_root,
            orchard_size,
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

        Ok(BlockData {
            version: block.header.version,
            time: block.header.time.timestamp(),
            merkle_root: block.header.merkle_root.0,
            bits: u32::from_be_bytes(block.header.difficulty_threshold.bytes_in_display_order()),
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

            let txdata =
                CompactTxData::new(i as u64, txn.hash().into(), transparent, sapling, orchard);
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

    /// Extract sapling transaction data
    fn extract_sapling_data(
        &self,
        txn: &zebra_chain::transaction::Transaction,
    ) -> SaplingCompactTx {
        let sapling_value = {
            let val = txn.sapling_value_balance().sapling_amount();
            if val == 0 {
                None
            } else {
                Some(i64::from(val))
            }
        };

        SaplingCompactTx::new(
            sapling_value,
            txn.sapling_nullifiers()
                .map(|nf| CompactSaplingSpend::new(*nf.0))
                .collect(),
            txn.sapling_outputs()
                .map(|output| {
                    let cipher: [u8; 52] = <[u8; 580]>::from(output.enc_ciphertext)[..52]
                        .try_into()
                        .unwrap(); // TODO: Remove unwrap
                    CompactSaplingOutput::new(
                        output.cm_u.to_bytes(),
                        <[u8; 32]>::from(output.ephemeral_key),
                        cipher,
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
        let orchard_value = {
            let val = txn.orchard_value_balance().orchard_amount();
            if val == 0 {
                None
            } else {
                Some(i64::from(val))
            }
        };

        OrchardCompactTx::new(
            orchard_value,
            txn.orchard_actions()
                .map(|action| {
                    let cipher: [u8; 52] = <[u8; 580]>::from(action.enc_ciphertext)[..52]
                        .try_into()
                        .unwrap(); // TODO: Remove unwrap
                    CompactOrchardAction::new(
                        <[u8; 32]>::from(action.nullifier),
                        <[u8; 32]>::from(action.cm_x),
                        <[u8; 32]>::from(action.ephemeral_key),
                        cipher,
                    )
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Create block index from block and metadata
    fn create_block_index(&self) -> Result<BlockIndex, String> {
        let block = self.block;
        let hash = BlockHash::from(block.hash());
        let parent_hash = BlockHash::from(block.header.previous_block_hash);
        let height = block.coinbase_height().map(|height| Height(height.0));

        let block_work = block.header.difficulty_threshold.to_work().ok_or_else(|| {
            "Failed to calculate block work from difficulty threshold".to_string()
        })?;
        let chainwork = self
            .metadata
            .parent_chainwork
            .add(&ChainWork::from(U256::from(block_work.as_u128())));

        Ok(BlockIndex {
            hash,
            parent_hash,
            chainwork,
            height,
        })
    }

    /// Create commitment tree data from metadata
    fn create_commitment_tree_data(&self) -> super::db::CommitmentTreeData {
        let commitment_tree_roots = super::db::CommitmentTreeRoots::new(
            <[u8; 32]>::from(self.metadata.sapling_root),
            <[u8; 32]>::from(self.metadata.orchard_root),
        );

        let commitment_tree_size = super::db::CommitmentTreeSizes::new(
            self.metadata.sapling_size,
            self.metadata.orchard_size,
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
        let index = block_with_metadata.create_block_index()?;
        let commitment_tree_data = block_with_metadata.create_commitment_tree_data();

        Ok(IndexedBlock {
            index,
            data,
            transactions,
            commitment_tree_data,
        })
    }
}
