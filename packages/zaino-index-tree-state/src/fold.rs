//! One running frontier per pool: nodes a batch of appends retains, subtree roots it completes
//!
//! - generic over the node type: all three pools share one code path (ironwood reuses orchard's
//!   `MerkleHashOrchard`)
//! - hashing = `Frontier::append_batch_visiting`: one `Hashable::combine_pairs` per level (the node
//!   types split wide levels across cores)
//! - folds into a [`Folded`] (no file access; the writer turns it into the block's `Changes`)

use std::collections::BTreeMap;

use incrementalmerkletree::{
    frontier::{Frontier, NonEmptyFrontier},
    Address, Hashable, Level, Position, Source,
};
use zaino_persistence::SequenceRead;
use zaino_primitives::types::{Height, TreeRoot};

use zcash_primitives::merkle_tree::HashSer;

use crate::{
    nodes::{slot, NodeView, Slot, MERKLE_DEPTH, NODE},
    subtrees::SubtreeEntry,
};

/// Level whose completion = one `GetSubtreeRoots` entry (2^16 leaves, the protocol's shard)
pub(crate) const SUBTREE_LEVEL: u8 = 16;

/// One pool's fold output for one block: nodes retained, subtrees completed (both in append order)
#[derive(Debug, Clone, Default)]
pub(crate) struct Folded {
    pub(crate) nodes: BTreeMap<Slot, [u8; NODE]>,
    pub(crate) subtrees: BTreeMap<u64, SubtreeEntry>,
}

/// Frontier of `nodes`' tree
///
/// - no hashing: every ommer = a retained left sibling, leaf = `node(0, position)`
/// - `None` = a node missing or non-canonical (the stored tree is corrupt)
pub(crate) fn frontier_at<H: Hashable + HashSer + Clone, V: SequenceRead>(
    nodes: NodeView<'_, V>,
) -> Option<Frontier<H, MERKLE_DEPTH>> {
    let Some(last) = nodes.size.checked_sub(1) else {
        return Some(Frontier::empty());
    };
    let position = Position::from(last);
    let node = |addr| node_from_bytes::<H>(&nodes.get(addr)?);

    // `witness_addrs` ascends by level, `Source::Past(i)` indexes `ommers[i]` in that same order
    // (iteration order = the contract, not a coincidence)
    let ommers = position
        .witness_addrs(position.root_level())
        .filter(|(_, source)| matches!(source, Source::Past(_)))
        .map(|(addr, _)| node(addr))
        .collect::<Option<Vec<H>>>()?;
    let leaf = node(Address::from(position))?;

    NonEmptyFrontier::from_parts(position, leaf, ommers).and_then(Frontier::try_from).ok()
}

/// `None` = non-canonical field element
pub(crate) fn node_from_bytes<H: HashSer>(bytes: &[u8; NODE]) -> Option<H> {
    H::read(&bytes[..]).ok()
}

pub(crate) fn encode<H: HashSer>(node: &H) -> [u8; NODE] {
    let mut bytes = [0u8; NODE];
    node.write(&mut bytes[..]).expect("a commitment tree node serializes to exactly 32 bytes");
    bytes
}

/// One pool's carry: the frontier every later batch extends
///
/// - `subtree_level` = [`SUBTREE_LEVEL`] outside this module's tests (they fold 2^1..2^4 subtrees)
#[derive(Debug, Clone)]
pub(crate) struct PoolFold<H> {
    frontier: Frontier<H, MERKLE_DEPTH>,
    subtree_level: Level,
}

impl<H: Hashable + HashSer + Clone> PoolFold<H> {
    /// Carry at `nodes`' size, rebuilt through [`frontier_at`] (read, restart and reorg all land
    /// here: a reconstruction bug cannot hide in the rare path)
    pub(crate) fn seed<V: SequenceRead>(nodes: NodeView<'_, V>) -> Option<Self> {
        let frontier = frontier_at::<H, V>(nodes)?;
        Some(Self { frontier, subtree_level: Level::from(SUBTREE_LEVEL) })
    }

