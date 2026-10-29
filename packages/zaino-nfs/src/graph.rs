//! Folded blocks above the root: one [`Node`] per block, keyed by hash (`usage.md`)
//!
//! - Best nodes = one contiguous run from the root (folded parent first)
//! - Side nodes = earlier best runs, kept while the header chain holds them (`holds`: side
//!   branches forking at or above the finalized tip, its own H4 bound)

use std::sync::Arc;

use zaino_header_chain::HeaderChainSnapshot;
use zaino_primitives::types::{Block, BlockRef};
use zcash_protocol::consensus::{BlockHeight, H0};
use zcash_protocol::BlockHash;

#[derive(Debug)]
pub(crate) struct Node<F> {
    pub(crate) at: BlockRef,
    pub(crate) parent: BlockHash,
    pub(crate) block: Arc<Block>,
    pub(crate) folded: Arc<F>,
}

/// O(1) clone: one per publication
#[derive(Debug)]
pub(crate) struct Graph<F> {
    nodes: imbl::HashMap<BlockHash, Arc<Node<F>>>,
}

impl<F> Clone for Graph<F> {
    fn clone(&self) -> Self {
        Self { nodes: self.nodes.clone() }
    }
}

/// `at()`'s answer: `folded` = `None` at the root (durable views alone)
#[derive(Debug)]
pub(crate) struct Base<F> {
    pub(crate) at: BlockRef,
    pub(crate) on_best: bool,
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

    #[cfg(test)]
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
        height: BlockHeight,
    ) -> bool {
        let node = self.nodes.get(&parent).is_some_and(|node| node.at.height + 1 == height);
        let root = match root {
            Some(root) => root.hash == parent && root.height + 1 == height,
            None => height == H0,
        };
        node || root
    }

    /// Deepest best node of the run from the root (`None` = no best node)
    pub(crate) fn best_top(
        &self,
        chain: &HeaderChainSnapshot,
        root: Option<BlockRef>,
    ) -> Option<&Arc<Node<F>>> {
        let from = root.map_or(0, |root| u32::from(root.height) + 1);
        let heights = (from..=u32::from(chain.best().height)).map(BlockHeight::from);
        let run = heights.map(|height| chain.hash_at(height));
        run.map_while(|hash| self.nodes.get(&hash?)).last()
    }

    /// Kept iff above `root` (every index holds the rest) and held by `chain` (N8)
    pub(crate) fn prune(&mut self, chain: &HeaderChainSnapshot, root: Option<BlockRef>) {
        let root_height = root.map(|root| root.height);
        self.nodes.retain(|_, node| Some(node.at.height) > root_height && chain.holds(node.at));
    }

    /// `hash` = the root or a node (`None` = neither: finalized below the root, never folded,
    /// unknown)
    pub(crate) fn at(
        &self,
        chain: &HeaderChainSnapshot,
        root: Option<BlockRef>,
        hash: &BlockHash,
    ) -> Option<Base<F>> {
        if let Some(root) = root.filter(|root| root.hash == *hash) {
            return Some(Base { at: root, on_best: true, folded: None });
        }
        let node = self.nodes.get(hash)?;
        let folded = Some(Arc::clone(&node.folded));
        Some(Base { at: node.at, on_best: chain.on_best(node.at), folded })
    }

    /// N1, N2, N8 (panics naming the invariant broken)
    #[cfg(test)]
    pub(crate) fn check(&self, chain: &HeaderChainSnapshot, root: Option<BlockRef>) {
        for node in self.nodes.values() {
            let header = node.block.header();
            let at = BlockRef { hash: header.hash, height: header.height };
            let node_at = node.at.height;
            let message = "every node's block = its own header (N1): block (left) vs node (right)";
            assert_eq!(at, node.at, "node at {node_at:?}: {message}");
            assert_eq!(header.prev_hash, node.parent, "node at {node_at:?}: {message}");
            let body = node.block.check_body();
            let message = "every node's body = its header's merkle root (N1)";
            assert_eq!(body, Ok(()), "node at {node_at:?}: {message}");
            let root_height = root.map(|root| root.height);
            let above = Some(node.at.height) > root_height;
            let message = "nodes only above the root (N2)";
            assert!(above, "node at {node_at:?}, root {root_height:?}: {message}");
            let held = self.holds_parent(root, node.parent, node.at.height);
            let (parent, message) = (node.parent, "every node folds on a held parent (N2)");
            assert!(held, "node at {node_at:?}, parent {parent} gone: {message}");
            let held = chain.holds(node.at);
            assert!(held, "N8: every node on the best chain or a side branch it holds");
        }
    }
}
