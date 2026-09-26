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
    ChainHeadBlock, ChainHeadError, ChainHeadSnapshot,
};
use zaino_primitives::types::{
    rpc::{ChainTip, ChainTipStatus},
    BlockHash, BlockRef, ChainStateEpoch, Height, Outpoint, TransactionId, TxIndex,
};

use crate::graph::{ChainGraph, NotChildOfTip, NotOnBestChain};

#[cfg(test)]
mod tests;

/// A transaction's block-order position, as a [`TxIndex`].
///
/// The slot is a `usize` from iterating the block's transactions; the position
/// type is `u32`. A block's transaction count is bounded well below `u32::MAX`
/// by the consensus block-size limit, so the narrowing cannot fail for any real
/// block — the `expect` names that invariant rather than asserting a hope.
fn tx_index(position: usize) -> TxIndex {
    TxIndex::try_from(position)
        .expect("a block's transaction count fits TxIndex; consensus bounds it below u32::MAX")
}

/// The retained graph: its tip, and every other retained block in a hash map.
///
/// `retained = {tip} ⊎ others`
///
/// `heights_to_hashes` names the canonical block at each height, so a block is
/// on the best chain exactly when the entry for its height is its own hash.
///
/// The tip is a field rather than a map entry, so the graph is never empty and
/// `tip_block` always has an answer. The fields are private. Besides the
/// `ChainGraph` invariants, the methods here keep `others` free of the tip's
/// hash.
#[derive(Debug, Clone)]
pub struct MapBackedSnapshot {
    tip: ChainHeadBlock,
    others: HashMap<BlockHash, ChainHeadBlock>,
    heights_to_hashes: HashMap<Height, BlockHash>,
    /// The block this graph's work is measured from, recorded at anchoring because retention prunes that block while every surviving block's work still counts from it.
    work_anchor: BlockRef,
    /// Which publication this is, in the sense of [`ChainStateEpoch`].
    ///
    /// Private even to the rest of this crate, and written only by
    /// [`stamp_generation`](Self::stamp_generation): the writer decides when to
    /// publish, but the rule for what generation a publication carries is the
    /// snapshot's own, and a field the writer could assign is a field the writer
    /// could assign wrongly.
    generation: u64,
}

impl MapBackedSnapshot {
    /// How many blocks are retained, canonical and competing together.
    ///
    /// Inherent rather than on the port: it describes what this implementation
    /// is holding, not anything about the chain, so no consumer needs it.
    pub fn retained_block_count(&self) -> usize {
        self.blocks().count()
    }

    /// Every retained block, the tip first.
    fn blocks(&self) -> impl Iterator<Item = &ChainHeadBlock> {
        std::iter::once(&self.tip).chain(self.others.values())
    }

    /// The lowest canonical height this snapshot retains, which is its anchor after a re-anchor and its trim floor otherwise.
    pub(crate) fn lowest_retained_height(&self) -> Height {
        self.heights_to_hashes
            .keys()
            .min()
            .copied()
            .unwrap_or(self.tip.height())
    }

    /// Makes `block` the tip; the previous tip becomes an ordinary retained
    /// block. `block` may already be retained, in which case it moves out of
    /// `others`.
    fn replace_tip(&mut self, block: ChainHeadBlock) {
        self.others.remove(&block.hash());
        let previous = std::mem::replace(&mut self.tip, block);
        if previous.hash() != self.tip.hash() {
            self.others.insert(previous.hash(), previous);
        }
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
            let Some(parent) = self.block_by_hash(&current.parent_hash) else {
                return branch_len;
            };
            current = parent;
        }
    }
}

impl ChainGraph for MapBackedSnapshot {
    fn from_initial_block(block: ChainHeadBlock) -> Self {
        let heights_to_hashes = HashMap::from([(block.height(), block.hash())]);
        Self {
            work_anchor: block.reference,
            tip: block,
            others: HashMap::new(),
            heights_to_hashes,
            generation: 0,
        }
    }

    fn tip_block(&self) -> &ChainHeadBlock {
        &self.tip
    }

    /// Folds over `others` from the tip, replacing it only on strictly more
    /// work.
    fn heaviest_block(&self) -> &ChainHeadBlock {
        self.others.values().fold(&self.tip, |heaviest, block| {
            if block.work > heaviest.work {
                block
            } else {
                heaviest
            }
        })
    }

