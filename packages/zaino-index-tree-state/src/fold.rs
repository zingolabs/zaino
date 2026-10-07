//! Blocks → `Changes`: note commitments appended onto each pool's frontier, read off the parent
//!
//! - one run = one `Hashable::combine_pairs` per level across every block of it
//!   (`Frontier::append_batch_visiting`; node types split wide levels across cores)
//! - pools fold concurrently; generic over the node type (ironwood reuses `MerkleHashOrchard`)
//! - node / subtree root → the block holding its last leaf: any split into runs = same `Changes`
//! - pure: parent state read through [`TreeStateReader`], nothing carried between calls

use std::collections::BTreeMap;

use incrementalmerkletree::{frontier::Frontier, Address, Hashable, Level};
use orchard::tree::MerkleHashOrchard;
use zaino_persistence::{Changes, SequenceId, SequenceRead};
use zaino_primitives::types::{
    Block, BlockRef, Height, PerPool, ShieldedPool, TreeRoot, TreeSizes,
};
use zcash_primitives::merkle_tree::HashSer;

use crate::{
    heights::{self, TreeStateHeight},
    level_table,
    nodes::{self, slot, Slot, MERKLE_DEPTH, NODE},
    schema, subtree_table,
    subtrees::{self, SubtreeEntry},
    TreeStateReader, HEIGHTS,
};

/// Level whose completion = one `GetSubtreeRoots` entry (2^16 leaves, the protocol's shard)
const SUBTREE_LEVEL: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    /// Parent's nodes will not rebuild a frontier of its recorded size (a fold bug)
    #[error("tree-state store is inconsistent at size {size}")]
    Inconsistent { size: u64 },

    /// Non-canonical field element off the wire
    #[error("block {height} carries an uncommittable note commitment")]
    Commitment { height: Height },
}

/// `block` on top of `parent` (= a run of one)
pub fn fold<V: SequenceRead>(
    parent: &TreeStateReader<V>,
    block: &Block,
) -> Result<Changes, FoldError> {
    let mut changes = fold_run(parent, &[block])?;
    Ok(changes.pop().expect("one Changes per block"))
}

/// `blocks` (contiguous, the first next above `parent`'s tip) hashed as one batch, split back into
/// one `Changes` per block: its height record, the nodes whose last leaf it holds, the subtree
/// roots it closes
///
/// - bulk sync's runs spread one level's hashing over every core, not one block's few leaves
/// - every leaf decoded and every frontier read before any hashing
pub(crate) fn fold_run<V: SequenceRead>(
    parent: &TreeStateReader<V>,
    blocks: &[&Block],
) -> Result<Vec<Changes>, FoldError> {
    let sizes = parent.tip_sizes();
    let sapling =
        PoolRun::<sapling_crypto::Node>::read(parent, sizes, blocks, ShieldedPool::Sapling)?;
    let orchard = PoolRun::<MerkleHashOrchard>::read(parent, sizes, blocks, ShieldedPool::Orchard)?;
    let ironwood =
        PoolRun::<MerkleHashOrchard>::read(parent, sizes, blocks, ShieldedPool::Ironwood)?;

    let subtree = Level::from(SUBTREE_LEVEL);
    let (sapling, (orchard, ironwood)) = rayon::join(
        || sapling.hash(subtree),
        || rayon::join(|| orchard.hash(subtree), || ironwood.hash(subtree)),
    );
    Ok(split(parent, sizes, blocks, &PerPool { sapling, orchard, ironwood }))
}

/// One pool's run: the parent's frontier + every leaf the run appends to it
///
/// - `ends` = `(height, leaves through that block)` per block, ascending
struct PoolRun<H> {
    frontier: Frontier<H, MERKLE_DEPTH>,
    leaves: Vec<H>,
    ends: Vec<(Height, usize)>,
}

/// One pool's nodes retained + subtrees completed by one block
#[derive(Debug, Clone, Default)]
struct Retained {
    nodes: BTreeMap<Slot, [u8; NODE]>,
    subtrees: BTreeMap<u64, SubtreeEntry>,
}

