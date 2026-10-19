//! Trusted validators' header tree above the finalized boundary + its most-work tip
//!
//! ```text
//!   anchor ─ finalized (last 2·depth in memory)   tree (every branch, bounded)
//!   ●──●──●──●──●  finalized tip ──┬──●──●──●──●   A   work 1000.7   ◀── best (best_path)
//!                                  └──●──●         B   work 1000.2   (kept: may still win)
//! ```
//!
//! - trusted: a header enters on its parent link + its nBits work alone (zebra validated it)
//! - untrusted ([`HeaderChain::insert_untrusted`]): every consensus rule first, over the
//!   ancestors held
//! - anchored, never from genesis: [`HeaderChain::anchor`] = one trusted header as the finalized
//!   tip, work counted from it
//! - pure core: no clock, no lock, no I/O, one owner (`HeaderSync`)
//! - H1 best = max work, H2 descends from finalized, H4 bounded, H5 published never changes, H6
//!   finalized = settled, H7 witnesses closed under parents: asserted by [`HeaderChain::check`]
//!   and on every finalize
//! - settled = vouched, or witnessed by peers in > 5 netgroups with no difficulty collapse since
//!   the last vouch ([`HeaderChain::witness`])
//! - finalized = min(highest settled on best, best − depth)

use std::cmp::Reverse;
use std::collections::HashMap;

use zaino_primitives::types::{BlockRef, FinalityDepth};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

use crate::header::Header;
use crate::params::Params;
use crate::rules::{header_work, validate, Ancestor, Rejected, CONTEXT};
use crate::snapshot::HeaderChainSnapshot;
use crate::witnesses::{Witness, Witnesses};

/// Side-branch tips held beside the best (H4)
const SIDE_TIPS: usize = 32;
/// Side-branch nodes held, per block of reorg depth (H4)
const SIDE_NODES_PER_DEPTH: usize = 4;
/// Finalized headers kept, per block of reorg depth (the NFS reads up to `2 · depth` below best)
const FINALIZED_PER_DEPTH: usize = 2;
/// Peer finality refused once a non-finalized best-chain block holds under 1/this of the last
/// vouched block's work
const MAX_DIFFICULTY_DROP: u128 = 2;

/// One header as the chain keeps it: identity, the fields block checks read, the work from the
/// anchor up to it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub hash: BlockHash,
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
/// - `Known` = held already (tree or finalized)
/// - `Side` = no more work than the best (maybe evicted at once: H4)
/// - `Best { reorg }` = new tip, `reorg` = not a child of the previous best
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    Known,
    Side,
    Best { reorg: bool },
}

/// - `received` = arrival order (H1 tie → first received wins, eviction tie → last received goes)
#[derive(Debug, Clone, Copy)]
pub(crate) struct Node {
    pub(crate) record: Record,
    pub(crate) height: BlockHeight,
    pub(crate) parent: BlockHash,
    pub(crate) received: u64,
    children: u32,
    pub(crate) witnesses: Witnesses,
}

/// - `finalized_headers` = the newest finalized headers, oldest first, the finalized tip last
///   (empty = no anchor)
/// - `best_path[i]` = the best branch at `base() + i`, up to the best tip
/// - `last_vouched` = (height, own work) of the highest block a trusted validator vouched for,
///   else the anchor: peer finality's difficulty reference (never moved by peers)
/// - `revision` = +1 per change (a snapshot worth publishing)
/// - every collection `imbl` (O(1) into each [`HeaderChainSnapshot`])
pub struct HeaderChain {
    depth: FinalityDepth,
    finalized_headers: imbl::Vector<(BlockHeight, Record)>,
    nodes: imbl::HashMap<BlockHash, Node>,
    leaves: imbl::HashSet<BlockHash>,
    best_path: imbl::Vector<Record>,
    received: u64,
    last_vouched: Option<(BlockHeight, u128)>,
    revision: u64,
}

