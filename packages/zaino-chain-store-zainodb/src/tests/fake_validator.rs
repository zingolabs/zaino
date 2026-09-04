//! A validator that answers from a fixed chain, for this crate's own tests.
//!
//! # Why not the mockchain
//!
//! `zaino-state`'s `MockchainSource` implements the whole thirty-question
//! `ChainIndexSourcePorts` surface and is wired through `ValidatorSource` so
//! ChainIndex's own conversions run in its tests. None of that is relevant
//! here: this crate asks a validator four questions, and a fake that answers
//! thirty is thirty chances for a test to depend on something the store does
//! not use.
//!
//! It is also what keeps the dependency honest. If this fake ever has to grow a
//! method, the store has started asking a new question, and
//! [`zaino_chain_store::ChainStoreSource`] has to say so.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use zaino_primitives::types::{
    Block, BlockHash, Height, TransactionId, TransactionLocation, TreeRoots,
};
use zaino_source::{
    GetBestBlockHeightError, GetBlockByHashError, GetBlockError, GetCommitmentTreeRootsError,
    GetTransactionError, OneShotGetBestBlockHeight, OneShotGetBlock, OneShotGetBlockByHash,
    OneShotGetCommitmentTreeRoots, OneShotGetTransaction, QueryError, TransactionResponse,
};

/// One block as the fake holds it: the block itself and the treestate after it.
///
/// Paired rather than kept in two lists, because a root belongs to a block and
/// a fake that let them fall out of step would produce a chain no validator
/// could produce.
#[derive(Debug, Clone)]
pub(crate) struct FakeBlock {
    /// The block.
    pub(crate) block: Block,
    /// The commitment tree roots *after* it is applied.
    pub(crate) tree_roots: TreeRoots,
}

/// A validator serving a fixed chain from memory.
#[derive(Debug, Clone)]
pub(crate) struct FakeValidator {
    /// Blocks in height order, starting at genesis.
    blocks: Vec<FakeBlock>,
    /// Hash to index, so a lookup by hash is not a scan.
    by_hash: HashMap<BlockHash, usize>,
    /// Where each transaction is, so `get_transaction` can answer without a
    /// scan. The store's passthrough mode asks this per transaction.
    by_txid: HashMap<TransactionId, Height>,
    /// The height this validator reports as its tip.
    ///
    /// Separate from the number of blocks held: a tip below the loaded blocks
    /// models a validator that has not caught up, which is what lets a test
    /// move the store's finalised seam by raising it.
    ///
    /// Atomic, and therefore raisable through a shared reference: the store
    /// holds its source behind an `Arc`, so a test that has handed one over has
    /// no `&mut` left to raise the tip with. The mock this replaced took the
    /// same approach for the same reason.
    tip: Arc<AtomicU32>,
}

impl FakeValidator {
    /// A validator holding `blocks`, reporting the last one as its tip.
    pub(crate) fn new(blocks: Vec<FakeBlock>) -> Self {
        let tip = blocks
            .last()
            .map(|last| last.block.header.height)
            .unwrap_or_else(|| Height::try_from(0).expect("zero is a valid height"));
        Self::with_tip(blocks, tip)
    }

    /// A validator holding `blocks` but reporting `tip` as its chain tip.
    ///
    /// Panics if `tip` names a block that is not held: a validator that reports
    /// a tip it cannot serve is not a case the store is built to survive, and a
    /// test asserting against one would be asserting about a chain that cannot
    /// exist.
    pub(crate) fn with_tip(blocks: Vec<FakeBlock>, tip: Height) -> Self {
        let by_hash = blocks
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.block.header.hash, index))
            .collect();

        let by_txid = blocks
            .iter()
            .flat_map(|entry| {
                let height = entry.block.header.height;
                entry
                    .block
                    .transactions
                    .iter()
                    .map(move |tx| (tx.txid, height))
            })
            .collect();

        assert!(
            blocks.is_empty() || u32::from(tip) < blocks.len() as u32,
            "fake validator asked to report a tip it does not hold",
        );

        Self {
            blocks,
            by_hash,
            by_txid,
            tip: Arc::new(AtomicU32::new(u32::from(tip))),
        }
    }

    /// The highest block this validator holds, whatever tip it reports.
    ///
    /// Only the migration suites move a validator's tip, and those compile out
    /// when the experimental address history is on (see `migrations.rs`).
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    pub(crate) fn loaded_height(&self) -> Height {
        self.blocks
            .last()
            .map(|last| last.block.header.height)
            .unwrap_or_else(|| Height::try_from(0).expect("zero is a valid height"))
    }

    /// The tip this validator currently reports.
    pub(crate) fn reported_tip(&self) -> Height {
        Height::try_from(self.tip.load(Ordering::Acquire)).expect("tip is a held height")
    }

    /// Advances the reported tip by `blocks`, up to what is held.
    ///
    /// How a test moves the finalised seam: the store syncs to a floor derived
    /// from the tip, so raising the tip is what gives it more to do.
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    pub(crate) fn advance_tip(&self, blocks: u32) {
        let raised = self.tip.load(Ordering::Acquire).saturating_add(blocks);
        let capped = raised.min(u32::from(self.loaded_height()));
        self.tip.store(capped, Ordering::Release);
    }

    fn at(&self, height: Height) -> Option<&FakeBlock> {
        self.blocks.get(u32::from(height) as usize)
    }
}

