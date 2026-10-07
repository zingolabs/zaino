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
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::{
        BlockRef, CommitmentTreeBytes, NoteCommitment, ShieldedPool, SubtreeRoot, TreeRoot,
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

    /// Writer over `store`, its final stream and committed view
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
            let (sink, mut committed, running) = start(open(&fs), NonZeroUsize::MIN);
            for (acked, block) in (1u64..).zip(&blocks[..5]) {
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
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5) as u32);
            assert!(acked.contains(&count), "{label}: recovered {count}");
            if let Some(tip) = count.checked_sub(1) {
                assert_eq!(trees(store.view(), tip), seen(tip), "{label}: at {tip}");
            }

            let (sink, committed, running) = start(store, NonZeroUsize::MIN);
            sink.send(step(&blocks[count as usize], None)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let next = trees(committed.borrow().clone(), count);
            assert_eq!(next, seen(count), "{label}: folding on after recovery");
        }
    }

    /// - `Send(n)`: next `n` blocks, unfolded until `Fold`, folded after it
    /// - `Fold`: bulk → tip handoff; `Reopen`: shutdown, reopen, resend from one below the tip
    ///   (held: skipped)
    #[derive(Debug, Clone)]
    enum Move {
        Send(usize),
        Fold,
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

    /// Genesis, then one block per `counts` entry; leaves unique per pool: sapling 1.., orchard
    /// 10_001.., ironwood 20_001.. (nullifier = the leaf, little-endian)
    async fn random_history(
        counts: Vec<(usize, usize, usize)>,
        moves: Vec<Move>,
        batch: NonZeroUsize,
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
        let folds = folded(&chain);
        let trees_through = |height: usize| naive_trees(&chain[..=height]);

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
    /// height, named by that block, whether folded by the writer (0..=2) or sent folded (3, 4);
    /// resumable from any `start_index` (`start_index == count` = empty, pepper-sync's probe); a
    /// reopen appends the next boundary after the ones on disk
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
        for (at, block) in blocks[..5].iter().enumerate() {
            sink.send(step(block, (at >= 3).then_some(&folds[at]))).await;
        }
        reached(&mut committed, Some(4)).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let view = TreeStateReader::new(open(&fs).view(), NETWORK);
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
        let (sink, committed, running) = start(open(&fs), QUEUE);
        sink.send(step(&blocks[5], None)).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let view = TreeStateReader::new(committed.borrow().clone(), NETWORK);
        let resumed_roots = view.subtree_roots(Orchard, 0, 0).expect("roots");
        let third = SubtreeRoot { root: tree_root(&view, 5), completing: completing(&blocks[5]) };
        assert_eq!(resumed_roots, [roots, vec![third]].concat(), "earlier entries untouched");
    }

    /// Every pool served as its real tree's serialization, never an empty field (both clients map
    /// an absent field onto `CommitmentTree::empty()` silently)
    #[tokio::test]
    async fn ironwood_serves_a_real_tree_not_an_empty_field() {
        let fs = SimFs::new();
        let (sink, committed, running) = start(open(&fs), QUEUE);
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
            sink.send(step(&block, None)).await;
        }
        sink.shutdown();
        running.await.expect("stops at Shutdown");
        let reader = TreeStateReader::new(committed.borrow().clone(), NETWORK);
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
