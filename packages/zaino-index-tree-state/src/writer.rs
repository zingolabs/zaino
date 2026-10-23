//! tree_state writer: the finalized stream → one [`fold_run`] per run → its store
//!
//! - one run = one `Hashable::combine_pairs` per level across every block of it
//!   (`Frontier::append_batch_visiting`, node types split wide levels across cores)
//! - pools fold concurrently
//! - generic over the node type (ironwood reuses `MerkleHashOrchard`)
//! - node / subtree root → the block holding its last leaf: any split into runs = same deltas
//! - nothing carried between runs: each reads its parent frontiers off `Store::applied`

use std::{
    collections::BTreeMap,
    slice,
    time::{Duration, Instant},
};

use incrementalmerkletree::{frontier::Frontier, Address, Hashable, Level};
use orchard::tree::MerkleHashOrchard;
use zaino_persistence::{BlockChanges, IndexKind, SequenceRead, SequenceTable, Store};
use zaino_primitives::types::{Block, PerPool, TreeRoot, TreeSizes};
use zaino_sync::{
    apply, assert_run, blocking, commit, held, ran, IndexHandle, IndexPublisher, Subscription,
};
use zcash_primitives::merkle_tree::HashSer;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::ShieldedPool;

use crate::{
    heights::{self, TreeStateHeight},
    nodes::{self, slot, Slot, MERKLE_DEPTH, NODE},
    subtrees::{self, SubtreeEntry},
    tables::{level_table, subtree_table, HEIGHTS},
    TreeStateReader,
};

const NAME: &str = IndexKind::TreeState.name();

/// Level whose completion = one `GetSubtreeRoots` entry (2^16 leaves, the protocol's shard)
const SUBTREE_LEVEL: u8 = 16;