impl OneShotGetBestBlockHeight for FakeValidator {
    async fn get_best_block_height(&self) -> Result<Height, QueryError<GetBestBlockHeightError>> {
        if self.blocks.is_empty() {
            return Err(QueryError::Domain(GetBestBlockHeightError::NotReady));
        }
        Ok(self.reported_tip())
    }
}

impl OneShotGetBlock for FakeValidator {
    async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
        self.at(height)
            .map(|entry| entry.block.clone())
            .ok_or(QueryError::Domain(GetBlockError::HeightNotFound(height)))
    }
}

impl OneShotGetBlockByHash for FakeValidator {
    async fn get_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        self.by_hash
            .get(&hash)
            .and_then(|index| self.blocks.get(*index))
            .map(|entry| entry.block.clone())
            .ok_or(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
    }
}

impl OneShotGetCommitmentTreeRoots for FakeValidator {
    async fn get_commitment_tree_roots(
        &self,
        block: BlockHash,
    ) -> Result<TreeRoots, QueryError<GetCommitmentTreeRootsError>> {
        self.by_hash
            .get(&block)
            .and_then(|index| self.blocks.get(*index))
            .map(|entry| entry.tree_roots.clone())
            .ok_or(QueryError::Domain(
                GetCommitmentTreeRootsError::BlockNotFound(block),
            ))
    }
}

impl OneShotGetTransaction for FakeValidator {
    /// Where a transaction is, and nothing useful in `bytes`.
    ///
    /// The store's only caller reads the location and discards the bytes — it
    /// is resolving a txid to a height — so serving empty bytes keeps the fake
    /// from implying it can produce consensus encodings, which the vectors it
    /// is built from do not carry once parsed.
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        self.by_txid
            .get(&txid)
            .map(|height| TransactionResponse {
                bytes: Vec::new(),
                location: TransactionLocation::BestChain(*height),
            })
            .ok_or(QueryError::Domain(GetTransactionError::NotFound(txid)))
    }
}

/// The vector chain, as a validator serving it.
///
/// Converts each recorded block into the domain shape the store's source port
/// yields, pairing it with the treestate recorded for it. The sizes come from
/// the same record as the roots, so the treestate a block is paired with is the
/// one that was measured after it.
pub(crate) fn fake_validator_from_vectors(
    blocks: &[super::vectors::VectorBlock],
) -> Arc<FakeValidator> {
    Arc::new(FakeValidator::new(fake_blocks_from_vectors(blocks)))
}

/// As [`fake_validator_from_vectors`], but reporting `tip` as the chain tip.
///
/// For suites that need the validator's chain to extend past what the store has
/// built, which is what makes the finalised seam move.
#[cfg(not(feature = "transparent_address_history_experimental"))]
pub(crate) fn fake_validator_with_tip(
    blocks: &[super::vectors::VectorBlock],
    tip: u32,
) -> Arc<FakeValidator> {
    Arc::new(FakeValidator::with_tip(
        fake_blocks_from_vectors(blocks),
        Height::try_from(tip).expect("test tip is a valid height"),
    ))
}

fn fake_blocks_from_vectors(blocks: &[super::vectors::VectorBlock]) -> Vec<FakeBlock> {
    blocks
        .iter()
        .map(|vector| {
            let block = zaino_convert_zebra::block_from_zebra(
                &vector.zebra_block,
                zaino_primitives::types::ChainMetadata {
                    sapling_tree_size: vector.sapling_tree_size as u32,
                    orchard_tree_size: vector.orchard_tree_size as u32,
                    ironwood_tree_size: 0,
                },
            )
            .expect("vector blocks convert to the domain shape");

            FakeBlock {
                block,
                tree_roots: TreeRoots {
                    sapling: Some(zaino_primitives::types::TreeRootInfo {
                        root: <[u8; 32]>::from(vector.sapling_root).into(),
                        size: vector.sapling_tree_size,
                    }),
                    orchard: Some(zaino_primitives::types::TreeRootInfo {
                        root: <[u8; 32]>::from(vector.orchard_root).into(),
                        size: vector.orchard_tree_size,
                    }),
                    // The vector chain predates NU6.3, so no block in it has an
                    // ironwood treestate. `None`, not a zero root: the two are
                    // distinct on disk and the store must write the former.
                    ironwood: None,
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake satisfies the port the store is written against.
    ///
    /// The point of the fake: if this stops compiling, the store has started
    /// asking a question `ChainStoreSource` does not list.
    #[test]
    fn the_fake_satisfies_the_store_source_bound() {
        fn assert_satisfied<T: zaino_chain_store::ChainStoreSource>() {}
        assert_satisfied::<FakeValidator>();
    }
}
