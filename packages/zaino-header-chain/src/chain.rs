//! Trusted validators' header tree above the final boundary + its most-work tip
//!
//! ```text
//!   anchor ─ final (last 2·depth in memory)       tree (every branch, bounded)
//!   ●──●──●──●──●  final tip ──┬──●──●──●──●   A   work 1000.7   ◀── best (best_path)
//!                              └──●──●         B   work 1000.2   (kept: may still win)
//! ```
//!
//! - trusted: a header enters on its parent link + its nBits work alone (zebra validated it);
//!   consensus rules = [`validate`](crate::validate), for an untrusted source
//! - anchored, never from genesis: [`HeaderChain::anchor`] = one trusted header as the final tip,
//!   work counted from it
//! - pure core: no clock, no lock, no I/O; one owner (`HeaderSync`) drives it
//! - H1 best = max work, H2 descends from final, H4 bounded, H5 published never changes, H6 final
//!   = vouched: asserted by [`HeaderChain::check`]
//! - final = min(highest vouched on best, best − depth) (H6: [`HeaderChain::vouch`])

use std::cmp::Reverse;
use std::collections::HashMap;

use zaino_primitives::types::{BlockHash, BlockRef, Height, MerkleRoot, ReorgDepth};

use crate::header::Header;
use crate::rules::Rejected;
use crate::target::{expand, work};
use crate::verified::VerifiedChain;

/// Side-branch tips held beside the best (H4)
const SIDE_TIPS: usize = 32;
/// Side-branch nodes held, per block of reorg depth (H4)
const SIDE_NODES_PER_DEPTH: usize = 4;
/// Final headers kept, per block of reorg depth (the NFS reads up to `2 · depth` below best)
const FINALS_PER_DEPTH: usize = 2;

/// One header as the chain keeps it: identity, the fields block checks read, the work from the
/// anchor up to it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub hash: BlockHash,
    pub merkle_root: MerkleRoot,
    pub time: u32,
    pub(crate) bits: u32,
    pub(crate) cumulative_work: u128,
}

/// Best tip + the work behind it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BestTip {
    pub block: BlockRef,
    pub(crate) cumulative_work: u128,
}

/// What an accepted header did to the chain
///
/// - `Known` = held already (tree or final); `Side` = no more work than the best (maybe evicted
///   at once: H4); `Best { reorg }` = new tip, `reorg` = not a child of the previous best
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    Known,
    Side,
    Best { reorg: bool },
}

/// - `received` = arrival order (H1 tie: first received wins; eviction tie: last received goes)
/// - `vouched` closed under parents (H6)
#[derive(Debug, Clone, Copy)]
pub(crate) struct Node {
    pub(crate) record: Record,
    pub(crate) height: Height,
    pub(crate) parent: BlockHash,
    pub(crate) received: u64,
    children: u32,
    vouched: bool,
}

impl Node {
    pub(crate) fn at(&self) -> BlockRef {
        BlockRef { hash: self.record.hash, height: self.height }
    }
}

/// - `finals` = the newest final headers, oldest first, the final tip last (empty = no anchor)
/// - `best_path[i]` = the best branch at `base() + i`, up to the best tip
/// - every collection `imbl` (O(1) into each [`VerifiedChain`])
pub struct HeaderChain {
    depth: ReorgDepth,
    finals: imbl::Vector<(Height, Record)>,
    nodes: imbl::HashMap<BlockHash, Node>,
    leaves: imbl::HashSet<BlockHash>,
    best_path: imbl::Vector<Record>,
    received: u64,
}

impl HeaderChain {
    /// Empty: nothing held until [`anchor`](Self::anchor)
    pub fn new(depth: ReorgDepth) -> Self {
        Self {
            depth,
            finals: imbl::Vector::new(),
            nodes: imbl::HashMap::new(),
            leaves: imbl::HashSet::new(),
            best_path: imbl::Vector::new(),
            received: 0,
        }
    }

    /// `header` at `height`, from a trusted validator, = the final tip; everything held before
    /// dropped (a start, or a jump to a validator far ahead)
    pub fn anchor(&mut self, header: &Header, height: Height) {
        let record = Record {
            hash: header.hash(),
            merkle_root: header.merkle_root(),
            time: header.time(),
            bits: header.bits(),
            cumulative_work: 0,
        };
        self.finals = imbl::vector![(height, record)];
        self.nodes = imbl::HashMap::new();
        self.leaves = imbl::HashSet::new();
        self.best_path = imbl::Vector::new();
    }

    pub fn depth(&self) -> ReorgDepth {
        self.depth
    }

    /// `None` = no anchor yet
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

    /// Last final header (`None` = no anchor yet)
    pub fn final_tip(&self) -> Option<BlockRef> {
        let (height, record) = self.finals.back()?;
        Some(BlockRef { hash: record.hash, height: *height })
    }

    /// Immutable snapshot of the best chain (`None` = no anchor yet)
    pub fn verified(&self) -> Option<VerifiedChain> {
        let best = self.best()?;
        let (above, finals) = (self.best_path.clone(), self.finals.clone());
        let (nodes, leaves) = (self.nodes.clone(), self.leaves.clone());
        Some(VerifiedChain::new(best, above, finals, nodes, leaves))
    }

    /// Headers held above the final tip, every branch
    #[cfg(test)]
    pub(crate) fn tree_len(&self) -> usize {
        self.nodes.len()
    }

