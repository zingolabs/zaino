//! tree_state writer: the final stream → one [`fold_run`] per run → its store
//!
//! - one run = one `Hashable::combine_pairs` per level across every block of it
//!   (`Frontier::append_batch_visiting`; node types split wide levels across cores)
//! - pools fold concurrently; generic over the node type (ironwood reuses `MerkleHashOrchard`)
//! - node / subtree root → the block holding its last leaf: any split into runs = same deltas
//! - nothing carried between runs: each reads its parent frontiers off `Store::staged`

use std::{collections::BTreeMap, slice};

use incrementalmerkletree::{frontier::Frontier, Address, Hashable, Level};
use orchard::tree::MerkleHashOrchard;
use zaino_persistence::{BlockChanges, IndexKind, SequenceRead, SequenceTable, Store, View};
use zaino_primitives::types::{Block, Height, PerPool, ShieldedPool, TreeRoot, TreeSizes};
use zaino_sync::{apply, blocking, commit, held, IndexHandle, IndexPublisher, Subscription};
use zcash_primitives::merkle_tree::HashSer;

use crate::{
    heights::{self, TreeStateHeight},
    level_table,
    nodes::{self, slot, Slot, MERKLE_DEPTH, NODE},
    subtree_table,
    subtrees::{self, SubtreeEntry},
    TreeStateReader, HEIGHTS,
};

const NAME: &str = IndexKind::TreeState.name();

/// Level whose completion = one `GetSubtreeRoots` entry (2^16 leaves, the protocol's shard)
const SUBTREE_LEVEL: u8 = 16;

pub struct TreeStateIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: SequenceRead>> TreeStateIndexWriter<S> {
    /// Over `store` (opened with [`TABLES`](crate::TABLES)) at its committed tip
    pub fn new(store: S) -> Self {
        let view = store.committed();
        let held = view.tip().map_or(0, |tip| u64::from(tip.height) + 1);
        assert_eq!(view.sequence(HEIGHTS).count(), held, "{NAME}: one record per committed height");
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    /// For `Nfs::add`: committed view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    ///
    /// - one `fold_run` per run: every block the store lacks, hashed as one batch
    /// - commits after the final tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            store = blocking(move || {
                let fresh = run.blocks.iter().filter(|(height, _)| !held(&store, *height));
                let fresh: Vec<&Block> = fresh.map(|(_, block)| &**block).collect();
                let mut out: Vec<BlockChanges> =
                    fresh.iter().map(|block| store.changes(block.at())).collect();
                let folded = fold_run(&TreeStateReader::new(store.staged()), &fresh, &mut out);
                folded.unwrap_or_else(|error| panic!("{NAME} index: {error}"));
                for changes in out {
                    apply(&mut store, changes);
                }
                if run.finalized {
                    commit(&mut store);
                }
                store
            })
            .await;
            publisher.publish(&store);
        }
        store = blocking(move || {
            commit(&mut store);
            store
        })
        .await;
        publisher.publish(&store);
    }
}

/// - `Inconsistent` = parent's nodes rebuild no frontier of its recorded size (a fold bug)
/// - `Commitment` = non-canonical field element off the wire
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    #[error("tree-state store is inconsistent at size {size}")]
    Inconsistent { size: u64 },

    #[error("block {height} carries an uncommittable note commitment")]
    Commitment { height: Height },
}

/// `block` onto `parent` (= a run of one)
pub fn fold<V: SequenceRead>(
    parent: &TreeStateReader<V>,
    block: &Block,
    out: &mut BlockChanges,
) -> Result<(), FoldError> {
    fold_run(parent, &[block], slice::from_mut(out))
}

