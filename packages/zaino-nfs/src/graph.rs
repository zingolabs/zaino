//! Folded blocks above the root: one [`Node`] per block, keyed by hash (`nfs.md` §6)
//!
//! - Best nodes = one contiguous run from the root (folded parent first)
//! - Side nodes = earlier best runs, kept while the header chain can still pick them (fork at or
//!   above the final tip), at most [`SIDE_NODES_PER_DEPTH`] · depth

use std::collections::HashSet;
use std::sync::Arc;

use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};

use crate::fetch::merkle_root;

/// Header chain's own side bound (`zaino-header-chain` H4)
const SIDE_NODES_PER_DEPTH: usize = 4;

#[derive(Debug)]
pub(crate) struct Node<F> {
    pub(crate) at: BlockRef,
    pub(crate) parent: BlockHash,
    pub(crate) block: Arc<Block>,
    pub(crate) folded: Arc<F>,
}

pub(crate) struct Graph<F> {
    nodes: imbl::HashMap<BlockHash, Arc<Node<F>>>,
    max_side: usize,
}

impl<F> Graph<F> {
    pub(crate) fn new(depth: u32) -> Self {
        Self { nodes: imbl::HashMap::new(), max_side: SIDE_NODES_PER_DEPTH * depth as usize }
    }

    pub(crate) fn get(&self, hash: &BlockHash) -> Option<&Arc<Node<F>>> {
        self.nodes.get(hash)
    }

    pub(crate) fn contains(&self, hash: &BlockHash) -> bool {
        self.nodes.contains_key(hash)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn nodes(&self) -> impl Iterator<Item = &Arc<Node<F>>> {
        self.nodes.values()
    }

    #[cfg(test)]
    pub(crate) fn remove(&mut self, hash: &BlockHash) {
        self.nodes.remove(hash);
    }

    pub(crate) fn insert(&mut self, node: Node<F>) {
        self.nodes.insert(node.at.hash, Arc::new(node));
    }

    /// `parent` = the block a node at `height` folds on: a held node or the root
    pub(crate) fn holds_parent(
        &self,
        root: Option<BlockRef>,
        parent: BlockHash,
        height: Height,
    ) -> bool {
        let node = self.nodes.get(&parent).is_some_and(|node| node.at.height.next() == height);
        let root = match root {
            Some(root) => root.hash == parent && root.height.next() == height,
            None => height == Height::GENESIS,
        };
        node || root
    }

    /// Deepest best node of the run from the root (`None` = no best node)
    pub(crate) fn best_top(
        &self,
        chain: &VerifiedChain,
        root: Option<BlockRef>,
    ) -> Option<&Arc<Node<F>>> {
        let from = root.map_or(Height::GENESIS, |root| root.height.next());
        let run = from.up_to(chain.best().height).map(|height| chain.hash_at(height));
        run.map_while(|hash| self.nodes.get(&hash?)).last()
    }

    /// Nodes at or below `root` gone (every index holds them), then dead and surplus side nodes
    pub(crate) fn prune(&mut self, chain: &VerifiedChain, root: Option<BlockRef>) {
        let root_height = root.map(|root| root.height);
        self.nodes.retain(|_, node| Some(node.at.height) > root_height);
        let held = self.held(chain, root);
        self.nodes.retain(|hash, _| held.contains(hash));
        while self.side(chain).count() > self.max_side {
            let parents: HashSet<BlockHash> = self.nodes.values().map(|node| node.parent).collect();
            let leaves = self.side(chain).filter(|node| !parents.contains(&node.at.hash));
            let lowest = leaves.map(|node| (node.at.height, node.at.hash)).min();
            let (_, hash) = lowest.expect("past the bound: a side leaf exists");
            self.nodes.remove(&hash);
        }
    }

    /// Best nodes + side nodes forking at or above the final tip (lower = the header chain pruned
    /// their branch)
    fn held(&self, chain: &VerifiedChain, root: Option<BlockRef>) -> HashSet<BlockHash> {
        let final_height = chain.final_tip().map(|tip| tip.height);
        let mut nodes: Vec<&Arc<Node<F>>> = self.nodes.values().collect();
        nodes.sort_by_key(|node| node.at.height);
        let mut held = HashSet::new();
        for node in nodes {
            let keep = on_best(chain, node.at)
                || match self.nodes.get(&node.parent) {
                    Some(parent) if on_best(chain, parent.at) => {
                        Some(parent.at.height) >= final_height
                    }
                    Some(parent) => held.contains(&parent.at.hash),
                    None => {
                        root.is_some_and(|root| root.hash == node.parent)
                            && root == chain.final_tip()
                    }
                };
            if keep {
                held.insert(node.at.hash);
            }
        }
        held
    }

    fn side<'a>(&'a self, chain: &'a VerifiedChain) -> impl Iterator<Item = &'a Arc<Node<F>>> {
        self.nodes.values().filter(|node| !on_best(chain, node.at))
    }

    /// N1, N2 and the side bounds; panics naming the invariant broken
    pub(crate) fn check(&self, chain: &VerifiedChain, root: Option<BlockRef>) {
        for node in self.nodes.values() {
            let header = node.block.header();
            let at = BlockRef { hash: header.hash, height: header.height };
            assert_eq!(at, node.at, "N1: every node's block = its own header");
            assert_eq!(header.prev_hash, node.parent, "N1: every node's block = its own header");
            let body = merkle_root(&node.block) == Some(header.merkle_root);
            assert!(body, "N1: every node's body = its header's merkle root");
            let above = Some(node.at.height) > root.map(|root| root.height);
            assert!(above, "N2: nodes only above the root");
            let held = self.holds_parent(root, node.parent, node.at.height);
            assert!(held, "N2: every node folds on a held parent");
        }
        let held = self.held(chain, root);
        let forks = self.nodes.keys().all(|hash| held.contains(hash));
        assert!(forks, "graph: every side node forks at or above the final tip");
        let side = self.side(chain).count();
        assert!(side <= self.max_side, "graph: {side} side nodes past the bound");
    }
}

pub(crate) fn on_best(chain: &VerifiedChain, at: BlockRef) -> bool {
    chain.hash_at(at.height) == Some(at.hash)
}