    /// A trusted header: attach to a held parent, its work, best, bounds
    pub fn insert(&mut self, header: &Header) -> Result<Inserted, Rejected> {
        let hash = header.hash();
        if self.nodes.contains_key(&hash) || self.is_final(hash) {
            return Ok(Inserted::Known);
        }
        let prev = header.prev_hash();
        let (height, parent_work) = if let Some(parent) = self.nodes.get(&prev) {
            (parent.height.next(), parent.record.cumulative_work)
        } else if let Some((height, record)) = self.finals.back().filter(|(_, r)| r.hash == prev) {
            (height.next(), record.cumulative_work)
        } else if self.is_final(prev) {
            return Err(Rejected::BelowFinal);
        } else {
            return Err(Rejected::Orphan);
        };

        let bits = header.bits();
        let own = expand(bits).and_then(work).ok_or(Rejected::Bits { bits })?;
        let cumulative_work = parent_work.checked_add(own).ok_or(Rejected::WorkOverflow)?;
        let record = Record {
            hash,
            merkle_root: header.merkle_root(),
            time: header.time(),
            bits,
            cumulative_work,
        };

        self.received += 1;
        let received = self.received;
        let node = Node { record, height, parent: prev, received, children: 0, vouched: false };
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

    /// A trusted validator had `block` on its best chain: it + every ancestor vouched (H6)
    ///
    /// - permanent (zebra commits only valid blocks); not held above the final tip = no-op
    pub fn vouch(&mut self, block: BlockRef) {
        let mut at = block.hash;
        if self.nodes.get(&at).is_none_or(|node| node.height != block.height) {
            return;
        }
        while let Some(node) = self.nodes.get_mut(&at).filter(|node| !node.vouched) {
            node.vouched = true;
            at = node.parent;
        }
    }

    /// Highest height a fetch may reach: `depth` + `run` above the final tip (bounds the tree
    /// whatever finality does)
    pub fn ceiling(&self, run: u32) -> Height {
        let above = self.depth.get().saturating_add(run).saturating_sub(1);
        self.base().checked_add(above).expect("no chain nears the protocol's maximum height")
    }

    /// Best-chain block final moves to next: min(highest vouched, best − depth), above the
    /// final tip
    pub fn finalizable(&self) -> Option<BlockRef> {
        let best = self.best()?;
        let boundary = best.block.height.checked_sub(self.depth.get())?.min(self.vouched()?);
        let hash = self.best_path.get(self.offset(boundary)?)?.hash;
        Some(BlockRef { hash, height: boundary })
    }

    /// Every best-chain header up to `through` becomes final, every branch not descending from
    /// it pruned; the newest `2 · depth` finals kept
    pub fn finalize(&mut self, through: BlockRef) {
        assert!(self.on_best(through), "H2: only a best-branch block becomes final");
        let best = self.best().expect("a best-branch block has a best tip");
        let deep = u32::from(best.block.height) - u32::from(through.height);
        assert!(deep >= self.depth.get(), "H2: the final boundary stays `depth` below the best");
        let vouched = self.vouched().is_some_and(|vouched| through.height <= vouched);
        assert!(vouched, "H6: only a vouched block (or an ancestor of one) becomes final");

        let count = self.offset(through.height).expect("on the best branch") + 1;
        let newly: Vec<(Height, Record)> =
            self.base().up_to(through.height).zip(self.best_path.iter().copied()).collect();
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
        let kept = FINALS_PER_DEPTH * self.depth.get() as usize;
        for (height, record) in newly {
            self.nodes.remove(&record.hash);
            self.finals.push_back((height, record));
        }
        if self.finals.len() > kept {
            self.finals = self.finals.skip(self.finals.len() - kept);
        }
        self.best_path = self.best_path.skip(count);
    }

    /// H1, H2, H4, H5, H6 and the tree's own bookkeeping; panics naming the invariant broken
    ///
    /// - O(nodes): tests run it after every mutation, the driver after every run in debug builds
    pub fn check(&self) {
        let final_tip = self.final_tip();
        assert!(final_tip.is_some() || self.nodes.is_empty(), "H2: nothing held before an anchor");
        let kept = FINALS_PER_DEPTH * self.depth.get() as usize;
        assert!(self.finals.len() <= kept.max(1), "H2: at most 2 · depth finals kept");
        let mut pairs = self.finals.iter().zip(self.finals.iter().skip(1));
        let contiguous = pairs.all(|((below, _), (above, _))| below.next() == *above);
        assert!(contiguous, "H2: finals one height apart");
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
                (None, None) => unreachable!("H2: nothing held before an anchor"),
            };
            let own = expand(node.record.bits).and_then(work).expect("nBits checked on insert");
            assert_eq!(parent_work + own, node.record.cumulative_work, "tree: cumulative work");
            let parent_vouched = self.nodes.get(&node.parent).is_none_or(|parent| parent.vouched);
            assert!(!node.vouched || parent_vouched, "H6: {hash:?} vouched, its parent not");
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

    /// Highest vouched best-branch height, else the final tip (final ⇒ vouched: H6)
    fn vouched(&self) -> Option<Height> {
        let top = self.best_path.iter().rposition(|record| self.nodes[&record.hash].vouched);
        match top {
            Some(at) => self.base().checked_add(u32::try_from(at).expect("heights fit u32")),
            None => self.final_tip().map(|tip| tip.height),
        }
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
}

#[cfg(test)]
mod fire_drills;