    pub(crate) fn size(&self) -> u64 {
        self.frontier.tree_size()
    }

    /// Appends `leaves` (every block of a batch at once: one `combine_pairs` per level) into
    /// `out`, one [`Folded`] per block: each node the grown tree retains and each subtree it
    /// completes, under the block holding its last leaf
    ///
    /// - `blocks` = `(height, leaves through that block)` ascending, as long as `out`
    /// - output = pure function of (start size, leaves): any split into batches retains the same
    /// - infallible: consensus keeps every pool below 2^32 leaves (a depth-32 tree never fills)
    pub(crate) fn append_batch(
        &mut self,
        leaves: Vec<H>,
        blocks: &[(Height, usize)],
        out: &mut [Folded],
    ) {
        assert_eq!(blocks.len(), out.len(), "one fold output per block");
        let span = Span { start: self.size(), blocks };
        let subtree_level = self.subtree_level;
        let appended = self.frontier.append_batch_visiting(leaves, |first, nodes| {
            for (index, node) in (first.index()..).zip(nodes) {
                let addr = Address::from_parts(first.level(), index);
                let block = span.block_of(u64::from(addr.max_position()));
                if let Some(slot) = slot(addr) {
                    out[block].nodes.insert(slot, encode(node));
                }
                if addr.level() == subtree_level {
                    let (root, end_height) = (TreeRoot::from(encode(node)), blocks[block].0);
                    out[block].subtrees.insert(index, SubtreeEntry { root, end_height });
                }
            }
        });
        assert!(appended, "commitment tree full");
    }
}

/// Leaves a batch appends from tree size `start`; `blocks` = `(height, leaves through it)` ascending
struct Span<'a> {
    start: u64,
    blocks: &'a [(Height, usize)],
}

