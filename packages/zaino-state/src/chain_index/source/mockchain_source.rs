//! Mock BlockchainSourceResult implementation.

use super::*;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use zebra_chain::{block::Block, orchard::tree as orchard, sapling::tree as sapling};
use zebra_state::HashOrHeight;

/// Build the txid → (height, tx) lookup map used by
/// [`MockchainSource::get_transaction`].
///
/// Each tx's `hash()` is computed once here (cryptographic cost) and
/// cached for the lifetime of the `MockchainSource`. First occurrence
/// wins if the same txid appears at multiple heights — matches the
/// original linear-scan behaviour (return on first match starting at
/// height 0).
fn build_txid_index(
    blocks: &[Arc<Block>],
) -> Arc<HashMap<zebra_chain::transaction::Hash, (usize, Arc<zebra_chain::transaction::Transaction>)>>
{
    let mut index = HashMap::new();
    for (height, block) in blocks.iter().enumerate() {
        for tx in &block.transactions {
            index
                .entry(tx.hash())
                .or_insert_with(|| (height, Arc::clone(tx)));
        }
    }
    Arc::new(index)
}

/// A test-only mock implementation of BlockchainReader using ordered lists by height.
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub(crate) struct MockchainSource {
    blocks: Vec<Arc<Block>>,
    roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
    treestates: Vec<(Vec<u8>, Vec<u8>)>,
    hashes: Vec<BlockHash>,
    /// txid → (block index, tx). Built once at construction; lets
    /// `get_transaction` run in O(1) instead of scanning every tx.
    /// Wrapped in `Arc` so cloning a `MockchainSource` is cheap.
    txid_index: Arc<
        HashMap<
            zebra_chain::transaction::Hash,
            (usize, Arc<zebra_chain::transaction::Transaction>),
        >,
    >,
    active_chain_height: Arc<AtomicU32>,
    force_requests_against_source_to_fail: Arc<std::sync::atomic::AtomicBool>,
}

impl MockchainSource {
    /// Creates a new MockchainSource.
    /// All inputs must be the same length, and ordered by ascending height starting from 0.
    #[allow(clippy::type_complexity)]
    pub(crate) fn new(
        blocks: Vec<Arc<Block>>,
        roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
        treestates: Vec<(Vec<u8>, Vec<u8>)>,
        hashes: Vec<BlockHash>,
    ) -> Self {
        assert!(
            blocks.len() == roots.len()
                && roots.len() == hashes.len()
                && hashes.len() == treestates.len(),
            "All input vectors must be the same length"
        );

        // len() returns one-indexed length, height is zero-indexed.
        let tip_height = blocks.len().saturating_sub(1) as u32;
        let txid_index = build_txid_index(&blocks);
        Self {
            blocks,
            roots,
            treestates,
            hashes,
            txid_index,
            active_chain_height: Arc::new(AtomicU32::new(tip_height)),
            force_requests_against_source_to_fail: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
        }
    }

    /// Creates a new MockchainSource, *with* an active chain height.
    ///
    /// Block will only be served up to the active chain height, with mempool data coming from
    /// the *next block in the chain.
    ///
    /// Blocks must be "mined" to extend the active chain height.
    ///
    /// All inputs must be the same length, and ordered by ascending height starting from 0.
    #[allow(clippy::type_complexity)]
    pub(crate) fn new_with_active_height(
        blocks: Vec<Arc<Block>>,
        roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
        treestates: Vec<(Vec<u8>, Vec<u8>)>,
        hashes: Vec<BlockHash>,
        active_chain_height: u32,
    ) -> Self {
        assert!(
            blocks.len() == roots.len()
                && roots.len() == hashes.len()
                && hashes.len() == treestates.len(),
            "All input vectors must be the same length"
        );

        // len() returns one-indexed length, height is zero-indexed.
        let max_height = blocks.len().saturating_sub(1) as u32;
        assert!(
            active_chain_height <= max_height,
            "active_chain_height must be in 0..=len-1"
        );

        let txid_index = build_txid_index(&blocks);
        Self {
            blocks,
            roots,
            treestates,
            hashes,
            txid_index,
            active_chain_height: Arc::new(AtomicU32::new(active_chain_height)),
            force_requests_against_source_to_fail: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
        }
    }

    /// When set to true, `get_best_block_height` and `get_best_block_hash`
    /// return `BlockchainSourceError::Unrecoverable`.
    pub(crate) fn set_failing(&self, fail: bool) {
        self.force_requests_against_source_to_fail
            .store(fail, Ordering::SeqCst);
    }

