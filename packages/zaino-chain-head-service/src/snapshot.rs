//! A map-backed implementation of the ChainHead view.
//!
//! The graph's only stored edge is each block's parent hash. Everything else —
//! which blocks are tips, how far a branch is from the canonical chain, where a
//! transaction sits — is derived by walking that edge.
//!
//! The representation lives here rather than in `zaino-chain-head` on purpose:
//! `ChainHeadSnapshot` is a capability, and how the graph is stored is this
//! runtime's business. A future runtime holding the same graph in persistent
//! structures — sharing unchanged subtrees between snapshots instead of cloning
//! maps on every publish — implements the same trait, and no consumer notices.

use std::collections::{HashMap, HashSet};

use zaino_chain_head::{
    snapshot::{
        ChainHeadBlockIter, ChainHeadTransactionLocations, ChainHeadTransactionService,
        ChainHeadTxPosition, SpenderLocation,
    },
    ChainHeadBlock, ChainHeadEpoch, ChainHeadError, ChainHeadSnapshot,
};
use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    BlockHash, BlockRef, Height, Outpoint, TransactionId,
};

/// The retained graph, held in hash maps.
///
/// `blocks` holds every retained block, canonical and competing alike.
/// `heights_to_hashes` names which of them is canonical at each height, so a
/// block is on the best chain exactly when the map's entry for its height is
/// its own hash.
///
/// Fields are `pub(crate)` rather than public: the runtime that builds these
/// needs to write them, and nothing outside this crate should know they exist.
#[derive(Debug, Clone)]
pub struct MapBackedSnapshot {
    pub(crate) blocks: HashMap<BlockHash, ChainHeadBlock>,
    pub(crate) heights_to_hashes: HashMap<Height, BlockHash>,
    pub(crate) best_tip: BlockRef,
    /// Which publication this is, in the sense of [`ChainHeadEpoch`].
    ///
    /// Stamped by the writer at publish time rather than derived here: only the
    /// writer knows whether the tip moved, and the epoch advances on tip changes
    /// alone.
    pub(crate) generation: u64,
}

impl MapBackedSnapshot {
    /// How many blocks are retained, canonical and competing together.
    ///
    /// Inherent rather than on the port: it describes what this implementation
    /// is holding, not anything about the chain, so no consumer needs it.
    pub fn retained_block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Create initial snapshot from a single block
    pub(crate) fn from_initial_block(block: ChainHeadBlock) -> Self {
        let best_tip = block.reference;
        let hash = block.hash();
        let height = block.height();

        let mut blocks = HashMap::new();
        let mut heights_to_hashes = HashMap::new();

        blocks.insert(hash, block);
        heights_to_hashes.insert(height, hash);

        Self {
            blocks,
            heights_to_hashes,
            best_tip,
            generation: 0,
        }
    }

    pub(crate) fn add_block_new_chaintip(&mut self, block: ChainHeadBlock) {
        self.best_tip = block.reference;
        self.add_block(block)
    }

    pub(crate) fn remove_finalized_blocks(&mut self, finalized_height: Height) {
        let top_block_hash = match self
            .heights_to_hashes
            .iter()
            .max_by_key(|(height, _hash)| *height)
        {
            Some((_height, hash)) => *hash,
            // We have no blocks. There's nothing to remove
            None => return,
        };
        // Keep the last finalized block. This means we don't have to check
        // the finalized state when the entire non-finalized state is reorged away.
        // If all blocks are below the finalized height, keep the highest anyway,
        // so we don't need to re-connect the the finalized state to get chainwork, etc.
        self.blocks.retain(|_hash, block| {
            block.height() >= finalized_height || block.hash() == top_block_hash
        });
        self.heights_to_hashes
            .retain(|height, hash| height >= &finalized_height || hash == &top_block_hash);
    }

    fn add_block(&mut self, block: ChainHeadBlock) {
        self.heights_to_hashes.insert(block.height(), block.hash());
        self.blocks.insert(block.hash(), block);
    }

    /// How many blocks separate this tip from the canonical chain.
    ///
    /// Zero for a canonical block. When the walk leaves the window before
    /// reaching the canonical chain the count so far is returned, matching what
    /// the caller can actually observe.
    fn branch_len_to_best_chain(&self, block: &ChainHeadBlock) -> u32 {
        let mut branch_len = 0;
        let mut current = block;

        loop {
            if self.is_on_best_chain(current.reference) {
                return branch_len;
            }
            branch_len += 1;
            let Some(parent) = self.blocks.get(&current.parent_hash) else {
                return branch_len;
            };
            current = parent;
        }
    }
}

impl ChainHeadSnapshot for MapBackedSnapshot {
    fn best_tip(&self) -> BlockRef {
        self.best_tip
    }

    fn epoch(&self) -> ChainHeadEpoch {
        ChainHeadEpoch {
            generation: self.generation,
            best_tip: self.best_tip,
        }
    }

    fn block_by_hash(&self, hash: &BlockHash) -> Option<&ChainHeadBlock> {
        self.blocks.get(hash)
    }

    fn best_block_by_height(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.heights_to_hashes
            .get(&height)
            .and_then(|hash| self.blocks.get(hash))
    }

    fn is_on_best_chain(&self, block: BlockRef) -> bool {
        self.heights_to_hashes.get(&block.height) == Some(&block.hash)
    }

