//! The verified header tree above the final boundary, and its most-work tip
//!
//! ```text
//!   final (store + last CONTEXT in memory)     tree (every valid branch, bounded)
//!   ●──●──●──●──●  final tip ──┬──●──●──●──●   A   work 1000.7   ◀── best (best_path)
//!                              └──●──●         B   work 1000.2   (kept: may still win)
//! ```
//!
//! - pure core: no clock (time is an input), no lock; one owner (`HeaderSync`) drives it
//! - invariants H1, H2, H4, H5 (`docs/design/verified-chain.md` §10) asserted by [`HeaderChain::check`]

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};

use zaino_persistence::StoreError;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};

use crate::params::Params;
use crate::rules::{in_context, Ancestor, Checked, Rejected, CONTEXT};
use crate::store::{HeaderStore, Record};
use crate::target::{expand, work};
use crate::verified::VerifiedChain;

/// Side-branch tips held beside the best (H4)
const SIDE_TIPS: usize = 32;
/// Side-branch nodes held, per block of reorg depth (H4)
pub const SIDE_NODES_PER_DEPTH: usize = 4;

/// The best tip and the work behind it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BestTip {
    pub block: BlockRef,
    pub(crate) cumulative_work: u128,
}

/// What an accepted header did to the chain
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// Held already (tree or final)
    Known,
    /// Valid, on a branch with no more work than the best (possibly evicted at once: H4)
    Side,
    /// New best tip; `reorg` = not a child of the previous best
    Best { reorg: bool },
}

#[derive(Debug, Clone, Copy)]
struct Node {
    record: Record,
    height: Height,
    parent: BlockHash,
    /// Arrival order (H1 tie: first received wins; eviction tie: last received goes)
    received: u64,
    children: u32,
}

/// `best_path[i]` = the best branch at `base() + i`, up to the best tip
pub struct HeaderChain {
    params: Params,
    depth: ReorgDepth,
    store: HeaderStore,
    finals: VecDeque<(Height, Record)>,
    nodes: HashMap<BlockHash, Node>,
    leaves: HashSet<BlockHash>,
    best_path: imbl::Vector<Record>,
    received: u64,
}

impl HeaderChain {
    /// Resumes from `store`'s final headers (verified once, when they were written); empty =
    /// the first header accepted is this network's genesis
    pub fn open(params: Params, depth: ReorgDepth, store: HeaderStore) -> Self {
        let mut finals = VecDeque::new();
        if let Some(tip) = store.tip() {
            let from = tip.height.saturating_sub(CONTEXT as u32 - 1);
            finals = from.up_to(tip.height).zip(store.view().records(from, tip.height)).collect();
        }
        Self {
            params,
            depth,
            store,
            finals,
            nodes: HashMap::new(),
            leaves: HashSet::new(),
            best_path: imbl::Vector::new(),
            received: 0,
        }
    }

    pub fn params(&self) -> &Params {
        &self.params
    }

    /// `None` = nothing verified yet
    pub fn best(&self) -> Option<BestTip> {
        match self.best_path.last() {
            Some(record) => {
                let above = u32::try_from(self.best_path.len() - 1).expect("heights fit u32");
                let height = self.base().checked_add(above).expect("a held height");
                let block = BlockRef { hash: record.hash, height };
                Some(BestTip { block, cumulative_work: record.cumulative_work })
            }
            None => self.finals.back().map(|(height, record)| BestTip {
                block: BlockRef { hash: record.hash, height: *height },
                cumulative_work: record.cumulative_work,
            }),
        }
    }

    /// Last final header (`None` = none final yet)
    pub fn final_tip(&self) -> Option<BlockRef> {
        let (height, record) = self.finals.back()?;
        Some(BlockRef { hash: record.hash, height: *height })
    }

    /// Immutable snapshot of the best chain (`None` = nothing verified yet)
    pub fn verified(&self) -> Option<VerifiedChain> {
        let best = self.best()?;
        Some(VerifiedChain::new(best, self.final_tip(), self.best_path.clone(), self.store.view()))
    }

    /// Headers held above the final tip, every branch
    #[cfg(test)]
    pub(crate) fn tree_len(&self) -> usize {
        self.nodes.len()
    }