impl<H: Hashable + HashSer + Clone> PoolRun<H> {
    /// Frontier at `sizes` off `parent`, `blocks`' note commitments → nodes
    fn read<V: SequenceRead>(
        parent: &TreeStateReader<V>,
        sizes: TreeSizes,
        blocks: &[&Block],
        pool: ShieldedPool,
    ) -> Result<Self, FoldError> {
        let size = u64::from(sizes.get(pool).get());
        let frontier = parent.frontier(pool, size).ok_or(FoldError::Inconsistent { size })?;
        let mut leaves = Vec::new();
        let mut ends = Vec::with_capacity(blocks.len());
        for block in blocks {
            let height = block.header().height;
            let leaf =
                |bytes: [u8; 32]| nodes::decode(&bytes).ok_or(FoldError::Commitment { height });
            for tx in block.transactions() {
                match pool {
                    ShieldedPool::Sapling => {
                        for output in &tx.sapling.outputs {
                            leaves.push(leaf(output.cmu.into())?);
                        }
                    }
                    ShieldedPool::Orchard => {
                        for action in &tx.orchard.actions {
                            leaves.push(leaf(action.cmx.into())?);
                        }
                    }
                    ShieldedPool::Ironwood => {
                        for action in &tx.ironwood.actions {
                            leaves.push(leaf(action.cmx.into())?);
                        }
                    }
                }
            }
            ends.push((height, leaves.len()));
        }
        Ok(Self { frontier, leaves, ends })
    }

    /// Every leaf appended at once, each visited node under the block holding its last leaf
    ///
    /// - `subtree_level` = [`SUBTREE_LEVEL`] outside this module's tests (2^1..2^4 subtrees there)
    /// - infallible: consensus keeps every pool below 2^32 leaves (a depth-32 tree never fills)
    fn hash(self, subtree_level: Level) -> Vec<Retained> {
        let Self { mut frontier, leaves, ends } = self;
        let mut out = vec![Retained::default(); ends.len()];
        let start = frontier.tree_size();
        // visited nodes end inside the run
        let block_of = |leaf: u64| {
            let in_run = usize::try_from(leaf - start).expect("run fits usize");
            ends.partition_point(|&(_, through)| through <= in_run)
        };
        let appended = frontier.append_batch_visiting(leaves, |first, nodes| {
            for (index, node) in (first.index()..).zip(nodes) {
                let addr = Address::from_parts(first.level(), index);
                let block = block_of(u64::from(addr.max_position()));
                if let Some(slot) = slot(addr) {
                    out[block].nodes.insert(slot, nodes::encode(node));
                }
                if addr.level() == subtree_level {
                    let (root, end_height) = (TreeRoot::from(nodes::encode(node)), ends[block].0);
                    out[block].subtrees.insert(index, SubtreeEntry { root, end_height });
                }
            }
        });
        assert!(appended, "commitment tree full");
        out
    }
}

/// `retained` per pool per block → one `Changes` per block, each table appended at its end
fn split<V: SequenceRead>(
    parent: &TreeStateReader<V>,
    mut sizes: TreeSizes,
    blocks: &[&Block],
    retained: &PerPool<Vec<Retained>>,
) -> Vec<Changes> {
    let schema = schema(parent.network());
    let mut ends: Vec<u64> = schema.sequence_ids().map(|table| parent.len(table)).collect();
    let mut changes = Vec::with_capacity(blocks.len());
    for (at, block) in blocks.iter().enumerate() {
        let header = block.header();
        sizes = sizes.advance(block).expect("pool size within u32 (consensus caps it)");
        let mut block_changes =
            Changes::new(BlockRef { hash: header.hash, height: header.height }, &schema);
        let record = TreeStateHeight { hash: header.hash, time: header.time, sizes };
        block_changes.append(HEIGHTS, &heights::encode(&record));
        for pool in ShieldedPool::ALL {
            let Retained { nodes, subtrees } = &retained.get(pool)[at];
            for level in 0..MERKLE_DEPTH {
                let run = nodes.range((level, 0)..(level + 1, 0));
                let run = run.map(|(&(_, slot), node)| (slot, node));
                append_in_order(&mut block_changes, &mut ends, level_table(pool, level), run);
            }
            let entries = subtrees.iter().map(|(&index, entry)| (index, subtrees::encode(entry)));
            append_in_order(&mut block_changes, &mut ends, subtree_table(pool), entries);
        }
        changes.push(block_changes);
    }
    changes
}

