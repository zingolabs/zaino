//! Folded blocks above the root: one [`Node`] per block, keyed by hash (`nfs.md` §6)
//!
//! - Best nodes = one contiguous run from the root (folded parent first)
//! - Side nodes = earlier best runs, kept while the header chain holds them (`holds`: side
//!   branches forking at or above the final tip, its own H4 bound)

use std::sync::Arc;

use zaino_header_chain::VerifiedChain;
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};

use crate::fetch::merkle_root;
use crate::snapshot::Branch;

#[derive(Debug)]
pub(crate) struct Node<F> {
    pub(crate) at: BlockRef,
    pub(crate) parent: BlockHash,
    pub(crate) block: Arc<Block>,
    pub(crate) folded: Arc<F>,
}

/// O(1) clone: one per published snapshot
#[derive(Debug)]
pub(crate) struct Graph<F> {
    nodes: imbl::HashMap<BlockHash, Arc<Node<F>>>,
}

impl<F> Clone for Graph<F> {
    fn clone(&self) -> Self {
        Self { nodes: self.nodes.clone() }
    }
}

/// `at()`'s answer: `folded` = `None` at the root (committed views alone)
#[derive(Debug)]
pub(crate) struct Base<F> {
    pub(crate) at: BlockRef,
    pub(crate) branch: Branch,
    pub(crate) folded: Option<Arc<F>>,
}

impl<F> Graph<F> {
    pub(crate) fn new() -> Self {
        Self { nodes: imbl::HashMap::new() }
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

    /// Kept iff above `root` (every index holds the rest) and held by `chain` (G8)
    pub(crate) fn prune(&mut self, chain: &VerifiedChain, root: Option<BlockRef>) {
        let root_height = root.map(|root| root.height);
        self.nodes.retain(|_, node| Some(node.at.height) > root_height && chain.holds(node.at));
    }

    /// `hash` = the root or a node (`None` = neither: final below the root, never folded, unknown)
    pub(crate) fn at(
        &self,
        chain: &VerifiedChain,
        root: Option<BlockRef>,
        hash: &BlockHash,
    ) -> Option<Base<F>> {
        if let Some(root) = root.filter(|root| root.hash == *hash) {
            return Some(Base { at: root, branch: Branch::Best, folded: None });
        }
        let node = self.nodes.get(hash)?;
        let folded = Some(Arc::clone(&node.folded));
        Some(Base { at: node.at, branch: self.branch(chain, node), folded })
    }

    /// Best, or side from its first ancestor on `chain`'s best (a best node or the root: N2)
    fn branch(&self, chain: &VerifiedChain, node: &Node<F>) -> Branch {
        if on_best(chain, node.at) {
            return Branch::Best;
        }
        let mut lowest = node;
        while let Some(parent) = self.nodes.get(&lowest.parent).filter(|p| !on_best(chain, p.at)) {
            lowest = parent;
        }
        let height = lowest.at.height.checked_sub(1).expect("a side node sits above the root");
        Branch::Side { from: BlockRef { hash: lowest.parent, height } }
    }

    /// N1, N2, G8; panics naming the invariant broken
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
            let held = chain.holds(node.at);
            assert!(held, "G8: every node on the best chain or a side branch it holds");
        }
    }
}

pub(crate) fn on_best(chain: &VerifiedChain, at: BlockRef) -> bool {
    chain.hash_at(at.height) == Some(at.hash)
}