    /// Regtest rules over `genesis` (a `testing::Chain`'s), any nBits (`mine_bits`: work varies
    /// per branch, so most work != highest), a fresh `SimFs` store, nothing final
    #[cfg(feature = "testing")]
    pub fn regtest_in_memory(genesis: BlockHash, depth: ReorgDepth) -> Self {
        let params = Params::regtest(Height::GENESIS.next(), None).with_genesis(genesis).any_bits();
        let fs = zaino_persistence::fs::SimFs::new();
        let regtest = zcash_protocol::consensus::NetworkType::Regtest;
        let store = HeaderStore::open(fs, std::path::Path::new("/headers"), regtest)
            .expect("a fresh SimFs store opens");
        Self::open(params, depth, store)
    }

    /// Every header of `blocks` in order, stage A then B (a `testing::Chain`'s real bytes); the
    /// first refusal ends it
    #[cfg(feature = "testing")]
    pub fn insert_blocks(
        &mut self,
        blocks: &[zaino_primitives::types::Block],
    ) -> Result<(), Rejected> {
        for block in blocks {
            let raw = zaino_primitives::testing::encode_header(block.header());
            let header = crate::decode_header(&raw).expect("testing::Chain headers decode");
            self.insert(&crate::check(&self.params, header)?, i64::MAX / 2)?;
        }
        Ok(())
    }

    /// Stage B: attach, nBits, time rules, work, best, bounds; `now_unix` = the local clock
    pub fn insert(&mut self, checked: &Checked, now_unix: i64) -> Result<Inserted, Rejected> {
        assert_eq!(checked.network(), self.params.network, "H3: checked under this chain's rules");
        let header = checked.header();
        let hash = header.hash();
        if self.nodes.contains_key(&hash) || self.is_final(hash) {
            return Ok(Inserted::Known);
        }
        let prev = header.prev_hash();
        let (height, parent_work) = if prev == BlockHash::ZERO {
            if hash != self.params.genesis {
                return Err(Rejected::WrongGenesis);
            }
            // unknown with a final tip = final below the tail
            if self.final_tip().is_some() {
                return Err(Rejected::BelowFinal);
            }
            (Height::GENESIS, 0)
        } else if let Some(parent) = self.nodes.get(&prev) {
            (parent.height.next(), parent.record.cumulative_work)
        } else if self.final_tip().is_some_and(|tip| tip.hash == prev) {
            let (height, record) = self.finals.back().expect("a final tip has a record");
            (height.next(), record.cumulative_work)
        } else if self.is_final(prev) {
            return Err(Rejected::BelowFinal);
        } else {
            return Err(Rejected::Orphan);
        };

        let context = self.context(prev, height);
        let own = in_context(&self.params, header, height, &context, now_unix)?;
        let cumulative_work = parent_work.checked_add(own).ok_or(Rejected::WorkOverflow)?;
        let record = Record {
            hash,
            merkle_root: header.merkle_root(),
            time: header.time(),
            bits: header.bits(),
            cumulative_work,
        };

        self.received += 1;
        let node = Node { record, height, parent: prev, received: self.received, children: 0 };
        self.nodes.insert(hash, node);
        self.leaves.insert(hash);
        if let Some(parent) = self.nodes.get_mut(&prev) {
            parent.children += 1;
            self.leaves.remove(&prev);
        }

        let best = self.best();
        let inserted = match best.is_some_and(|best| cumulative_work <= best.cumulative_work) {
            true => Inserted::Side,
            false => {
                let reorg = best.is_some_and(|best| best.block.hash != prev);
                self.adopt(hash, height);
                Inserted::Best { reorg }
            }
        };
        self.evict();
        Ok(inserted)
    }

    /// The best-chain block at the final boundary (`depth` below the best tip), once deep enough
    pub fn finalizable(&self) -> Option<BlockRef> {
        let best = self.best()?;
        let boundary = best.block.height.checked_sub(self.depth.get())?;
        let hash = self.best_path.get(self.offset(boundary)?)?.hash;
        Some(BlockRef { hash, height: boundary })
    }