/// `records` appended to `table`, each at its own slot (slot != the table's end = a fold bug)
fn append_in_order<R: AsRef<[u8]>>(
    changes: &mut Changes,
    ends: &mut [u64],
    table: SequenceId,
    records: impl Iterator<Item = (u64, R)>,
) {
    let end = &mut ends[usize::from(table.0)];
    for (slot, record) in records {
        assert_eq!(slot, *end, "{table:?}: slot {slot} appended at {end}");
        changes.append(table, record.as_ref());
        *end += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use proptest::prelude::*;
    use zaino_persistence::{fs::SimFs, DiskEngine, PersistenceEngine, Store};
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        BlockHash, CompactCiphertext, OrchardAction, OrchardData, SaplingData, SaplingOutput,
        Transaction, TransactionId,
    };
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::nodes::retained_nodes;

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

        /// Random blocks split into random runs, each run's frontier read back off the nodes
        /// the runs before it retained: retained nodes + subtree roots = the naive tree's, each
        /// under the block holding its last leaf; every frontier read = the library's
        /// leaf-by-leaf one at that size
        #[test]
        fn any_split_into_runs_retains_the_naive_trees_nodes_reading_each_frontier_back(
            blocks in proptest::collection::vec(0usize..40, 1..60),
            runs in proptest::collection::vec(1usize..12, 1..60),
            subtree_level in 1u8..5,
        ) {
            let total: usize = blocks.iter().sum();
            let height = |n: usize| Height::try_from(u32::try_from(n).expect("n")).expect("h");
            let library = |size: u64| {
                let mut frontier = Frontier::<Mix, MERKLE_DEPTH>::empty();
                (0..size).all(|n| frontier.append(leaf(n))).then_some(frontier)
            };

            let schema = schema(NetworkType::Regtest);
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/ts"), &schema);
            let mut store = store.expect("empty store");
            let mut out = Vec::new();
            let (mut next_block, mut next_leaf) = (0usize, 0u64);
            for (run, length) in runs.iter().cycle().enumerate() {
                if next_block == blocks.len() {
                    break;
                }
                let these = &blocks[next_block..(next_block + length).min(blocks.len())];
                let parent = TreeStateReader::new(store.staged(), NetworkType::Regtest);
                let frontier = parent.frontier::<Mix>(ShieldedPool::Sapling, next_leaf);
                prop_assert_eq!(&frontier, &library(next_leaf), "frontier read before run {}", run);
                let mut leaves = Vec::new();
                let mut ends = Vec::new();
                for (offset, count) in these.iter().enumerate() {
                    leaves.extend((0..*count as u64).map(|i| leaf(next_leaf + i)));
                    next_leaf += *count as u64;
                    ends.push((height(next_block + offset), leaves.len()));
                }
                let frontier = frontier.expect("read");
                let retained = PoolRun { frontier, leaves, ends }.hash(Level::from(subtree_level));

                // the run's nodes as one "block" per run (heights = run number)
                let tip = BlockRef { hash: BlockHash::from([0; 32]), height: height(run) };
                let mut changes = Changes::new(tip, &schema);
                let nodes: BTreeMap<_, _> = retained.iter().flat_map(|block| &block.nodes).collect();
                for (&(level, _), node) in nodes {
                    changes.append(level_table(ShieldedPool::Sapling, level), node);
                }
                store.apply(changes);
                out.extend(retained);
                next_block += these.len();
            }
            let parent = TreeStateReader::new(store.staged(), NetworkType::Regtest);
            let last = parent.frontier::<Mix>(ShieldedPool::Sapling, total as u64);
            prop_assert_eq!(last, library(total as u64), "frontier after every run");

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
                        expected_nodes[block].insert((level, slot), nodes::encode(node));
                    }
                    if level == subtree_level {
                        expected_subtrees[block].insert(index, (nodes::encode(node), height(block)));
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
        }
    }

    /// Five real blocks (0, 1, many commitments per pool, both parities) folded in every split
    /// into runs, each run on a parent holding the runs before it: every block's `Changes` =
    /// folding it alone on its parent, table by table
    #[test]
    fn a_run_folds_to_the_same_changes_as_its_blocks_one_by_one() {
        let commitment = |seed: u32| {
            let mut bytes = [0u8; 32];
            bytes[..4].copy_from_slice(&seed.to_le_bytes());
            bytes
        };
        let action = |seed: &u32| OrchardAction {
            nullifier: [4u8; 32].into(),
            cmx: commitment(*seed).into(),
            ephemeral_key: [6u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([7u8; CompactCiphertext::LENGTH]),
        };
        let chain: Vec<Arc<Block>> = linked(
            [
                (&[1, 2, 3][..], &[101][..], &[][..]),
                (&[4], &[102, 103], &[201]),
                (&[], &[], &[]),
                (&[5, 6], &[104, 105, 106, 107], &[202, 203]),
                (&[7, 8, 9, 10, 11], &[], &[204]),
            ]
            .into_iter()
            .zip(0u32..)
            .map(|((sapling, orchard, ironwood), seed)| {
                vec![Transaction {
                    txid: TransactionId::from(commitment(seed)),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: SaplingData {
                        outputs: (sapling.iter())
                            .map(|seed| SaplingOutput {
                                cmu: commitment(*seed).into(),
                                ephemeral_key: [2u8; 32].into(),
                                enc_ciphertext: CompactCiphertext::from(
                                    [3u8; CompactCiphertext::LENGTH],
                                ),
                            })
                            .collect(),
                        ..Default::default()
                    },
                    orchard: OrchardData {
                        actions: orchard.iter().map(action).collect(),
                        ..Default::default()
                    },
                    ironwood: OrchardData {
                        actions: ironwood.iter().map(action).collect(),
                        ..Default::default()
                    },
                }]
            }),
        );
        let schema = schema(NetworkType::Regtest);
        // (tip, every table's appends) per block
        let tables = |changes: &Changes| {
            let appends = schema
                .sequence_ids()
                .map(|table| changes.appends(table).map(<[u8]>::to_vec).collect::<Vec<_>>());
            (changes.tip(), appends.collect::<Vec<_>>())
        };
        let empty = || {
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/ts"), &schema);
            store.expect("empty store")
        };
        let mut store = empty();
        let one_by_one: Vec<_> = chain
            .iter()
            .map(|block| {
                let parent = TreeStateReader::new(store.staged(), NetworkType::Regtest);
                let changes = fold(&parent, block).expect("folds");
                let folded = tables(&changes);
                store.apply(changes);
                folded
            })
            .collect();

        // bit `i` of `split` = a run ends after block `i`
        let fold_in_runs = |split: u32| {
            let mut store = empty();
            let mut folded = Vec::new();
            let mut run: Vec<&Block> = Vec::new();
            for (at, block) in chain.iter().enumerate() {
                run.push(block);
                if split & (1 << at) == 0 && at + 1 < chain.len() {
                    continue;
                }
                let parent = TreeStateReader::new(store.staged(), NetworkType::Regtest);
                for changes in fold_run(&parent, &std::mem::take(&mut run)).expect("folds") {
                    folded.push(tables(&changes));
                    store.apply(changes);
                }
            }
            folded
        };

        for split in 0..1 << (chain.len() - 1) {
            assert_eq!(fold_in_runs(split), one_by_one, "runs split by {split:#06b}");
        }
    }

    /// Non-canonical commitment anywhere in a run → the run refused, naming its block
    #[test]
    fn an_uncommittable_note_commitment_refuses_the_run_naming_its_block() {
        // 0x01.. < both moduli, 0xff.. above both
        let chain: Vec<Arc<Block>> =
            linked([[0x01; 32], [0xff; 32]].into_iter().zip(0u8..).map(|(cmu, tag)| {
                let output = SaplingOutput {
                    cmu: cmu.into(),
                    ephemeral_key: [2u8; 32].into(),
                    enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
                };
                vec![Transaction {
                    txid: TransactionId::from([tag; 32]),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: SaplingData { outputs: vec![output], ..Default::default() },
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }]
            }));
        let store = DiskEngine::new(SimFs::new())
            .open(Path::new("/ts"), &schema(NetworkType::Regtest))
            .expect("empty store");
        let parent = TreeStateReader::new(store.staged(), NetworkType::Regtest);
        let run: Vec<&Block> = chain.iter().map(|block| &**block).collect();
        let refused = FoldError::Commitment { height: Height::GENESIS.next() };
        assert_eq!(fold_run(&parent, &run).err(), Some(refused));
    }
}
