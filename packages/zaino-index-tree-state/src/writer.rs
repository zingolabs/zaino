//! tree_state writer: the final stream → one [`fold_run`] per run of unfolded steps → its store
//!
//! - one run = one batched Merkle hashing on the CPU pool (every core); nothing carried between
//!   runs: each reads its parent frontiers off `Store::staged`

use std::num::NonZeroUsize;

use tokio::sync::watch;
use zaino_persistence::{IndexKind, SequenceRead, Store, View};
use zaino_primitives::types::Block;
use zaino_sync::{held, Committer, Final, Subscription};

use crate::{fold::fold_run, TreeStateReader, HEIGHTS};

const NAME: &str = IndexKind::TreeState.name();

pub struct TreeStateIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: SequenceRead>> TreeStateIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)) at its committed tip; `batch_bytes` =
    /// buffered bytes per bulk commit (one fsync), and one run's stream bytes
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let view = store.view();
        let held = view.tip().map_or(0, |tip| u64::from(tip.height) + 1);
        assert_eq!(view.len(HEIGHTS), held, "{NAME}: one record per committed height");
        Self { store: Committer::new(store, batch_bytes) }
    }

    /// For `Nfs::subscribe`: the committed view after every commit
    pub fn committed(&self) -> watch::Receiver<S::View> {
        self.store.committed()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    pub async fn run(mut self, mut blocks: Subscription<Final>) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let applied = move |store: &mut S| {
                let unfolded = run.unfolded.iter().filter(|(height, _)| !held(store, *height));
                let fresh: Vec<&Block> = unfolded.map(|(_, block)| &**block).collect();
                if !fresh.is_empty() {
                    let parent = TreeStateReader::new(store.staged(), store.schema().network);
                    let folded = fold_run(&parent, &fresh);
                    for changes in folded.unwrap_or_else(|error| panic!("{NAME} index: {error}")) {
                        store.apply(changes);
                    }
                }
                run.apply_folded(store);
            };
            self.store.compute(applied).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{path::Path, sync::Arc};

    use incrementalmerkletree::{
        frontier::{CommitmentTree, Frontier},
        Hashable, Level,
    };
    use orchard::tree::MerkleHashOrchard;
    use proptest::strategy::Strategy as _;
    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine};
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        BlockRef, CommitmentTreeBytes, CompactCiphertext, Height, OrchardAction, OrchardData,
        SaplingData, SaplingOutput, ShieldedPool, SubtreeRoot, Transaction, TransactionId,
        TreeRoot,
    };
    use zaino_sync::{Folds, IndexerDataSink, Step};
    use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree, HashSer};
    use zcash_protocol::consensus::NetworkType;

    use crate::{fold, schema, ServeError};

    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const NETWORK: NetworkType = NetworkType::Regtest;

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        DiskEngine::new(fs.clone()).open(Path::new("/ts"), &schema(NETWORK)).expect("open")
    }

    /// The writer over `store`, its final stream and committed view
    fn start(
        store: DiskStore,
        batch: NonZeroUsize,
    ) -> (IndexerDataSink<Final>, watch::Receiver<DiskView>, tokio::task::JoinHandle<()>) {
        let writer = TreeStateIndexWriter::new(store, batch);
        let committed = writer.committed();
        let mut sink = IndexerDataSink::new("final");
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE)));
        (sink, committed, running)
    }

    /// Each block's own fold from genesis, as the NFS folds it (the folded steps' payload)
    fn folded(chain: &[Arc<Block>]) -> Vec<Arc<Folds>> {
        let mut scratch = open(&SimFs::new());
        chain
            .iter()
            .map(|block| {
                let changes = fold(&TreeStateReader::new(scratch.staged(), NETWORK), block);
                let changes = changes.expect("canonical commitments");
                let mut folds = Folds::default();
                folds.insert(IndexKind::TreeState, changes.clone());
                scratch.apply(changes);
                Arc::new(folds)
            })
            .collect()
    }

    fn step(block: &Arc<Block>, folds: Option<&Arc<Folds>>) -> Step<Final> {
        let (height, folds) = (block.header().height, folds.map(Arc::clone));
        Step::Apply { height, data: Arc::new(Final { block: Arc::clone(block), folds }) }
    }

    /// `committed` at `tip` (`None` = nothing)
    async fn reached(committed: &mut watch::Receiver<DiskView>, tip: Option<u32>) {
        let at = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height)) == tip;
        committed.wait_for(at).await.expect("writer alive");
    }

    /// Canonical field element for every pool (small LE value < both moduli, unlike a
    /// repeated-byte filler)
    fn leaf(seed: u32) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&seed.to_le_bytes());
        bytes
    }

    /// One transaction carrying each pool's commitment list, txid = `leaf(seed)`
    fn pools(seed: u32, sapling: &[u32], orchard: &[u32], ironwood: &[u32]) -> Transaction {
        let sapling_out = |seed: &u32| SaplingOutput {
            cmu: leaf(*seed).into(),
            ephemeral_key: [2u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
        };
        let action = |seed: &u32| OrchardAction {
            nullifier: [4u8; 32].into(),
            cmx: leaf(*seed).into(),
            ephemeral_key: [6u8; 32].into(),
            enc_ciphertext: CompactCiphertext::from([7u8; CompactCiphertext::LENGTH]),
        };

        Transaction {
            txid: TransactionId::from(leaf(seed)),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: SaplingData {
                outputs: sapling.iter().map(sapling_out).collect(),
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
        }
    }

    /// Oracle: the tree both clients parse, every commitment appended from genesis
    fn naive_tree<H: Hashable + HashSer + Clone>(leaves: &[u32]) -> CommitmentTreeBytes {
        let mut frontier = Frontier::<H, 32>::empty();
        for seed in leaves {
            let node = H::read(&leaf(*seed)[..]).expect("canonical leaf");
            assert!(frontier.append(node), "frontier full");
        }

        let mut bytes = Vec::new();
        write_commitment_tree(&CommitmentTree::from_frontier(&frontier), &mut bytes)
            .expect("encode");
        CommitmentTreeBytes::new(bytes)
    }

    fn sapling_tree(leaves: &[u32]) -> CommitmentTreeBytes {
        naive_tree::<sapling_crypto::Node>(leaves)
    }

    /// Orchard and ironwood (ironwood reuses orchard's node)
    fn orchard_tree(leaves: &[u32]) -> CommitmentTreeBytes {
        naive_tree::<MerkleHashOrchard>(leaves)
    }

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// `(sapling, orchard, ironwood)` per height: 0, 1, many commitments per pool on both
    /// parities (forces carries several levels up)
    const CHAIN: [(&[u32], &[u32], &[u32]); 5] = [
        (&[1, 2, 3], &[101], &[]),
        (&[4], &[102, 103], &[201]),
        (&[], &[], &[]),
        (&[5, 6], &[104, 105, 106, 107], &[202, 203]),
        (&[7, 8, 9, 10, 11], &[], &[204]),
    ];

    /// The oracle's three trees after `CHAIN[..through]`
    fn seen(through: usize) -> (CommitmentTreeBytes, CommitmentTreeBytes, CommitmentTreeBytes) {
        let mut pools = (Vec::new(), Vec::new(), Vec::new());
        for (sapling, orchard, ironwood) in &CHAIN[..through] {
            pools.0.extend_from_slice(sapling);
            pools.1.extend_from_slice(orchard);
            pools.2.extend_from_slice(ironwood);
        }
        (sapling_tree(&pools.0), orchard_tree(&pools.1), orchard_tree(&pools.2))
    }

    /// Four bulk blocks, each its own commit, crashed after every operation: each state reopens
    /// to an acknowledged or attempted commit, the committed tip's trees equal the naive oracle's,
    /// and the next block folds on correctly
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_proven_prefix_that_keeps_folding() {
        let fs = SimFs::recording();
        let chain =
            linked((0u32..).zip(&CHAIN).map(|(height, (s, o, i))| vec![pools(height, s, o, i)]));
        {
            let (sink, mut committed, running) = start(open(&fs), NonZeroUsize::MIN);
            for (acked, block) in (1u64..).zip(&chain[..4]) {
                sink.send(step(block, None)).await;
                reached(&mut committed, Some(u32::from(block.header().height))).await;
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("stops at Shutdown");
        }

        let trees = |view: DiskView, at: u32| {
            let trees = TreeStateReader::new(view, NETWORK).treestate(h(at)).expect("held");
            (trees.sapling, trees.orchard, trees.ironwood)
        };
        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs);
            let count = store.view().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(4) as u32);
            assert!(acked.contains(&count), "{label}: recovered {count}");
            if let Some(tip) = count.checked_sub(1) {
                assert_eq!(trees(store.view(), tip), seen(count as usize), "{label}: at {tip}");
            }

            let (sink, committed, running) = start(store, NonZeroUsize::MIN);
            sink.send(step(&chain[count as usize], None)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let next = trees(committed.borrow().clone(), count);
            assert_eq!(next, seen(count as usize + 1), "{label}: folding on after recovery");
        }
    }

    #[derive(Debug, Clone)]
    enum Move {
        /// Next blocks: unfolded until `Fold`, folded after it
        Send(usize),
        /// Bulk → tip handoff: every later block folded
        Fold,
        /// Shutdown, reopen, resend from one below the committed tip (held: skipped)
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random chains through random final streams (bulk runs, tip steps, restarts): once
        /// every move commits, every height serves exactly the naive frontier's three trees and
        /// nothing past the tip is served
        #[test]
        fn random_histories_serve_the_naive_trees_at_every_height(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    4 => (1usize..=3).prop_map(Move::Send),
                    1 => proptest::strategy::Just(Move::Fold),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
            batch in proptest::prop_oneof![
                proptest::strategy::Just(NonZeroUsize::MIN),
                proptest::strategy::Just(QUEUE),
            ],
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves, batch));
        }
    }

    /// Leaves unique per pool: sapling 1.., orchard 10_001.., ironwood 20_001..
    async fn random_history(
        counts: Vec<(usize, usize, usize)>,
        moves: Vec<Move>,
        batch: NonZeroUsize,
    ) {
        let mut next = (1u32, 10_001u32, 20_001u32);
        let take = |count: usize, from: &mut u32| -> Vec<u32> {
            let taken = (*from..*from + count as u32).collect();
            *from += count as u32;
            taken
        };
        let leaves: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = counts
            .iter()
            .map(|&(s, o, i)| {
                let sapling = take(s, &mut next.0);
                let orchard = take(o, &mut next.1);
                let ironwood = take(i, &mut next.2);
                (sapling, orchard, ironwood)
            })
            .collect();
        let chain: Vec<Arc<Block>> =
            linked((0u32..).zip(&leaves).map(|(height, (s, o, i))| vec![pools(height, s, o, i)]));
        let folds = folded(&chain);
        let trees_through = |height: usize| {
            let (mut s, mut o, mut i) = (Vec::new(), Vec::new(), Vec::new());
            for (sapling, orchard, ironwood) in &leaves[..=height] {
                s.extend_from_slice(sapling);
                o.extend_from_slice(orchard);
                i.extend_from_slice(ironwood);
            }
            (sapling_tree(&s), orchard_tree(&o), orchard_tree(&i))
        };

        let fs = SimFs::new();
        let (mut sink, mut committed, mut running) = start(open(&fs), batch);
        let (mut sent, mut folding) = (0usize, false);
        for (at, next) in moves.iter().enumerate() {
            match *next {
                Move::Send(count) => {
                    for (block, folds) in chain.iter().zip(&folds).skip(sent).take(count) {
                        sink.send(step(block, folding.then_some(folds))).await;
                        sent += 1;
                    }
                }
                Move::Fold => folding = true,
                Move::Reopen => {
                    sink.shutdown();
                    running.await.expect("stops at Shutdown");
                    (sink, committed, running) = start(open(&fs), batch);
                    folding = false;
                    if let Some(held) = sent.checked_sub(1) {
                        sink.send(step(&chain[held], None)).await;
                    }
                }
            }
            let tip = sent.checked_sub(1).map(|last| last as u32);
            reached(&mut committed, tip).await;

            let label = format!("move {at} {next:?}");
            let reader = TreeStateReader::new(committed.borrow().clone(), NETWORK);
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
    /// height, named by that block, whether folded by the writer (0, 1) or sent folded (2, 3);
    /// resumable from any `start_index` (`start_index == count` = empty, pepper-sync's probe); a
    /// reopen appends the next boundary after the ones on disk
    #[tokio::test]
    async fn subtree_roots_resume_from_start_index() {
        const HALF: u32 = 1 << 15;
        let fs = SimFs::new();
        let orchard = |first: u32, count: u32| (first..first + count).collect::<Vec<u32>>();
        // orchard subtree 0 completes at height 1, subtree 1 at height 3, subtree 2 at 4 (sent
        // after a reopen)
        let blocks: Vec<Arc<Block>> = linked(
            [
                (0, orchard(1, HALF)),
                (1, orchard(1 + HALF, HALF)),
                (2, orchard(1 + 2 * HALF, 1)),
                (3, orchard(2 + 2 * HALF, 2 * HALF - 1)),
                (4, orchard(1 + 4 * HALF, 2 * HALF)),
            ]
            .into_iter()
            .map(|(seed, leaves)| vec![pools(seed, &[], &leaves, &[])]),
        );
        let folds = folded(&blocks);

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

        let (sink, mut committed, running) = start(open(&fs), QUEUE);
        for (at, block) in blocks[..4].iter().enumerate() {
            sink.send(step(block, (at >= 2).then_some(&folds[at]))).await;
        }
        reached(&mut committed, Some(3)).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let view = TreeStateReader::new(open(&fs).view(), NETWORK);
        let roots = view.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");
        let expected = [(1, &blocks[1]), (3, &blocks[3])].map(|(at, block)| SubtreeRoot {
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
        let (sink, committed, running) = start(open(&fs), QUEUE);
        sink.send(step(&blocks[4], None)).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let view = TreeStateReader::new(committed.borrow().clone(), NETWORK);
        let resumed_roots = view.subtree_roots(Orchard, 0, 0).expect("roots");
        let third = SubtreeRoot { root: tree_root(&view, 4), completing: completing(&blocks[4]) };
        assert_eq!(resumed_roots, [roots, vec![third]].concat(), "earlier entries untouched");
    }

    /// Every pool served as its real tree's serialization, never an empty field (both clients map
    /// an absent field onto `CommitmentTree::empty()` silently)
    #[tokio::test]
    async fn ironwood_serves_a_real_tree_not_an_empty_field() {
        let fs = SimFs::new();
        let (sink, committed, running) = start(open(&fs), QUEUE);
        // ironwood-only block: sapling and orchard empty at this height
        let genesis = linked([vec![pools(0, &[], &[], &[201, 202, 203])]]);
        sink.send(step(&genesis[0], None)).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let reader = TreeStateReader::new(committed.borrow().clone(), NETWORK);
        let trees = reader.treestate(Height::GENESIS).expect("served");

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