pub struct TreeStateIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: SequenceRead>> TreeStateIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)) at its durable tip
    pub fn new(store: S) -> Self {
        zaino_sync::assert_dense(&store.durable(), HEIGHTS);
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    /// For `Nfs::add`: durable view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    ///
    /// - one `fold_run` per run: every block the store lacks, hashed as one batch
    /// - commits after the finalized tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            let started = Instant::now();
            let write;
            (store, write) = blocking(move || {
                let mut write = Duration::ZERO;
                let fresh = run.blocks.iter().filter(|block| !held(&store, block));
                let fresh: Vec<&Block> = fresh.map(|block| &**block).collect();
                let mut out: Vec<BlockChanges> =
                    fresh.iter().map(|block| store.changes(block.block_ref())).collect();
                let folded = fold_run(&TreeStateReader::new(store.applied()), &fresh, &mut out);
                folded.unwrap_or_else(|error| panic!("{NAME} index: {error}"));
                for changes in out {
                    write += apply(&mut store, changes);
                }
                if run.ends_at_finalized_tip {
                    write += commit(&mut store);
                }
                (store, write)
            })
            .await;
            publisher.publish(&store);
            ran(&store, started, write);
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
    Commitment { height: BlockHeight },
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
    assert_run(parent.view().tip(), blocks, out);
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
    ends: Vec<(BlockHeight, usize)>,
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
            let txs = block.transactions().iter();
            for commitment in txs.flat_map(|tx| tx.note_commitments(pool)) {
                let leaf = nodes::decode(&commitment.0);
                leaves.push(leaf.ok_or(FoldError::Commitment { height })?);
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
                    let (root, end_height) = (TreeRoot(nodes::encode(node)), ends[block].0);
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
        sizes = sizes.advance(block);
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

    use std::{num::NonZeroUsize, sync::Arc};

    use incrementalmerkletree::frontier::CommitmentTree;
    use proptest::prelude::*;
    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema, View};
    use zaino_primitives::types::{BlockRef, CommitmentTreeBytes, NoteCommitment, SubtreeRoot};
    use zaino_source::testing::{h, MockChain};
    use zaino_sync::testing::{
        block_on_paused, finalized, moves, open, step, write_buffers, FinalizedStreamFixture, Move,
        Writer, QUEUE,
    };
    use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree};
    use zcash_protocol::consensus::NetworkType;
    use zcash_protocol::BlockHash;

    use crate::ServeError;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    fn start(store: DiskStore) -> Writer<DiskView> {
        let writer = TreeStateIndexWriter::new(store);
        Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
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

    fn height(n: usize) -> BlockHeight {
        BlockHeight::from(u32::try_from(n).expect("test heights fit u32"))
    }

    /// Oracle: incrementalmerkletree's frontier, leaf by leaf
    fn library_frontier(size: u64) -> Frontier<Mix, MERKLE_DEPTH> {
        let mut frontier = Frontier::empty();
        assert!((0..size).all(|n| frontier.append(leaf(n))), "{size} leaves fit a depth-32 tree");
        frontier
    }

    /// Oracle: the whole tree built level by level, each retained node + subtree root under the
    /// block holding its last leaf (block `b` = `leaves_per_block[b]` leaves, height `b`)
    fn naive_retained(leaves_per_block: &[usize], subtree_level: u8) -> Vec<Retained> {
        let block_of_leaf: Vec<usize> = (0..leaves_per_block.len())
            .flat_map(|block| std::iter::repeat_n(block, leaves_per_block[block]))
            .collect();
        let total = block_of_leaf.len() as u64;
        let mut retained = vec![Retained::default(); leaves_per_block.len()];
        let mut level_nodes: Vec<Mix> = (0..total).map(leaf).collect();
        for level in 0..MERKLE_DEPTH {
            for (index, node) in (0u64..).zip(&level_nodes) {
                let last_leaf = usize::try_from(((index + 1) << level) - 1).expect("leaf index");
                let block = block_of_leaf[last_leaf];
                let kept = match level {
                    0 => Some(index),
                    _ => (index % 2 == 0).then_some(index / 2),
                };
                if let Some(slot) = kept {
                    retained[block].nodes.insert((level, slot), nodes::encode(node));
                }
                if level == subtree_level {
                    let entry = SubtreeEntry {
                        root: TreeRoot(nodes::encode(node)),
                        end_height: height(block),
                    };
                    retained[block].subtrees.insert(index, entry);
                }
            }
            level_nodes = level_nodes
                .chunks_exact(2)
                .map(|pair| Mix::combine(Level::from(level), &pair[0], &pair[1]))
                .collect();
        }
        retained
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// Random blocks split into random runs, each run hashed onto the frontier read back off
        /// the nodes the runs before it retained
        ///
        /// - every frontier read = the library's leaf-by-leaf one at that size
        /// - retained nodes + subtree roots = the naive tree's, each under the block holding its
        ///   last leaf (any split = the same per-block deltas)
        #[test]
        fn any_split_into_runs_retains_the_naive_trees_nodes_reading_each_frontier_back(
            leaves_per_block in proptest::collection::vec(0usize..40, 1..60),
            run_lengths in proptest::collection::vec(1usize..12, 1..60),
            subtree_level in 1u8..5,
        ) {
            let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
            let mut folded: Vec<Retained> = Vec::new();
            let mut leaves_before = 0u64;
            for (run, length) in run_lengths.iter().cycle().enumerate() {
                let first = folded.len();
                if first == leaves_per_block.len() {
                    break;
                }
                let these = &leaves_per_block[first..(first + length).min(leaves_per_block.len())];
                let frontier = TreeStateReader::new(store.applied())
                    .frontier::<Mix>(ShieldedPool::Sapling, leaves_before);
                let library = library_frontier(leaves_before);
                prop_assert_eq!(&frontier, &Some(library), "frontier read before run {}", run);
                let mut leaves = Vec::new();
                let mut ends = Vec::new();
                for (offset, &count) in these.iter().enumerate() {
                    leaves.extend((leaves_before..).take(count).map(leaf));
                    leaves_before += count as u64;
                    ends.push((height(first + offset), leaves.len()));
                }
                let frontier = frontier.expect("compared above");

                let retained = PoolRun { frontier, leaves, ends }.hash(Level::from(subtree_level));

                // run's nodes stored as one change set (tip height = run number)
                let mut changes =
                    store.changes(BlockRef { hash: BlockHash([0; 32]), height: height(run) });
                let nodes: BTreeMap<_, _> = retained.iter().flat_map(|block| &block.nodes).collect();
                for (&(level, _), node) in nodes {
                    changes.sequence(level_table(ShieldedPool::Sapling, level)).append(node);
                }
                store.apply(changes);
                folded.extend(retained);
            }
            let last = TreeStateReader::new(store.applied())
                .frontier::<Mix>(ShieldedPool::Sapling, leaves_before);
            prop_assert_eq!(last, Some(library_frontier(leaves_before)), "frontier after every run");

            let naive = naive_retained(&leaves_per_block, subtree_level);
            for (block, (folded, naive)) in folded.iter().zip(&naive).enumerate() {
                let slots = |retained: &Retained| retained.nodes.keys().copied().collect::<Vec<_>>();
                prop_assert_eq!(slots(folded), slots(naive), "block {}: retained slots", block);
                prop_assert!(folded.nodes == naive.nodes, "block {block}: retained node bytes");
                prop_assert_eq!(&folded.subtrees, &naive.subtrees, "block {}: subtree roots", block);
            }
        }
    }

    /// Genesis + five real blocks (0, 1, many commitments per pool, both parities) folded in
    /// every split into runs, each run on a parent holding the runs before it: every block's
    /// delta = folding it alone on its parent, table by table
    #[test]
    fn a_run_folds_to_the_same_changes_as_its_blocks_one_by_one() {
        let mut mock = MockChain::regtest();
        // nullifier tag = the leaf
        #[rustfmt::skip]
        let leaves: [(&[u8], &[u8], &[u8]); 5] = [
            // (sapling,         orchard,               ironwood)
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
                    let t = orchard.iter().fold(t, |t, &n| t.orchard_action(n.into(), n.into()));
                    ironwood.iter().fold(t, |t, &n| t.ironwood_action(n.into(), n.into()))
                })
            });
        }
        let chain = mock.blocks(mock.tip());
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let one_by_one: Vec<BlockChanges> = chain
            .iter()
            .map(|block| {
                let mut changes = store.changes(block.block_ref());
                fold(&TreeStateReader::new(store.applied()), block, &mut changes).expect("folds");
                store.apply(changes.clone());
                changes
            })
            .collect();

        // bit `i` of `split` = a run ends after block `i`
        for split in 0..1u32 << (chain.len() - 1) {
            let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
            let mut in_runs: Vec<BlockChanges> = Vec::new();
            let mut run: Vec<&Block> = Vec::new();
            for (at, block) in chain.iter().enumerate() {
                run.push(block);
                if split & (1 << at) == 0 && at + 1 < chain.len() {
                    continue;
                }
                let mut out: Vec<BlockChanges> =
                    run.iter().map(|b| store.changes(b.block_ref())).collect();
                let parent = TreeStateReader::new(store.applied());
                fold_run(&parent, &std::mem::take(&mut run), &mut out).expect("folds");
                for changes in out {
                    store.apply(changes.clone());
                    in_runs.push(changes);
                }
            }

            for (alone, ran) in one_by_one.iter().zip(&in_runs) {
                let case = format!("runs split by {split:#06b}, block {:?}", alone.block());
                assert_eq!(ran.block(), alone.block(), "{case}: delta for its own block");
                for &table in SCHEMA.sequences() {
                    let ran: Vec<&[u8]> = ran.appends(table).collect();
                    let alone: Vec<&[u8]> = alone.appends(table).collect();
                    let counts = format!("{} appends vs {} alone", ran.len(), alone.len());
                    assert!(ran == alone, "{case}: table {} differs ({counts})", table.name);
                }
            }
        }
    }

    /// Non-canonical commitment anywhere in a run → the run refused, naming its block
    #[test]
    fn an_uncommittable_note_commitment_refuses_the_run_naming_its_block() {
        let mut chain = MockChain::regtest();
        chain.mine(|b| b.tx(|t| t.sapling_output(1)));
        let two = chain.mine(|b| b.tx(|t| t.sapling_output(2)));
        // lie: 2's cmu edited to 0xff.. (above both moduli, builder's leaves = canonical)
        let mut txs = chain.block(two.hash).transactions().to_vec();
        txs[1].sapling.outputs[0].cmu = NoteCommitment([0xff; 32]);
        let uncommittable = Arc::new(Block::new(chain.block(two.hash).header().clone(), txs));
        let mut blocks = chain.blocks(two);
        blocks[2] = uncommittable;
        let store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let run: Vec<&Block> = blocks.iter().map(|block| &**block).collect();
        let mut out: Vec<BlockChanges> =
            run.iter().map(|block| store.changes(block.block_ref())).collect();

        let refused = fold_run(&TreeStateReader::new(store.applied()), &run, &mut out);

        let expected = Err(FoldError::Commitment { height: h(2) });
        assert_eq!(refused, expected, "run 0..=2, cmu edited at 2");
    }

    /// Oracle: the three trees both clients parse, every commitment of `blocks` appended in order
    fn naive_trees(blocks: &[Arc<Block>]) -> PerPool<CommitmentTreeBytes> {
        let txs = || blocks.iter().flat_map(|block| block.transactions());
        let sapling = txs().flat_map(|tx| &tx.sapling.outputs).map(|output| output.cmu);
        let orchard = txs().flat_map(|tx| &tx.orchard.actions).map(|action| action.cmx);
        let ironwood = txs().flat_map(|tx| &tx.ironwood.actions).map(|action| action.cmx);
        // ironwood reuses orchard's node
        PerPool {
            sapling: naive_tree::<sapling_crypto::Node>(sapling),
            orchard: naive_tree::<MerkleHashOrchard>(orchard),
            ironwood: naive_tree::<MerkleHashOrchard>(ironwood),
        }
    }

    fn naive_tree<H: Hashable + HashSer + Clone>(
        commitments: impl Iterator<Item = NoteCommitment>,
    ) -> CommitmentTreeBytes {
        let mut frontier = Frontier::<H, 32>::empty();
        for commitment in commitments {
            let node = H::read(&commitment.0[..]).expect("canonical leaf");
            assert!(frontier.append(node), "frontier full");
        }

        let mut bytes = Vec::new();
        write_commitment_tree(&CommitmentTree::from_frontier(&frontier), &mut bytes)
            .expect("encode");
        CommitmentTreeBytes::new(bytes)
    }

    /// Genesis + four bulk blocks, each its own commit, crashed after every operation: each state
    /// reopens to an acknowledged or attempted commit, the durable tip's trees equal the naive
    /// oracle's, and the next block folds on correctly
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_proven_prefix_that_keeps_folding() {
        let (fs, recording) = SimFs::recording();
        let mut chain = MockChain::regtest();
        // - 0, 1, many commitments per pool on both parities (forces carries several levels up)
        // - nullifier tag = the leaf
        #[rustfmt::skip]
        let leaves: [(&[u8], &[u8], &[u8]); 5] = [
            // (sapling,         orchard,               ironwood)
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
                    let t = orchard.iter().fold(t, |t, &n| t.orchard_action(n.into(), n.into()));
                    ironwood.iter().fold(t, |t, &n| t.ironwood_action(n.into(), n.into()))
                })
            });
        }
        let blocks = chain.blocks(chain.tip());
        let naive_through = |through: u32| naive_trees(&blocks[..=through as usize]);
        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MIN));
        for (acked, block) in (1u64..).zip(&blocks[..5]) {
            writer.send(step(block)).await;
            writer.durable_at(block.header().height).await;
            fs.set_tag(acked);
        }
        writer.stop().await.expect("stops at Shutdown");

        let trees = |view: DiskView, at: u32| {
            TreeStateReader::new(view).treestate(h(at)).expect("held").trees
        };
        let states = recording.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MIN);
            let recovered = store.durable().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let acknowledged = (state.tag as u32).min(5);
            let attempted = (acknowledged + 1).min(5);
            let case =
                format!("{}: recovered {recovered} blocks, next height {recovered}", state.label);
            assert!(
                [acknowledged, attempted].contains(&recovered),
                "{case}: acknowledged {acknowledged}, attempted {attempted}"
            );
            if let Some(tip) = recovered.checked_sub(1) {
                let served = trees(store.durable(), tip);
                assert_eq!(served, naive_through(tip), "{case}: trees at the recovered tip {tip}");
            }

            let writer = start(store);
            writer.send(step(&blocks[recovered as usize])).await;
            let handle = writer.stop().await.unwrap_or_else(|error| panic!("{case}: {error}"));
            let next = trees(handle.durable_view(), recovered);
            assert_eq!(next, naive_through(recovered), "{case}: trees after folding on");
        }
    }

    /// Commitments one block adds, per pool (0..=3 each)
    fn commits() -> impl Strategy<Value = PerPool<u32>> {
        (0u32..=3, 0u32..=3, 0u32..=3).prop_map(|(sapling, orchard, ironwood)| PerPool {
            sapling,
            orchard,
            ironwood,
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

        /// Random chains through random finalized streams (runs, restarts): once every move
        /// commits, every height serves exactly the naive frontier's three trees and nothing past
        /// the tip is served
        #[test]
        fn random_histories_serve_the_naive_trees_at_every_height(
            per_block in proptest::collection::vec(commits(), 1..10),
            moves in moves(),
            write_buffer in write_buffers(),
        ) {
            block_on_paused(random_history(per_block, moves, write_buffer))?;
        }
    }

    /// Genesis, then one block per `per_block` entry
    ///
    /// - leaves unique per pool: sapling 1.., orchard 10_001.., ironwood 20_001..
    /// - nullifier tag = the leaf
    async fn random_history(
        per_block: Vec<PerPool<u32>>,
        moves: Vec<Move>,
        write_buffer: NonZeroUsize,
    ) -> Result<(), TestCaseError> {
        let mut next_leaf = PerPool { sapling: 1u32, orchard: 10_001, ironwood: 20_001 };
        let mut mock = MockChain::regtest();
        for adds in &per_block {
            let mut take = |pool: ShieldedPool| {
                let from = next_leaf.get_mut(pool);
                let taken = *from..*from + adds.get(pool);
                *from = taken.end;
                taken
            };
            let (sapling, orchard) = (take(ShieldedPool::Sapling), take(ShieldedPool::Orchard));
            let ironwood = take(ShieldedPool::Ironwood);
            mock.mine(|b| {
                b.tx(|t| {
                    let t = sapling.fold(t, |t, leaf| t.sapling_output(leaf));
                    let t = orchard.fold(t, |t, leaf| t.orchard_action(leaf, leaf));
                    ironwood.fold(t, |t, leaf| t.ironwood_action(leaf, leaf))
                })
            });
        }
        let chain = mock.blocks(mock.tip());
        let trees_through = |height: usize| naive_trees(&chain[..=height]);

        let fs = SimFs::new();
        let mut stream = FinalizedStreamFixture::new(&chain);
        let mut writer = start(open(&fs, &SCHEMA, write_buffer));
        for (at, next) in moves.into_iter().enumerate() {
            if next == Move::Reopen {
                writer.stop().await.expect("stops at Shutdown");
                writer = start(open(&fs, &SCHEMA, write_buffer));
            }
            for step in stream.steps(next) {
                writer.send(step).await;
            }
            writer.durable_at(stream.tip()).await;

            let case = format!("move {at} {next:?}");
            let reader = TreeStateReader::new(writer.handle().durable_view());
            let sent = stream.sent() as u32;
            for height in 0..sent {
                let served = reader.treestate(h(height)).map(|served| served.trees);
                let expected = Ok(trees_through(height as usize));
                prop_assert_eq!(served, expected, "{}: trees at {}", case, height);
            }
            let absent = Err(ServeError::NotFound { height: h(sent) });
            prop_assert_eq!(reader.treestate(h(sent)), absent, "{}: past the tip {}", case, sent);
        }
        writer.stop().await.expect("stops at Shutdown");
        Ok(())
    }

    /// Orchard leaves `(first, count)` per height 1..: subtree 0 completes at 2, subtree 1 at 4,
    /// subtree 2 at 5
    const HALF: u32 = 1 << 15;
    const ORCHARD_LEAVES: [(u32, u32); 5] = [
        (1, HALF),
        (1 + HALF, HALF),
        (1 + 2 * HALF, 1),
        (2 + 2 * HALF, 2 * HALF - 1),
        (1 + 4 * HALF, 2 * HALF),
    ];
    /// <= 8 192 actions per tx (a tx's proof length = a CompactSize, <= 32 MiB)
    const PER_TX: u32 = 1 << 13;

    /// Level-16 root of the subtree holding the tip of the orchard tree served at `height`
    /// (the client's own reading of the served tree)
    fn served_subtree_root(view: &TreeStateReader<DiskView>, height: u32) -> TreeRoot {
        let tree = view.treestate(h(height)).expect("served").trees.orchard;
        let frontier = read_commitment_tree::<MerkleHashOrchard, _, 32>(tree.as_bytes())
            .expect("parses")
            .to_frontier();
        let root = frontier.value().expect("non-empty").root(Some(Level::from(16)));
        let mut bytes = [0u8; 32];
        root.write(&mut bytes[..]).expect("32 bytes");
        TreeRoot(bytes)
    }

    /// Real 2^16-leaf subtrees: each root = the served tree's own level-16 root at its completing
    /// height, named by that block, resumable from any `start_index`
    #[tokio::test]
    async fn subtree_roots_name_their_completing_blocks_and_resume_from_any_start_index() {
        let mut chain = MockChain::regtest();
        for (first, count) in ORCHARD_LEAVES[..4].iter().copied() {
            chain.mine(|b| {
                (first..first + count).step_by(PER_TX as usize).fold(b, |b, from| {
                    let to = (from + PER_TX).min(first + count);
                    b.tx(|t| (from..to).fold(t, |t, leaf| t.orchard_action(leaf, leaf)))
                })
            });
        }
        let blocks = chain.blocks(chain.tip());
        let mut writer = start(open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX));
        for block in &blocks[..4] {
            writer.send(step(block)).await;
        }
        writer.send(finalized(&blocks[4])).await;
        writer.durable_at(h(4)).await;
        let view = TreeStateReader::new(writer.stop().await.expect("stops").durable_view());

        let roots = view.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");

        let expected = [2, 4].map(|at| SubtreeRoot {
            root: served_subtree_root(&view, at),
            completing: blocks[at as usize].block_ref(),
        });
        assert_eq!(roots, expected, "two boundaries, their completing blocks");
        // (pool, start_index, max_entries) → served = a slice of `roots`
        use ShieldedPool::{Orchard, Sapling};
        #[rustfmt::skip]
        let cases = [
            ("resume = a seek",                   Orchard, 1, 0, &roots[1..]),
            ("bound = a prefix",                  Orchard, 0, 1, &roots[..1]),
            ("start = count = empty (the probe)", Orchard, 2, 0, &[][..]),
            ("untouched pool",                    Sapling, 0, 0, &[][..]),
        ];
        for (case, pool, start_index, max_entries, expected) in cases {
            let served = view.subtree_roots(pool, start_index, max_entries);
            assert_eq!(served.as_deref(), Ok(expected), "{case}: {pool:?} from {start_index}");
        }
    }

    /// Reopen rebuilds the subtree cursor from the files → the next boundary follows those on disk
    #[tokio::test]
    async fn a_reopened_writer_appends_the_next_subtree_root_after_those_on_disk() {
        let fs = SimFs::new();
        let mut chain = MockChain::regtest();
        for (first, count) in ORCHARD_LEAVES {
            chain.mine(|b| {
                (first..first + count).step_by(PER_TX as usize).fold(b, |b, from| {
                    let to = (from + PER_TX).min(first + count);
                    b.tx(|t| (from..to).fold(t, |t, leaf| t.orchard_action(leaf, leaf)))
                })
            });
        }
        let blocks = chain.blocks(chain.tip());
        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));
        for block in &blocks[..4] {
            writer.send(step(block)).await;
        }
        writer.send(finalized(&blocks[4])).await;
        writer.durable_at(h(4)).await;
        writer.stop().await.expect("stops at Shutdown");
        let before = TreeStateReader::new(open(&fs, &SCHEMA, NonZeroUsize::MAX).durable());
        let before = before.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");

        let writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));
        writer.send(step(&blocks[5])).await;
        let view = TreeStateReader::new(writer.stop().await.expect("stops").durable_view());

        let after = view.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");
        let third =
            SubtreeRoot { root: served_subtree_root(&view, 5), completing: blocks[5].block_ref() };
        assert_eq!(before.len(), 2, "subtrees 0 and 1 on disk before the reopen");
        assert_eq!(after, [before, vec![third]].concat(), "earlier entries untouched, 2 appended");
    }

    /// Every pool served as its real tree's serialization, never an empty field (both clients map
    /// an absent field onto `CommitmentTree::empty()` silently)
    #[tokio::test]
    async fn ironwood_serves_a_real_tree_not_an_empty_field() {
        let writer = start(open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX));
        // ironwood-only block: sapling and orchard empty at this height
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| {
            b.tx(|t| t.ironwood_action(1, 201).ironwood_action(2, 202).ironwood_action(3, 203))
        });
        for block in chain.blocks(one) {
            writer.send(step(&block)).await;
        }
        let handle = writer.stop().await.expect("stops at Shutdown");
        let reader = TreeStateReader::new(handle.durable_view());

        let trees = reader.treestate(h(1)).expect("served").trees;

        let parsed = read_commitment_tree::<MerkleHashOrchard, _, 32>(trees.ironwood.as_bytes())
            .expect("the clients' own parser accepts it");
        assert_eq!(parsed.to_frontier().tree_size(), 3, "ironwood: its 3 leaves");
        // empty pool = the three-byte empty tree, not "" (active pool with no notes != a pool
        // below its activation)
        for (pool, empty) in [("sapling", trees.sapling), ("orchard", trees.orchard)] {
            assert_eq!(empty.as_bytes(), [0u8, 0, 0], "{pool}: the empty tree's bytes");
            let parsed = read_commitment_tree::<MerkleHashOrchard, _, 32>(empty.as_bytes());
            assert_eq!(parsed.expect("parses").to_frontier().tree_size(), 0, "{pool}: no leaves");
        }
    }
}