    fn extend(&mut self, block: ChainHeadBlock) -> Result<(), NotChildOfTip> {
        let child_height = self.tip.height().checked_add(1);
        if block.parent_hash != self.tip.hash() || child_height != Some(block.height()) {
            return Err(NotChildOfTip {
                tip: self.tip.reference,
                block: block.reference,
            });
        }
        self.heights_to_hashes.insert(block.height(), block.hash());
        self.replace_tip(block);
        Ok(())
    }

    fn rewind_to(&mut self, block: BlockRef) -> Result<(), NotOnBestChain> {
        if block == self.tip.reference {
            return Ok(());
        }
        if !self.is_on_best_chain(block) {
            return Err(NotOnBestChain);
        }
        let Some(new_tip) = self.others.remove(&block.hash) else {
            return Err(NotOnBestChain);
        };
        self.heights_to_hashes
            .retain(|height, _hash| *height <= block.height);
        self.replace_tip(new_tip);
        Ok(())
    }

    /// The tip is not in `others`, so trimming `others` cannot remove it.
    fn remove_finalized_blocks(&mut self, floor: Height) {
        let tip_hash = self.tip.hash();
        self.others.retain(|_hash, block| block.height() >= floor);
        self.heights_to_hashes
            .retain(|height, hash| *height >= floor || *hash == tip_hash);
    }

    fn stamp_generation(&mut self, previous: &Self, highest_published: u64) {
        self.generation = if self.best_tip() == previous.best_tip() {
            previous.generation
        } else {
            highest_published.saturating_add(1)
        };
    }
}

impl ChainHeadSnapshot for MapBackedSnapshot {
    fn best_tip(&self) -> BlockRef {
        self.tip.reference
    }

    fn work_anchor(&self) -> BlockRef {
        self.work_anchor
    }

    fn epoch(&self) -> ChainStateEpoch {
        ChainStateEpoch {
            generation: self.generation,
            best_tip: self.best_tip(),
        }
    }

    fn block_by_hash(&self, hash: &BlockHash) -> Option<&ChainHeadBlock> {
        if *hash == self.tip.hash() {
            Some(&self.tip)
        } else {
            self.others.get(hash)
        }
    }

    fn best_block_by_height(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.heights_to_hashes
            .get(&height)
            .and_then(|hash| self.block_by_hash(hash))
    }

    fn is_on_best_chain(&self, block: BlockRef) -> bool {
        self.heights_to_hashes.get(&block.height) == Some(&block.hash)
    }

    fn find_fork_point(&self, hash: &BlockHash) -> Option<BlockRef> {
        let mut current = self.block_by_hash(hash)?;
        loop {
            if self.is_on_best_chain(current.reference) {
                return Some(current.reference);
            }
            current = self.block_by_hash(&current.parent_hash)?;
        }
    }

    /// A tip is a retained block that no other retained block claims as its
    /// parent. The canonical tip is always included, even in the degenerate
    /// case where the window holds a single block.
    ///
    /// the legacy full node enumerates block-tree leaves and reports inactive fully-known
    /// branches as `valid-fork`. ChainHead retains whole blocks, never
    /// headers-only or invalid candidates, so those two statuses are the only
    /// ones this can emit.
    fn chain_tips(&self) -> Vec<ChainTip> {
        let parent_hashes = self
            .blocks()
            .map(|block| block.parent_hash)
            .collect::<HashSet<_>>();

        let mut tips = self
            .blocks()
            .filter(|block| {
                block.hash() == self.tip.hash() || !parent_hashes.contains(&block.hash())
            })
            .map(|block| {
                let is_active_tip = block.hash() == self.tip.hash();
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

        for block in self.blocks() {
            let Some((slot, _transaction)) = block
                .block
                .transactions
                .iter()
                .enumerate()
                .find(|(_, transaction)| &transaction.txid == txid)
            else {
                continue;
            };

            let position = ChainHeadTxPosition {
                block: block.reference,
                tx_index: tx_index(slot),
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
            let Some(block) = self.block_by_hash(hash) else {
                continue;
            };
            for (slot, transaction) in block.block.transactions.iter().enumerate() {
                for input in &transaction.transparent.inputs {
                    spenders.insert(
                        Outpoint {
                            txid: input.prev_txid,
                            index: input.prev_index,
                        },
                        SpenderLocation {
                            block: block.reference,
                            txid: transaction.txid,
                            tx_index: tx_index(slot),
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