    /// Every best-chain header up to `through` becomes final: written (one commit, before anything
    /// in memory moves), every branch not descending from it pruned
    pub fn finalize(&mut self, through: BlockRef) -> Result<(), StoreError> {
        assert!(self.on_best(through), "H2: only a best-branch block becomes final");
        let best = self.best().expect("a best-branch block has a best tip");
        let deep = u32::from(best.block.height) - u32::from(through.height);
        assert!(deep >= self.depth.get(), "H2: the final boundary stays `depth` below the best");

        let count = self.offset(through.height).expect("on the best branch") + 1;
        let newly: Vec<(Height, Record)> =
            self.base().up_to(through.height).zip(self.best_path.iter().copied()).collect();
        self.store.append(&newly)?;

        let side: Vec<BlockHash> =
            self.leaves.iter().copied().filter(|leaf| *leaf != best.block.hash).collect();
        for leaf in side {
            let (doomed, prune) = self.off_best(leaf, through.height);
            if prune {
                for hash in doomed {
                    self.nodes.remove(&hash);
                    self.leaves.remove(&hash);
                }
            }
        }
        for (height, record) in newly {
            self.nodes.remove(&record.hash);
            self.finals.push_back((height, record));
            if self.finals.len() > CONTEXT {
                self.finals.pop_front();
            }
        }
        self.best_path = self.best_path.skip(count);
        Ok(())
    }

    /// H1, H2, H4, H5 and the tree's own bookkeeping; panics naming the invariant broken
    ///
    /// - O(nodes): tests run it after every mutation, the driver after every run in debug builds
    pub fn check(&self) {
        let final_tip = self.final_tip();
        assert_eq!(self.store.tip(), final_tip, "H2: final tip = the store's committed tip");
        let base = self.base();
        let mut children: HashMap<BlockHash, u32> = HashMap::new();
        for (hash, node) in &self.nodes {
            assert_eq!(node.record.hash, *hash, "tree: node keyed by its own hash");
            let parent_work = match (self.nodes.get(&node.parent), final_tip) {
                (Some(parent), _) => {
                    assert_eq!(
                        parent.height.next(),
                        node.height,
                        "H2: one height above its parent"
                    );
                    parent.record.cumulative_work
                }
                (None, Some(tip)) => {
                    let off_final = node.parent != tip.hash || node.height != base;
                    assert!(!off_final, "H2: {hash:?} does not descend from the final tip");
                    self.finals.back().map_or(0, |(_, record)| record.cumulative_work)
                }
                (None, None) => {
                    let genesis = *hash == self.params.genesis && node.height == Height::GENESIS;
                    assert!(genesis, "H2: {hash:?} descends from nothing held");
                    0
                }
            };
            let own = expand(node.record.bits).and_then(work).expect("verified nBits");
            assert_eq!(parent_work + own, node.record.cumulative_work, "tree: cumulative work");
            *children.entry(node.parent).or_default() += 1;
        }
        for (hash, node) in &self.nodes {
            let held = children.get(hash).copied().unwrap_or(0);
            assert_eq!(node.children, held, "tree: child count of {hash:?}");
            assert_eq!(self.leaves.contains(hash), held == 0, "tree: leaf set at {hash:?}");
        }
        assert!(self.leaves.iter().all(|leaf| self.nodes.contains_key(leaf)), "tree: leaf held");

        let mut parent = final_tip.map_or(BlockHash::ZERO, |tip| tip.hash);
        for (at, record) in self.best_path.iter().enumerate() {
            let node = self.nodes.get(&record.hash);
            let height = base.checked_add(at as u32);
            let path = node.is_some_and(|n| n.parent == parent && Some(n.height) == height);
            assert!(path, "H5: best_path at {height:?} = a held chain from the final tip");
            assert_eq!(node.map(|n| n.record), Some(*record), "H5: best_path record = its node");
            parent = record.hash;
        }

        let top = self.leaves.iter().max_by_key(|leaf| {
            let node = &self.nodes[*leaf];
            (node.record.cumulative_work, Reverse(node.received))
        });
        let best = self.best_path.last().map(|record| record.hash);
        assert_eq!(top.copied(), best, "H1: best = the max-work leaf, first received on a tie");

        let side_nodes = self.nodes.len() - self.best_path.len();
        let side_tips = self.leaves.len() - usize::from(best.is_some());
        assert!(side_nodes <= self.max_side_nodes(), "H4: {side_nodes} side nodes");
        assert!(side_tips <= SIDE_TIPS, "H4: {side_tips} side tips");
    }