/// `blocks` (oldest first) onto `parent`, hashed as one batch, block `i` into `out[i]`: its height
/// record, the nodes whose last leaf it holds, the subtree roots it closes
///
/// - bulk sync's runs spread one level's hashing over every core, not one block's few leaves
/// - every leaf decoded and every frontier read before any hashing
pub(crate) fn fold_run<V: SequenceRead>(
    parent: &TreeStateReader<V>,
    blocks: &[&Block],
    out: &mut [BlockChanges],
) -> Result<(), FoldError> {
    BlockChanges::assert_run(parent.view().tip(), blocks, out);
    if blocks.is_empty() {
        return Ok(());
    }
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
    split(parent, sizes, blocks, &PerPool { sapling, orchard, ironwood }, out);
    Ok(())
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

/// `retained` per pool per block → each block's delta, every table appended at its end
fn split<V: SequenceRead>(
    parent: &TreeStateReader<V>,
    mut sizes: TreeSizes,
    blocks: &[&Block],
    retained: &PerPool<Vec<Retained>>,
    out: &mut [BlockChanges],
) {
    let ends_of = |pool| {
        let levels = (0..MERKLE_DEPTH).map(|level| parent.len(level_table(pool, level)));
        (levels.collect::<Vec<u64>>(), parent.len(subtree_table(pool)))
    };
    let mut ends = ShieldedPool::ALL.map(|pool| (pool, ends_of(pool)));
    for (at, (block, out)) in blocks.iter().zip(out).enumerate() {
        let header = block.header();
        sizes = sizes.advance(block).expect("pool size within u32 (consensus caps it)");
        let record = TreeStateHeight { hash: header.hash, time: header.time, sizes };
        out.sequence(HEIGHTS).append(&heights::encode(&record));
        for (pool, (levels, subtrees_end)) in &mut ends {
            let Retained { nodes, subtrees } = &retained.get(*pool)[at];
            for (level, end) in (0..MERKLE_DEPTH).zip(levels.iter_mut()) {
                let run = nodes.range((level, 0)..(level + 1, 0));
                let run = run.map(|(&(_, slot), node)| (slot, node));
                append_in_order(out, level_table(*pool, level), end, run);
            }
            let entries = subtrees.iter().map(|(&index, entry)| (index, subtrees::encode(entry)));
            append_in_order(out, subtree_table(*pool), subtrees_end, entries);
        }
    }
}

/// `records` appended to `table`, each at its own slot (slot != the table's `end` = a fold bug)
fn append_in_order<R: AsRef<[u8]>>(
    out: &mut BlockChanges,
    table: SequenceTable,
    end: &mut u64,
    records: impl Iterator<Item = (u64, R)>,
) {
    let mut appends = out.sequence(table);
    for (slot, record) in records {
        assert_eq!(slot, *end, "{}: slot {slot} appended at {end}", table.name);
        appends.append(record.as_ref());
        *end += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{num::NonZeroUsize, path::Path, sync::Arc};

    use incrementalmerkletree::frontier::CommitmentTree;
    use proptest::prelude::*;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine, Schema,
    };
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::{
        BlockHash, BlockRef, CommitmentTreeBytes, NoteCommitment, SubtreeRoot,
    };
    use zaino_sync::{IndexerDataSink, Step};
    use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree};
    use zcash_protocol::consensus::NetworkType;

    use crate::{nodes::retained_nodes, ServeError, FORMAT, TABLES};

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const SCHEMA: Schema = Schema::new(IndexKind::TreeState, FORMAT, NetworkType::Regtest, TABLES);

    /// `write_buffer` = MIN: every applied block committed by the store itself
    fn open(fs: &Arc<SimFs>, write_buffer: NonZeroUsize) -> DiskStore {
        DiskEngine::new(fs.clone()).open(Path::new("/ts"), &SCHEMA, write_buffer).expect("open")
    }

    /// Writer over `store`, its final stream and handle
    fn start(
        store: DiskStore,
    ) -> (IndexerDataSink<Block>, IndexHandle<DiskView>, tokio::task::JoinHandle<()>) {
        let writer = TreeStateIndexWriter::new(store);
        let handle = writer.handle();
        let mut sink = IndexerDataSink::new("final");
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE)));
        (sink, handle, running)
    }

    fn step(block: &Arc<Block>) -> Step<Block> {
        Step::Apply { height: block.header().height, data: Arc::clone(block) }
    }

    /// The chain's final tip: the writer commits after it
    fn finalized(block: &Arc<Block>) -> Step<Block> {
        Step::Finalized { height: block.header().height, data: Arc::clone(block) }
    }

    /// `handle`'s durable tip at `tip` (`None` = nothing)
    async fn reached(handle: &mut IndexHandle<DiskView>, tip: Option<u32>) {
        while handle.tip().map(|tip| u32::from(tip.height)) != tip {
            assert!(handle.changed().await, "writer alive");
        }
    }

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

            let mut store = open(&SimFs::new(), NonZeroUsize::MAX);
            let mut out = Vec::new();
            let (mut next_block, mut next_leaf) = (0usize, 0u64);
            for (run, length) in runs.iter().cycle().enumerate() {
                if next_block == blocks.len() {
                    break;
                }
                let these = &blocks[next_block..(next_block + length).min(blocks.len())];
                let parent = TreeStateReader::new(store.staged());
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
                let mut changes = store.changes(tip);
                let nodes: BTreeMap<_, _> = retained.iter().flat_map(|block| &block.nodes).collect();
                for (&(level, _), node) in nodes {
                    changes.sequence(level_table(ShieldedPool::Sapling, level)).append(node);
                }
                store.apply(changes);
                out.extend(retained);
                next_block += these.len();
            }
            let parent = TreeStateReader::new(store.staged());
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

    /// Genesis + five real blocks (0, 1, many commitments per pool, both parities) folded in
    /// every split into runs, each run on a parent holding the runs before it: every block's
    /// delta = folding it alone on its parent, table by table
    #[test]
    fn a_run_folds_to_the_same_changes_as_its_blocks_one_by_one() {
        let mut mock = MockChain::regtest();
        // (sapling, orchard, ironwood) leaves per block; nullifier = the leaf byte repeated
        #[rustfmt::skip]
        let leaves: [(&[u8], &[u8], &[u8]); 5] = [
            (&[1, 2, 3],         &[101],                &[]),
            (&[4],               &[102, 103],           &[201]),
            (&[],                &[],                   &[]),
            (&[5, 6],            &[104, 105, 106, 107], &[202, 203]),
            (&[7, 8, 9, 10, 11], &[],                   &[204]),
        ];
        for (sapling, orchard, ironwood) in leaves {
            mock.mine(|b| {
                b.tx(|t| {
                    let t = sapling.iter().fold(t, |t, &leaf| t.sapling_output(leaf.into()));
                    let t = orchard
                        .iter()
                        .fold(t, |t, &leaf| t.orchard_action([leaf; 32], leaf.into()));
                    ironwood.iter().fold(t, |t, &leaf| t.ironwood_action([leaf; 32], leaf.into()))
                })
            });
        }
        let chain = mock.blocks(mock.tip());
        // (tip, every table's appends) per block
        let tables = |changes: &BlockChanges| {
            let appends = SCHEMA
                .sequences()
                .iter()
                .map(|&table| changes.appends(table).map(<[u8]>::to_vec).collect::<Vec<_>>());
            (changes.tip(), appends.collect::<Vec<_>>())
        };
        let mut store = open(&SimFs::new(), NonZeroUsize::MAX);
        let one_by_one: Vec<_> = chain
            .iter()
            .map(|block| {
                let mut changes = store.changes(block.at());
                fold(&TreeStateReader::new(store.staged()), block, &mut changes).expect("folds");
                let folded = tables(&changes);
                store.apply(changes);
                folded
            })
            .collect();

        // bit `i` of `split` = a run ends after block `i`
        let fold_in_runs = |split: u32| {
            let mut store = open(&SimFs::new(), NonZeroUsize::MAX);
            let mut folded = Vec::new();
            let mut run: Vec<&Block> = Vec::new();
            for (at, block) in chain.iter().enumerate() {
                run.push(block);
                if split & (1 << at) == 0 && at + 1 < chain.len() {
                    continue;
                }
                let mut out: Vec<BlockChanges> =
                    run.iter().map(|b| store.changes(b.at())).collect();
                let parent = TreeStateReader::new(store.staged());
                fold_run(&parent, &std::mem::take(&mut run), &mut out).expect("folds");
                for changes in out {
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
        let mut chain = MockChain::regtest();
        chain.mine(|b| b.tx(|t| t.sapling_output(1)));
        let two = chain.mine(|b| b.tx(|t| t.sapling_output(2)));
        // lie: 2's cmu edited to 0xff.. (above both moduli; the builder's leaves are canonical)
        let mut txs = chain.block(two.hash).transactions().to_vec();
        txs[1].sapling.outputs[0].cmu = [0xff; 32].into();
        let uncommittable = Arc::new(Block::new(chain.block(two.hash).header().clone(), txs));
        let mut blocks = chain.blocks(two);
        blocks[2] = uncommittable;
        let store = open(&SimFs::new(), NonZeroUsize::MAX);
        let run: Vec<&Block> = blocks.iter().map(|block| &**block).collect();
        let mut out: Vec<BlockChanges> =
            run.iter().map(|block| store.changes(block.at())).collect();
        let refused = fold_run(&TreeStateReader::new(store.staged()), &run, &mut out);
        assert_eq!(refused.err(), Some(FoldError::Commitment { height: h(2) }));
    }

    type Trees = (CommitmentTreeBytes, CommitmentTreeBytes, CommitmentTreeBytes);

    /// Oracle: the three trees both clients parse, every commitment of `blocks` appended in order
    fn naive_trees(blocks: &[Arc<Block>]) -> Trees {
        let txs = || blocks.iter().flat_map(|block| block.transactions());
        let sapling = txs().flat_map(|tx| &tx.sapling.outputs).map(|output| output.cmu);
        let orchard = txs().flat_map(|tx| &tx.orchard.actions).map(|action| action.cmx);
        let ironwood = txs().flat_map(|tx| &tx.ironwood.actions).map(|action| action.cmx);
        // ironwood reuses orchard's node
        let sapling = naive_tree::<sapling_crypto::Node>(sapling);
        (
            sapling,
            naive_tree::<MerkleHashOrchard>(orchard),
            naive_tree::<MerkleHashOrchard>(ironwood),
        )
    }

    fn naive_tree<H: Hashable + HashSer + Clone>(
        commitments: impl Iterator<Item = NoteCommitment>,
    ) -> CommitmentTreeBytes {
        let mut frontier = Frontier::<H, 32>::empty();
        for commitment in commitments {
            let node = H::read(&<[u8; 32]>::from(commitment)[..]).expect("canonical leaf");
            assert!(frontier.append(node), "frontier full");
        }

        let mut bytes = Vec::new();
        write_commitment_tree(&CommitmentTree::from_frontier(&frontier), &mut bytes)
            .expect("encode");
        CommitmentTreeBytes::new(bytes)
    }

    /// Genesis + four bulk blocks, each its own commit, crashed after every operation: each state
    /// reopens to an acknowledged or attempted commit, the committed tip's trees equal the naive
    /// oracle's, and the next block folds on correctly
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_proven_prefix_that_keeps_folding() {
        let fs = SimFs::recording();
        let mut chain = MockChain::regtest();
        // (sapling, orchard, ironwood) leaves per height 1..: 0, 1, many commitments per pool on
        // both parities (forces carries several levels up); nullifier = the leaf byte repeated
        #[rustfmt::skip]
        let leaves: [(&[u8], &[u8], &[u8]); 5] = [
            (&[1, 2, 3],         &[101],                &[]),
            (&[4],               &[102, 103],           &[201]),
            (&[],                &[],                   &[]),
            (&[5, 6],            &[104, 105, 106, 107], &[202, 203]),
            (&[7, 8, 9, 10, 11], &[],                   &[204]),
        ];
        for (sapling, orchard, ironwood) in leaves {
            chain.mine(|b| {
                b.tx(|t| {
                    let t = sapling.iter().fold(t, |t, &leaf| t.sapling_output(leaf.into()));
                    let t = orchard.iter().fold(t, |t, &n| t.orchard_action([n; 32], n.into()));
                    ironwood.iter().fold(t, |t, &n| t.ironwood_action([n; 32], n.into()))
                })
            });
        }
        let blocks = chain.blocks(chain.tip());
        let seen = |through: u32| naive_trees(&blocks[..=through as usize]);
        {
            let (sink, mut handle, running) = start(open(&fs, NonZeroUsize::MIN));
            for (acked, block) in (1u64..).zip(&blocks[..5]) {
                sink.send(step(block)).await;
                reached(&mut handle, Some(u32::from(block.header().height))).await;
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("stops at Shutdown");
        }

        let trees = |view: DiskView, at: u32| {
            let trees = TreeStateReader::new(view).treestate(h(at)).expect("held");
            (trees.sapling, trees.orchard, trees.ironwood)
        };
        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs, NonZeroUsize::MIN);
            let count = store.committed().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5) as u32);
            assert!(acked.contains(&count), "{label}: recovered {count}");
            if let Some(tip) = count.checked_sub(1) {
                assert_eq!(trees(store.committed(), tip), seen(tip), "{label}: at {tip}");
            }

            let (sink, handle, running) = start(store);
            sink.send(step(&blocks[count as usize])).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let next = trees(handle.view(), count);
            assert_eq!(next, seen(count), "{label}: folding on after recovery");
        }
    }

    /// - `Send(n)`: next `n` blocks
    /// - `Reopen`: shutdown, reopen, resend from one below the tip (held: skipped)
    #[derive(Debug, Clone)]
    enum Move {
        Send(usize),
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random chains through random final streams (runs, restarts): once every move commits,
        /// every height serves exactly the naive frontier's three trees and nothing past the tip
        /// is served
        #[test]
        fn random_histories_serve_the_naive_trees_at_every_height(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    4 => (1usize..=3).prop_map(Move::Send),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
            write_buffer in proptest::prop_oneof![
                proptest::strategy::Just(NonZeroUsize::MIN),
                proptest::strategy::Just(NonZeroUsize::MAX),
            ],
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves, write_buffer));
        }
    }

    /// Genesis, then one block per `counts` entry; leaves unique per pool: sapling 1.., orchard
    /// 10_001.., ironwood 20_001.. (nullifier = the leaf, little-endian)
    async fn random_history(
        counts: Vec<(usize, usize, usize)>,
        moves: Vec<Move>,
        write_buffer: NonZeroUsize,
    ) {
        let mut next = (1u32, 10_001u32, 20_001u32);
        let take = |count: usize, from: &mut u32| {
            let taken = *from..*from + count as u32;
            *from += count as u32;
            taken
        };
        let nullifier = |leaf: u32| {
            let mut nullifier = [0u8; 32];
            nullifier[..4].copy_from_slice(&leaf.to_le_bytes());
            nullifier
        };
        let mut mock = MockChain::regtest();
        for &(sapling, orchard, ironwood) in &counts {
            let (sapling, orchard) = (take(sapling, &mut next.0), take(orchard, &mut next.1));
            let ironwood = take(ironwood, &mut next.2);
            mock.mine(|b| {
                b.tx(|t| {
                    let t = sapling.fold(t, |t, leaf| t.sapling_output(leaf));
                    let t = orchard.fold(t, |t, leaf| t.orchard_action(nullifier(leaf), leaf));
                    ironwood.fold(t, |t, leaf| t.ironwood_action(nullifier(leaf), leaf))
                })
            });
        }
        let chain = mock.blocks(mock.tip());
        let trees_through = |height: usize| naive_trees(&chain[..=height]);

        let fs = SimFs::new();
        let (mut sink, mut handle, mut running) = start(open(&fs, write_buffer));
        let mut sent = 0usize;
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Send(count) => {
                    let burst: Vec<_> = chain.iter().skip(sent).take(count).collect();
                    for (at, block) in burst.iter().enumerate() {
                        let last = at + 1 == burst.len();
                        sink.send(if last { finalized(block) } else { step(block) }).await;
                        sent += 1;
                    }
                }
                Move::Reopen => {
                    sink.shutdown();
                    running.await.expect("stops at Shutdown");
                    (sink, handle, running) = start(open(&fs, write_buffer));
                    if let Some(held) = sent.checked_sub(1) {
                        sink.send(step(&chain[held])).await;
                    }
                }
            }
            let tip = sent.checked_sub(1).map(|last| last as u32);
            reached(&mut handle, tip).await;

            let label = format!("move {at} {next:?}");
            let reader = TreeStateReader::new(handle.view());
            for height in 0..sent as u32 {
                let served = reader.treestate(h(height));
                let served = served.unwrap_or_else(|error| panic!("{label}: {error}"));
                let trees = (served.sapling, served.orchard, served.ironwood);
                assert_eq!(trees, trees_through(height as usize), "{label}: trees at {height}");
            }
            let past = h(sent as u32);
            let absent = Err(ServeError::NotFound { height: past });
            assert_eq!(reader.treestate(past), absent, "{label}: past the tip");
        }
        sink.shutdown();
        running.await.expect("stops at Shutdown");
    }

    /// Real 2^16-leaf subtrees: each root = the served tree's own level-16 root at its completing
    /// height, named by that block; resumable from any `start_index` (`start_index == count` =
    /// empty, pepper-sync's probe); a reopen appends the next boundary after the ones on disk
    #[tokio::test]
    async fn subtree_roots_resume_from_start_index() {
        const HALF: u32 = 1 << 15;
        let fs = SimFs::new();
        let mut chain = MockChain::regtest();
        // orchard leaves (first, count) per height 1..: subtree 0 completes at height 2, subtree 1
        // at 4, subtree 2 at 5 (sent after a reopen); nullifier = the leaf, little-endian
        let orchard = [
            (1, HALF),
            (1 + HALF, HALF),
            (1 + 2 * HALF, 1),
            (2 + 2 * HALF, 2 * HALF - 1),
            (1 + 4 * HALF, 2 * HALF),
        ];
        for (first, count) in orchard {
            chain.mine(|b| {
                b.tx(|t| {
                    (first..first + count).fold(t, |t, leaf| {
                        let mut nullifier = [0u8; 32];
                        nullifier[..4].copy_from_slice(&leaf.to_le_bytes());
                        t.orchard_action(nullifier, leaf)
                    })
                })
            });
        }
        let blocks = chain.blocks(chain.tip());

        let completing =
            |block: &Block| BlockRef { hash: block.header().hash, height: block.header().height };
        // level-16 root of the subtree holding the tip of the orchard tree served at `height`
        let tree_root = |view: &TreeStateReader<DiskView>, height: u32| {
            let tree = view.treestate(h(height)).expect("served").orchard;
            let frontier = read_commitment_tree::<MerkleHashOrchard, _, 32>(tree.as_bytes())
                .expect("parses")
                .to_frontier();
            let root = frontier.value().expect("non-empty").root(Some(Level::from(16)));
            let mut bytes = [0u8; 32];
            root.write(&mut bytes[..]).expect("32 bytes");
            TreeRoot::from(bytes)
        };

        let (sink, mut handle, running) = start(open(&fs, NonZeroUsize::MAX));
        for block in &blocks[..4] {
            sink.send(step(block)).await;
        }
        sink.send(finalized(&blocks[4])).await;
        reached(&mut handle, Some(4)).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let view = TreeStateReader::new(open(&fs, NonZeroUsize::MAX).committed());
        let roots = view.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");
        let expected = [(2, &blocks[2]), (4, &blocks[4])].map(|(at, block)| SubtreeRoot {
            root: tree_root(&view, at),
            completing: completing(block),
        });
        assert_eq!(roots, expected, "two boundaries, their completing blocks");

        // slot = subtree index → resume = a seek, bound = a prefix
        use ShieldedPool::{Orchard, Sapling};
        assert_eq!(view.subtree_roots(Orchard, 1, 0).expect("resume"), roots[1..]);
        assert_eq!(view.subtree_roots(Orchard, 0, 1).expect("bounded"), roots[..1]);
        assert_eq!(view.subtree_roots(Orchard, 2, 0).expect("probe"), Vec::new());
        assert_eq!(view.subtree_roots(Sapling, 0, 0).expect("untouched pool"), Vec::new());

        // reopen rebuilds the subtree cursor from the files; the next boundary follows it
        let (sink, handle, running) = start(open(&fs, NonZeroUsize::MAX));
        sink.send(step(&blocks[5])).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let view = TreeStateReader::new(handle.view());
        let resumed_roots = view.subtree_roots(Orchard, 0, 0).expect("roots");
        let third = SubtreeRoot { root: tree_root(&view, 5), completing: completing(&blocks[5]) };
        assert_eq!(resumed_roots, [roots, vec![third]].concat(), "earlier entries untouched");
    }

    /// Every pool served as its real tree's serialization, never an empty field (both clients map
    /// an absent field onto `CommitmentTree::empty()` silently)
    #[tokio::test]
    async fn ironwood_serves_a_real_tree_not_an_empty_field() {
        let fs = SimFs::new();
        let (sink, handle, running) = start(open(&fs, NonZeroUsize::MAX));
        // ironwood-only block: sapling and orchard empty at this height
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| {
            b.tx(|t| {
                t.ironwood_action([1; 32], 201)
                    .ironwood_action([2; 32], 202)
                    .ironwood_action([3; 32], 203)
            })
        });
        for block in chain.blocks(one) {
            sink.send(step(&block)).await;
        }
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let reader = TreeStateReader::new(handle.view());
        let trees = reader.treestate(h(1)).expect("served");

        let parsed = read_commitment_tree::<MerkleHashOrchard, _, 32>(trees.ironwood.as_bytes())
            .expect("the clients' own parser accepts it");
        assert_eq!(parsed.to_frontier().tree_size(), 3);

        // empty pool = the three-byte empty tree, not "" (active pool with no notes != a pool
        // below its activation)
        for empty in [trees.sapling, trees.orchard] {
            assert_eq!(empty.as_bytes(), [0u8, 0, 0]);
            let parsed = read_commitment_tree::<MerkleHashOrchard, _, 32>(empty.as_bytes());
            assert_eq!(parsed.expect("parses").to_frontier().tree_size(), 0);
        }
    }
}