    pub(crate) fn mine_blocks(&self, blocks: u32) {
        // len() returns one-indexed length, height is zero-indexed.
        let max_height = self.max_chain_height();
        let _ =
            self.active_chain_height
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    let target = current.saturating_add(blocks).min(max_height);
                    if target == current {
                        None
                    } else {
                        Some(target)
                    }
                });
    }

    pub(crate) fn max_chain_height(&self) -> u32 {
        // len() returns one-indexed length, height is zero-indexed.
        self.blocks.len().saturating_sub(1) as u32
    }

    pub(crate) fn active_height(&self) -> u32 {
        self.active_chain_height.load(Ordering::SeqCst)
    }

    fn valid_height(&self, height: u32) -> Option<usize> {
        let active_chain_height = self.active_height() as usize;
        let valid_height = height as usize;

        if valid_height <= active_chain_height {
            Some(valid_height)
        } else {
            None
        }
    }

    fn valid_hash(&self, hash: &zebra_chain::block::Hash) -> Option<usize> {
        let active_chain_height = self.active_height() as usize;
        let height_index = self.hashes.iter().position(|h| h.0 == hash.0);

        if height_index.is_some() && height_index.unwrap() <= active_chain_height {
            height_index
        } else {
            None
        }
    }
}

#[async_trait]
impl BlockchainSource for MockchainSource {
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match id {
            HashOrHeight::Height(h) => {
                let Some(height_index) = self.valid_height(h.0) else {
                    return Ok(None);
                };
                Ok(Some(Arc::clone(&self.blocks[height_index])))
            }
            HashOrHeight::Hash(hash) => {
                let Some(hash_index) = self.valid_hash(&hash) else {
                    return Ok(None);
                };

                Ok(Some(Arc::clone(&self.blocks[hash_index])))
            }
        }
    }

    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        let active_chain_height = self.active_height() as usize; // serve up to active tip

        if let Some(height) = self.hashes.iter().position(|h| h == &id) {
            if height <= active_chain_height {
                Ok(self.roots[height])
            } else {
                Ok((None, None))
            }
        } else {
            Ok((None, None))
        }
    }

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let active_chain_height = self.active_height() as usize; // serve up to active tip

        if let Some(height) = self.hashes.iter().position(|h| h == &id) {
            if height <= active_chain_height {
                let (sapling_state, orchard_state) = &self.treestates[height];
                Ok((Some(sapling_state.clone()), Some(orchard_state.clone())))
            } else {
                Ok((None, None))
            }
        } else {
            Ok((None, None))
        }
    }

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        let mempool_height = self.active_height() as usize + 1;

        let txids = if mempool_height < self.blocks.len() {
            self.blocks[mempool_height]
                .transactions
                .iter()
                .filter(|tx| !tx.is_coinbase()) // <-- exclude coinbase
                .map(|tx| tx.hash())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        Ok(Some(txids))
    }

    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        let zebra_txid = zebra_chain::transaction::Hash::from(txid.0);
        let active_chain_height = self.active_height() as usize;
        let mempool_height = active_chain_height + 1;

        let Some((stored_height, tx)) = self.txid_index.get(&zebra_txid) else {
            return Ok(None);
        };

        if *stored_height <= active_chain_height {
            return Ok(Some((
                Arc::clone(tx),
                GetTransactionLocation::BestChain(zebra_chain::block::Height(
                    *stored_height as u32,
                )),
            )));
        }
        if *stored_height == mempool_height {
            return Ok(Some((Arc::clone(tx), GetTransactionLocation::Mempool)));
        }
        Ok(None)
    }

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        if self
            .force_requests_against_source_to_fail
            .load(Ordering::SeqCst)
        {
            return Err(BlockchainSourceError::Unrecoverable(
                "forced source failure".into(),
            ));
        }
        let active_chain_height = self.active_height() as usize;

        if self.blocks.is_empty() || active_chain_height > self.max_chain_height() as usize {
            return Ok(None);
        }

        Ok(Some(self.blocks[active_chain_height].hash()))
    }

    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        if self
            .force_requests_against_source_to_fail
            .load(Ordering::SeqCst)
        {
            return Err(BlockchainSourceError::Unrecoverable(
                "forced source failure".into(),
            ));
        }
        let active_chain_height = self.active_height() as usize;

        if self.blocks.is_empty() || active_chain_height > self.max_chain_height() as usize {
            return Ok(None);
        }

        Ok(Some(
            self.blocks[active_chain_height].coinbase_height().unwrap(),
        ))
    }

    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        Ok(None)
    }

    async fn get_subtree_roots(
        &self,
        _pool: ShieldedPool,
        _start_index: u16,
        _max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        todo!()
    }
}