    fn find_fork_point(&self, hash: &BlockHash) -> Option<BlockRef> {
        let mut current = self.blocks.get(hash)?;
        loop {
            if self.is_on_best_chain(current.reference) {
                return Some(current.reference);
            }
            current = self.blocks.get(&current.parent_hash)?;
        }
    }

    /// A tip is a retained block that no other retained block claims as its
    /// parent. The canonical tip is always included, even in the degenerate
    /// case where the window holds a single block.
    ///
    /// zcashd enumerates block-tree leaves and reports inactive fully-known
    /// branches as `valid-fork`. ChainHead retains whole blocks, never
    /// headers-only or invalid candidates, so those two statuses are the only
    /// ones this can emit.
    fn chain_tips(&self) -> Vec<ChainTip> {
        let parent_hashes = self
            .blocks
            .values()
            .map(|block| block.parent_hash)
            .collect::<HashSet<_>>();

        let mut tip_hashes = self
            .blocks
            .keys()
            .filter(|hash| !parent_hashes.contains(hash))
            .copied()
            .collect::<HashSet<_>>();
        tip_hashes.insert(self.best_tip.hash);

        let mut tips = tip_hashes
            .into_iter()
            .filter_map(|hash| self.blocks.get(&hash))
            .map(|block| {
                let is_active_tip = block.hash() == self.best_tip.hash;
                ChainTip {
                    height: block.height(),
                    hash: block.hash(),
                    branch_len: if is_active_tip {
                        0
                    } else {
                        self.branch_len_to_best_chain(block)
                    },
                    status: if is_active_tip {
                        ChainTipStatus::Active
                    } else {
                        ChainTipStatus::ValidFork
                    },
                }
            })
            .collect::<Vec<_>>();

        // Descending height, then ascending hash. The tie-break compares
        // *display-order* bytes, which is the ordering the hex strings a client
        // sees would produce — hashes are byte-reversed for display, so sorting
        // internal bytes would silently reorder equal-height tips.
        tips.sort_by(|left, right| {
            let display_order = |hash: BlockHash| {
                let mut bytes = <[u8; 32]>::from(hash);
                bytes.reverse();
                bytes
            };
            right
                .height
                .cmp(&left.height)
                .then_with(|| display_order(left.hash).cmp(&display_order(right.hash)))
        });
        tips
    }

    fn best_chain(&self) -> ChainHeadBlockIter<'_> {
        // Sorted once per call: the height index is a hash map, and a caller
        // walking the chain to accumulate state needs the blocks in order.
        let mut heights: Vec<Height> = self.heights_to_hashes.keys().copied().collect();
        heights.sort_unstable();
        ChainHeadBlockIter::new(
            heights
                .into_iter()
                .filter_map(move |height| self.best_block_by_height(height)),
        )
    }

    fn best_chain_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> Result<ChainHeadBlockIter<'_>, ChainHeadError> {
        if start > end {
            return Err(ChainHeadError::InvalidRange { start, end });
        }

        let (start, end) = (u32::from(start), u32::from(end));
        Ok(ChainHeadBlockIter::new((start..=end).filter_map(
            move |height| {
                Height::try_from(height)
                    .ok()
                    .and_then(|height| self.best_block_by_height(height))
            },
        )))
    }
}

impl ChainHeadTransactionService for MapBackedSnapshot {
    /// A bounded scan of the window. The window is small and this is not on a
    /// hot path; when it becomes one, the answer is a `txid ->` position index
    /// carried alongside the graph, not a faster scan.
    fn transaction_locations(&self, txid: &TransactionId) -> ChainHeadTransactionLocations {
        let mut locations = ChainHeadTransactionLocations::default();

        for block in self.blocks.values() {
            let Some(transaction) = block
                .block
                .transactions
                .iter()
                .find(|transaction| &transaction.txid == txid)
            else {
                continue;
            };

            let position = ChainHeadTxPosition {
                block: block.reference,
                tx_index: transaction.index,
            };
            if self.is_on_best_chain(block.reference) {
                locations.best_chain = Some(position);
            } else {
                locations.non_best_chain.push(position);
            }
        }

        locations
    }

    /// Canonical spenders only: a spend on a competing branch is not a spend of
    /// the chain's UTXO set.
    ///
    /// One pass over the canonical blocks builds one map for the whole batch,
    /// so cost is independent of how many outpoints are asked about.
    fn outpoint_spenders(&self, outpoints: &[Outpoint]) -> Vec<Option<SpenderLocation>> {
        if outpoints.is_empty() {
            return Vec::new();
        }

        let mut spenders: HashMap<Outpoint, SpenderLocation> = HashMap::new();
        for hash in self.heights_to_hashes.values() {
            let Some(block) = self.blocks.get(hash) else {
                continue;
            };
            for transaction in &block.block.transactions {
                for input in &transaction.transparent.inputs {
                    spenders.insert(
                        Outpoint {
                            txid: input.prev_txid,
                            index: input.prev_index,
                        },
                        SpenderLocation {
                            block: block.reference,
                            txid: transaction.txid,
                            tx_index: transaction.index,
                        },
                    );
                }
            }
        }

        outpoints
            .iter()
            .map(|outpoint| spenders.get(outpoint).copied())
            .collect()
    }
}