impl Span<'_> {
    /// Index into `blocks` of the block holding tree position `leaf` (visited nodes end inside
    /// the batch)
    fn block_of(&self, leaf: u64) -> usize {
        let in_batch = usize::try_from(leaf - self.start).expect("batch fits usize");
        self.blocks.partition_point(|&(_, through)| through <= in_batch)
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path};

    use proptest::prelude::*;
    use zaino_persistence::{fs::SimFs, Changes, DiskEngine, PersistenceEngine, Tiered};
    use zaino_primitives::types::{BlockHash, BlockRef, ShieldedPool};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{level_table, nodes::retained_nodes, schema};

    /// Cheap, order- and level-sensitive stand-in hash (trees past 2^16 leaves in milliseconds)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Mix([u8; 32]);

    impl Hashable for Mix {
        fn empty_leaf() -> Self {
            Mix([0; 32])
        }
        fn combine(level: Level, a: &Self, b: &Self) -> Self {
            let mut out = [0u8; 32];
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = a.0[i].wrapping_mul(31).wrapping_add(b.0[(i + 7) % 32].rotate_left(3))
                    ^ u8::from(level).wrapping_add(i as u8);
            }
            Mix(out)
        }
    }

    impl HashSer for Mix {
        fn read<R: std::io::Read>(mut reader: R) -> std::io::Result<Self> {
            let mut bytes = [0u8; 32];
            reader.read_exact(&mut bytes)?;
            Ok(Mix(bytes))
        }
        fn write<W: std::io::Write>(&self, mut writer: W) -> std::io::Result<()> {
            writer.write_all(&self.0)
        }
    }

    fn leaf(n: u64) -> Mix {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&n.to_le_bytes());
        Mix(bytes)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// Random batches of random blocks: retained nodes + subtree roots = the naive tree's,
        /// each under the block holding its last leaf, the frontier = the library's leaf-by-leaf
        /// one, whatever the split
        #[test]
        fn any_split_into_batches_retains_the_naive_trees_nodes_and_frontier(
            blocks in proptest::collection::vec(0usize..40, 1..60),
            batches in proptest::collection::vec(1usize..12, 1..60),
            subtree_level in 1u8..5,
        ) {
            let total: usize = blocks.iter().sum();
            let height = |n: usize| Height::try_from(u32::try_from(n).expect("n")).expect("h");

            let mut fold = PoolFold::<Mix> {
                frontier: Frontier::empty(),
                subtree_level: Level::from(subtree_level),
            };
            let mut out = vec![Folded::default(); blocks.len()];
            let (mut next_block, mut next_leaf) = (0usize, 0u64);
            for batch in batches.iter().cycle() {
                if next_block == blocks.len() {
                    break;
                }
                let these = &blocks[next_block..(next_block + batch).min(blocks.len())];
                let mut leaves = Vec::new();
                let mut ends = Vec::new();
                for (offset, count) in these.iter().enumerate() {
                    leaves.extend((0..*count as u64).map(|i| leaf(next_leaf + i)));
                    next_leaf += *count as u64;
                    ends.push((height(next_block + offset), leaves.len()));
                }
                fold.append_batch(leaves, &ends, &mut out[next_block..next_block + these.len()]);
                next_block += these.len();
            }

            let mut library = Frontier::<Mix, MERKLE_DEPTH>::empty();
            for n in 0..total as u64 {
                prop_assert!(library.append(leaf(n)));
            }
            prop_assert_eq!(&fold.frontier, &library, "frontier after every batch");

            let block_of_leaf: Vec<usize> =
                blocks.iter().enumerate().flat_map(|(b, &c)| std::iter::repeat_n(b, c)).collect();
            let mut expected_nodes = vec![BTreeMap::new(); blocks.len()];
            let mut expected_subtrees = vec![BTreeMap::new(); blocks.len()];
            let mut level_nodes: Vec<Mix> = (0..total as u64).map(leaf).collect();
            for level in 0..MERKLE_DEPTH {
                let retained = retained_nodes(level, total as u64);
                for (index, node) in (0u64..).zip(&level_nodes) {
                    let last = usize::try_from(((index + 1) << level) - 1).expect("leaf");
                    let block = block_of_leaf[last];
                    if level == 0 || (index % 2 == 0 && index / 2 < retained) {
                        let slot = if level == 0 { index } else { index / 2 };
                        expected_nodes[block].insert((level, slot), encode(node));
                    }
                    if level == subtree_level {
                        expected_subtrees[block].insert(index, (encode(node), height(block)));
                    }
                }
                level_nodes = level_nodes
                    .chunks_exact(2)
                    .map(|pair| Mix::combine(Level::from(level), &pair[0], &pair[1]))
                    .collect();
            }
            let folded: Vec<_> = out.iter().map(|block| block.nodes.clone()).collect();
            prop_assert_eq!(folded, expected_nodes, "retained nodes, per block");
            let subtrees: Vec<BTreeMap<_, _>> = out
                .iter()
                .map(|block| {
                    let entries = block.subtrees.iter();
                    entries.map(|(i, e)| (*i, (<[u8; 32]>::from(e.root), e.end_height))).collect()
                })
                .collect();
            prop_assert_eq!(subtrees, expected_subtrees, "subtree roots, per block");

            // the retained nodes, held as one block's appends, read back as the same frontier
            let schema = schema(NetworkType::Regtest);
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/ts"), &schema);
            let mut tiered = Tiered::new(store.expect("empty store"), NonZeroUsize::MIN);
            let tip = BlockRef { hash: BlockHash::from([0; 32]), height: Height::GENESIS };
            let mut changes = Changes::new(tip, tiered.schema());
            let every: BTreeMap<_, _> = out.iter().flat_map(|block| block.nodes.clone()).collect();
            for (&(level, _), node) in &every {
                changes.append(level_table(ShieldedPool::Sapling, level), node);
            }
            tiered.apply(changes);
            let view = tiered.view();
            let nodes = NodeView { view: &view, pool: ShieldedPool::Sapling, size: total as u64 };
            prop_assert_eq!(frontier_at::<Mix, _>(nodes), Some(library));
        }
    }
}