    /// First height above the final tip
    fn base(&self) -> Height {
        self.final_tip().map_or(Height::GENESIS, |tip| tip.height.next())
    }

    /// `best_path` index of `height` (`None` = at or below the final tip)
    fn offset(&self, height: Height) -> Option<usize> {
        let above = u32::from(height).checked_sub(u32::from(self.base()))?;
        Some(above as usize)
    }

    fn on_best(&self, block: BlockRef) -> bool {
        self.offset(block.height)
            .and_then(|at| self.best_path.get(at))
            .is_some_and(|record| record.hash == block.hash)
    }

    fn max_side_nodes(&self) -> usize {
        SIDE_NODES_PER_DEPTH * self.depth.get() as usize
    }

    /// `tip` (just inserted, most work) becomes best: `best_path` re-pointed at its branch
    fn adopt(&mut self, tip: BlockHash, height: Height) {
        let mut branch = Vec::new();
        let (mut at, mut height) = (tip, height);
        let keep = loop {
            let Some(offset) = self.offset(height) else { break 0 };
            if self.best_path.get(offset).is_some_and(|record| record.hash == at) {
                break offset + 1;
            }
            let node = self.nodes[&at];
            branch.push(node.record);
            at = node.parent;
            match height.checked_sub(1) {
                Some(below) => height = below,
                None => break 0,
            }
        };
        self.best_path.truncate(keep);
        self.best_path.extend(branch.into_iter().rev());
    }

    /// H4: past either bound, the lowest-work side leaf goes (last received on a tie)
    fn evict(&mut self) {
        loop {
            let best = self.best_path.last().map(|record| record.hash);
            let side_nodes = self.nodes.len() - self.best_path.len();
            let side_tips = self.leaves.len() - usize::from(best.is_some());
            if side_nodes <= self.max_side_nodes() && side_tips <= SIDE_TIPS {
                return;
            }
            let victim = self
                .leaves
                .iter()
                .copied()
                .filter(|leaf| Some(*leaf) != best)
                .min_by_key(|leaf| {
                    let node = &self.nodes[leaf];
                    (node.record.cumulative_work, Reverse(node.received))
                })
                .expect("past a bound: a side leaf exists");
            let node = self.nodes.remove(&victim).expect("a leaf is held");
            self.leaves.remove(&victim);
            if let Some(parent) = self.nodes.get_mut(&node.parent) {
                parent.children -= 1;
                if parent.children == 0 {
                    self.leaves.insert(node.parent);
                }
            }
        }
    }

    /// Off-best nodes from `leaf` down, and whether its branch leaves the best one below `final`
    /// (a node already gone = an earlier walk pruned the same branch)
    fn off_best(&self, leaf: BlockHash, final_height: Height) -> (Vec<BlockHash>, bool) {
        let mut walked = Vec::new();
        let mut at = leaf;
        loop {
            match self.nodes.get(&at) {
                Some(node) if self.on_best(BlockRef { hash: at, height: node.height }) => {
                    return (walked, node.height < final_height);
                }
                Some(node) => {
                    walked.push(at);
                    at = node.parent;
                }
                None => return (walked, true),
            }
        }
    }

    fn is_final(&self, hash: BlockHash) -> bool {
        self.finals.iter().any(|(_, record)| record.hash == hash)
    }

    /// Up to `CONTEXT` ancestors of a header at `height` whose parent is `prev`, newest first
    fn context(&self, prev: BlockHash, height: Height) -> Vec<Ancestor> {
        let mut context = Vec::with_capacity(CONTEXT);
        if height == Height::GENESIS {
            return context;
        }
        let mut at = prev;
        while let Some(node) = self.nodes.get(&at) {
            context.push(Ancestor { bits: node.record.bits, time: node.record.time });
            if context.len() == CONTEXT {
                return context;
            }
            at = node.parent;
        }
        let below = self.finals.iter().rev().skip_while(|(_, record)| record.hash != at);
        context.extend(
            below
                .take(CONTEXT - context.len())
                .map(|(_, record)| Ancestor { bits: record.bits, time: record.time }),
        );
        context
    }
}

#[cfg(test)]
mod fire_drills;