impl HeaderChain {
    /// Empty: nothing held until [`anchor`](Self::anchor)
    pub fn new(depth: FinalityDepth) -> Self {
        Self {
            depth,
            finalized_headers: imbl::Vector::new(),
            nodes: imbl::HashMap::new(),
            leaves: imbl::HashSet::new(),
            best_path: imbl::Vector::new(),
            received: 0,
            last_vouched: None,
            revision: 0,
        }
    }

    /// `header` at `height`, from a trusted validator, = the finalized tip
    ///
    /// - everything held before dropped (a start, or a jump to a validator far ahead)
    pub fn anchor(&mut self, header: &Header, height: BlockHeight) {
        let record = Record {
            hash: header.hash(),
            time: header.time(),
            bits: header.bits(),
            cumulative_work: 0,
        };
        self.finalized_headers = imbl::vector![(height, record)];
        self.nodes = imbl::HashMap::new();
        self.leaves = imbl::HashSet::new();
        self.best_path = imbl::Vector::new();
        self.last_vouched = header_work(header.bits()).ok().map(|work| (height, work));
        self.revision += 1;
    }

    /// `None` = no anchor yet
    pub fn best(&self) -> Option<BestTip> {
        match self.best_path.last() {
            Some(record) => {
                let above = u32::try_from(self.best_path.len() - 1).expect("heights fit u32");
                let height = self.base() + above;
                let block = BlockRef { hash: record.hash, height };
                Some(BestTip { block, cumulative_work: record.cumulative_work })
            }
            None => self.finalized_headers.back().map(|(height, record)| BestTip {
                block: BlockRef { hash: record.hash, height: *height },
                cumulative_work: record.cumulative_work,
            }),
        }
    }

    /// Last finalized header (`None` = no anchor yet)
    pub fn finalized_tip(&self) -> Option<BlockRef> {
        let (height, record) = self.finalized_headers.back()?;
        Some(BlockRef { hash: record.hash, height: *height })
    }

    /// Moves on every change: a held snapshot is stale iff taken at an older revision
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Immutable view of the best chain (`None` = no anchor yet)
    pub fn snapshot(&self) -> Option<HeaderChainSnapshot> {
        let best = self.best()?;
        let (above, finalized_headers) = (self.best_path.clone(), self.finalized_headers.clone());
        Some(HeaderChainSnapshot::new(best, above, finalized_headers, self.nodes.clone()))
    }

    /// Headers held above the finalized tip, every branch
    #[cfg(test)]
    pub(crate) fn tree_len(&self) -> usize {
        self.nodes.len()
    }

    /// Finalized headers kept: what the NFS reads below the best (`2 · depth`), at least what the
    /// rules read below an untrusted header ([`CONTEXT`])
    pub(crate) fn finalized_headers_kept(&self) -> usize {
        (FINALIZED_PER_DEPTH * self.depth.get() as usize).max(CONTEXT)
    }

    /// How far below a trusted claim a fresh anchor sits: the finalized depth, at least `CONTEXT`
    /// (the claim's run then holds every ancestor the rules read)
    pub fn anchor_below(&self) -> u32 {
        self.depth.get().max(CONTEXT as u32)
    }

    /// Height `header` takes on its parent (`None` = parent not held at or above the finalized tip)
    pub fn height_of(&self, header: &Header) -> Option<BlockHeight> {
        self.parent(header.prev_hash()).ok().map(|(height, _)| height)
    }

    /// An untrusted header: every consensus rule (`validate`) over the ancestors held, then
    /// [`insert`](Self::insert)
    ///
    /// - `now_unix` = the local clock (the 2 h future bound)
    pub fn insert_untrusted(
        &mut self,
        header: &Header,
        params: &Params,
        now_unix: i64,
    ) -> Result<Inserted, Rejected> {
        if self.known(header.hash()) {
            return Ok(Inserted::Known);
        }
        let (height, _) = self.parent(header.prev_hash())?;
        validate(params, header, height, &self.ancestors(header.prev_hash()), now_unix)?;
        self.insert(header)
    }

    /// A trusted header: attach to a held parent, its work, best, bounds
    pub fn insert(&mut self, header: &Header) -> Result<Inserted, Rejected> {
        let hash = header.hash();
        if self.known(hash) {
            return Ok(Inserted::Known);
        }
        let prev = header.prev_hash();
        let (height, parent_work) = self.parent(prev)?;

        let bits = header.bits();
        let own = header_work(bits)?;
        let cumulative_work = parent_work.checked_add(own).ok_or(Rejected::WorkOverflow)?;
        let record = Record { hash, time: header.time(), bits, cumulative_work };

        self.received += 1;
        let received = self.received;
        let witnesses = Witnesses::default();
        let node = Node { record, height, parent: prev, received, children: 0, witnesses };
        self.nodes.insert(hash, node);
        self.revision += 1;
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

    /// `by` had `hash` on its best chain: it + every ancestor witnessed, for good (H7)
    ///
    /// - not held above the finalized tip = no-op
    pub fn witness(&mut self, hash: BlockHash, by: Witness) {
        let Some(node) = self.nodes.get(&hash) else { return };
        let higher = self.last_vouched.is_none_or(|(height, _)| node.height > height);
        if by == Witness::Trusted && higher {
            self.last_vouched = header_work(node.record.bits).ok().map(|work| (node.height, work));
        }
        let mut at = hash;
        while let Some(node) = self.nodes.get_mut(&at) {
            if !node.witnesses.add(by) {
                break;
            }
            at = node.parent;
        }
        if at != hash {
            self.revision += 1;
        }
    }

    /// Highest height a fetch may reach: `depth` + `run` above the finalized tip (bounds the tree
    /// whatever finality does)
    pub fn ceiling(&self, run: u32) -> BlockHeight {
        let above = self.depth.get().saturating_add(run).saturating_sub(1);
        self.base() + above
    }

    /// Best-chain block finalized moves to next: min(highest settled, best − depth), above the
    /// finalized tip
    pub fn finalizable(&self) -> Option<BlockRef> {
        let best = self.best()?;
        let below = u32::from(best.block.height).checked_sub(self.depth.get())?;
        let boundary = BlockHeight::from(below).min(self.settled()?);
        let hash = self.best_path.get(self.offset(boundary)?)?.hash;
        Some(BlockRef { hash, height: boundary })
    }

    /// Every best-chain header up to `through` finalized
    ///
    /// - every branch not descending from it pruned
    /// - newest `finalized_headers_kept()` finalized headers kept
    pub fn finalize(&mut self, through: BlockRef) {
        let settled = self.settled().is_some_and(|settled| through.height <= settled);
        assert!(settled, "H6: only a settled block (or an ancestor of one) becomes finalized");
        assert!(self.on_best(through), "H2: only a best-branch block becomes finalized");
        let best = self.best().expect("a best-branch block has a best tip");
        let deep = u32::from(best.block.height) - u32::from(through.height);
        assert!(
            deep >= self.depth.get(),
            "H2: the finalized boundary stays `depth` below the best"
        );

        let count = self.offset(through.height).expect("on the best branch") + 1;
        let newly: Vec<(BlockHeight, Record)> = (u32::from(self.base())
            ..=u32::from(through.height))
            .map(BlockHeight::from)
            .zip(self.best_path.iter().copied())
            .collect();
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
        let kept = self.finalized_headers_kept();
        for (height, record) in newly {
            self.nodes.remove(&record.hash);
            self.finalized_headers.push_back((height, record));
        }
        if self.finalized_headers.len() > kept {
            self.finalized_headers =
                self.finalized_headers.skip(self.finalized_headers.len() - kept);
        }
        self.best_path = self.best_path.skip(count);
        self.revision += 1;
    }

    /// H1, H2, H4, H5, H7 + the tree's own bookkeeping (panics naming the invariant broken)
    ///
    /// - O(nodes): tests run it after every mutation
    pub fn check(&self) {
        let finalized_tip = self.finalized_tip();
        assert!(
            finalized_tip.is_some() || self.nodes.is_empty(),
            "H2: nothing held before an anchor"
        );
        let kept = self.finalized_headers_kept();
        assert!(
            self.finalized_headers.len() <= kept,
            "H2: at most max(2 · depth, CONTEXT) finalized headers kept"
        );
        let mut pairs = self.finalized_headers.iter().zip(self.finalized_headers.iter().skip(1));
        let contiguous = pairs.all(|((below, _), (above, _))| *below + 1 == *above);
        assert!(contiguous, "H2: finalized headers one height apart");
        let base = self.base();
        let mut children: HashMap<BlockHash, u32> = HashMap::new();
        for (hash, node) in &self.nodes {
            assert_eq!(node.record.hash, *hash, "tree: node keyed by its own hash");
            let parent_work = match (self.nodes.get(&node.parent), finalized_tip) {
                (Some(parent), _) => {
                    assert_eq!(parent.height + 1, node.height, "H2: one height above its parent");
                    parent.record.cumulative_work
                }
                (None, Some(tip)) => {
                    let off_finalized = node.parent != tip.hash || node.height != base;
                    assert!(!off_finalized, "H2: {hash:?} does not descend from the finalized tip");
                    self.finalized_headers.back().map_or(0, |(_, record)| record.cumulative_work)
                }
                (None, None) => unreachable!("H2: nothing held before an anchor"),
            };
            let own = header_work(node.record.bits).expect("nBits checked on insert");
            assert_eq!(parent_work + own, node.record.cumulative_work, "tree: cumulative work");
            let parent = self.nodes.get(&node.parent);
            let covered = parent.is_none_or(|parent| parent.witnesses.covers(&node.witnesses));
            assert!(covered, "H7: {hash:?} witnessed, its parent not");
            *children.entry(node.parent).or_default() += 1;
        }
        for (hash, node) in &self.nodes {
            let held = children.get(hash).copied().unwrap_or(0);
            assert_eq!(node.children, held, "tree: child count of {hash:?}");
            assert_eq!(self.leaves.contains(hash), held == 0, "tree: leaf set at {hash:?}");
        }
        assert!(self.leaves.iter().all(|leaf| self.nodes.contains_key(leaf)), "tree: leaf held");

        let mut parent = finalized_tip.map_or(BlockHash([0; 32]), |tip| tip.hash);
        for (at, record) in self.best_path.iter().enumerate() {
            let node = self.nodes.get(&record.hash);
            let height = base + at as u32;
            let path = node.is_some_and(|n| n.parent == parent && n.height == height);
            assert!(path, "H5: best_path at {height:?} = a held chain from the finalized tip");
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

    /// First height above the finalized tip
    fn base(&self) -> BlockHeight {
        self.finalized_tip().map_or(H0, |tip| tip.height + 1)
    }

    /// `best_path` index of `height` (`None` = at or below the finalized tip)
    fn offset(&self, height: BlockHeight) -> Option<usize> {
        let above = u32::from(height).checked_sub(u32::from(self.base()))?;
        Some(above as usize)
    }

    fn on_best(&self, block: BlockRef) -> bool {
        self.offset(block.height)
            .and_then(|at| self.best_path.get(at))
            .is_some_and(|record| record.hash == block.hash)
    }

    /// Highest settled best-branch height, else the finalized tip (H6)
    ///
    /// - settled = vouched, or peer-quorate while no unvouched block's difficulty collapsed (an
    ///   eclipse attacker's private chain retargets down to its own hashrate)
    fn settled(&self) -> Option<BlockHeight> {
        let witnesses = |record: &Record| self.nodes[&record.hash].witnesses;
        let vouched = self.best_path.iter().rposition(|record| witnesses(record).vouched());
        let unvouched_from = vouched.map_or(0, |at| at + 1);
        let quorate = match self.collapsed(unvouched_from) {
            true => None,
            false => self.best_path.iter().rposition(|record| witnesses(record).peer_quorate()),
        };
        match vouched.max(quorate) {
            Some(at) => Some(self.base() + u32::try_from(at).expect("heights fit u32")),
            None => self.finalized_tip().map(|tip| tip.height),
        }
    }

    /// A best-chain block from `best_path[from]` up with its own work under
    /// 1 / `MAX_DIFFICULTY_DROP` of the last vouched block's
    fn collapsed(&self, from: usize) -> bool {
        let Some((_, reference)) = self.last_vouched else { return true };
        let finalized_work =
            self.finalized_headers.back().map_or(0, |(_, record)| record.cumulative_work);
        let parents = self.best_path.iter().map(|record| record.cumulative_work);
        let parents = std::iter::once(finalized_work).chain(parents);
        let own = self.best_path.iter().zip(parents).map(|(r, parent)| r.cumulative_work - parent);
        own.skip(from).any(|own| own.saturating_mul(MAX_DIFFICULTY_DROP) < reference)
    }

    fn max_side_nodes(&self) -> usize {
        SIDE_NODES_PER_DEPTH * self.depth.get() as usize
    }

    /// `tip` (just inserted, most work) becomes best: `best_path` re-pointed at its branch
    fn adopt(&mut self, tip: BlockHash, height: BlockHeight) {
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
            match u32::from(height).checked_sub(1) {
                Some(below) => height = BlockHeight::from(below),
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

    /// Off-best nodes from `leaf` down, and whether its branch leaves the best one below
    /// `finalized` (a node already gone = an earlier walk pruned the same branch)
    fn off_best(&self, leaf: BlockHash, finalized_height: BlockHeight) -> (Vec<BlockHash>, bool) {
        let mut walked = Vec::new();
        let mut at = leaf;
        loop {
            match self.nodes.get(&at) {
                Some(node) if self.on_best(BlockRef { hash: at, height: node.height }) => {
                    return (walked, node.height < finalized_height);
                }
                Some(node) => {
                    walked.push(at);
                    at = node.parent;
                }
                None => return (walked, true),
            }
        }
    }

    fn is_finalized(&self, hash: BlockHash) -> bool {
        self.finalized_headers.iter().any(|(_, record)| record.hash == hash)
    }

    fn known(&self, hash: BlockHash) -> bool {
        self.nodes.contains_key(&hash) || self.is_finalized(hash)
    }

    /// A child of `prev` = (its height, the work up to `prev`): a tree node or the finalized tip
    fn parent(&self, prev: BlockHash) -> Result<(BlockHeight, u128), Rejected> {
        if let Some(parent) = self.nodes.get(&prev) {
            Ok((parent.height + 1, parent.record.cumulative_work))
        } else if let Some((height, record)) =
            self.finalized_headers.back().filter(|(_, r)| r.hash == prev)
        {
            Ok((*height + 1, record.cumulative_work))
        } else if self.is_finalized(prev) {
            Err(Rejected::BelowFinalized)
        } else {
            Err(Rejected::Orphan)
        }
    }

    /// `prev` and its ancestors held, newest first, up to [`CONTEXT`]: tree nodes down to the
    /// finalized tip, then the finalized headers
    fn ancestors(&self, prev: BlockHash) -> Vec<Ancestor> {
        let ancestor = |record: &Record| Ancestor { bits: record.bits, time: record.time };
        let tree =
            std::iter::successors(self.nodes.get(&prev), |node| self.nodes.get(&node.parent));
        let finalized_headers =
            self.finalized_headers.iter().rev().map(|(_, record)| ancestor(record));
        tree.map(|node| ancestor(&node.record)).chain(finalized_headers).take(CONTEXT).collect()
    }
}

#[cfg(test)]
mod fire_drills;
