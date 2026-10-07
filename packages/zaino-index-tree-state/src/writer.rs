//! tree_state index: each run of queued blocks → one [`fold_run`] on the parent read off the held
//! view, kept by its own loop
//!
//! - fold → `compute` (Merkle hashing, every core); storage tiers = `zaino_persistence::Tiered`
//! - nothing carried: open and reorg = the view's tip, the next fold reads its frontiers there
//!
//! Tiering: `docs/design/non-finalized-state.md`

use std::{num::NonZeroUsize, sync::Arc};

use zaino_persistence::{Changes, IndexKind, LayeredView, SequenceRead, Store, Tiered, View};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_sync::{Applied, Offloaded, Published, Step, Subscription, Weight};

use crate::{fold_run, TreeStateReader, HEIGHTS};

const NAME: &str = IndexKind::TreeState.name();

/// `batch_bytes` = one bulk commit's source bytes, and one run's (one fold batch)
pub struct TreeStateIndexWriter<S: Store> {
    tiered: Offloaded<Tiered<S>>,
    batch_bytes: NonZeroUsize,
    published: Published<TreeStateReader<LayeredView<S::View>>>,
}

impl<S: Store<View: SequenceRead>> TreeStateIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)) at its committed tip; `batch_bytes` =
    /// final blocks per bulk commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let tiered = Tiered::new(store, batch_bytes);
        let view = tiered.view();
        let held = view.tip().map_or(0, |tip| u64::from(tip.height) + 1);
        assert_eq!(view.len(HEIGHTS), held, "{NAME}: one record per committed height");
        let reader = TreeStateReader::new(view, tiered.schema().network);
        let published = Published::new(reader, tiered.durable_tip());
        Self { tiered: Offloaded::new(tiered), batch_bytes, published }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.tiered.get().durable_tip()
    }

    /// View, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<TreeStateReader<LayeredView<S::View>>> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown` (a failure panics: its dropped queue fails the rest)
    pub async fn run(mut self, mut blocks: Subscription<Block>) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized, data } => {
                    let run = blocks.run((height, finalized, data), self.batch_bytes);
                    self.apply_run(run).await
                }
                Step::Finalized { height } => self.finalize(height).await,
                Step::Reorg => self.reorg(),
                Step::Shutdown => return self.finalize_staged().await,
            }
        }
    }

    /// Run of queued blocks folded as one batch, then each in order: a final one (bulk sync)
    /// staged for the next batch commit, a tip one applied (staged blocks written first)
    async fn apply_run(&mut self, run: Vec<Applied<Block>>) {
        // at or below = replay for an index behind this one: already on disk
        let durable = self.durable_tip().map(|tip| tip.height);
        let run: Vec<_> = run.into_iter().filter(|(height, ..)| Some(*height) > durable).collect();
        let blocks: Vec<Arc<Block>> = run.iter().map(|(_, _, block)| Arc::clone(block)).collect();
        for ((height, finalized, block), changes) in run.into_iter().zip(self.fold(blocks).await) {
            if finalized {
                let full = self.tiered.get_mut().stage(changes, block.weight());
                self.published.merged(height);
                if full {
                    self.finalize(height).await;
                }
                continue;
            }
            self.finalize_staged().await;
            self.tiered.get_mut().apply(changes);
            self.publish();
        }
    }

    /// Back to the durable tip (= restart)
    fn reorg(&mut self) {
        self.tiered.get_mut().reorg();
        self.publish();
        self.published.reorged();
    }

    /// Staged bulk → disk (before a tip block builds on it, and at `Shutdown`)
    async fn finalize_staged(&mut self) {
        if let Some(staged) = self.tiered.get().staged() {
            self.finalize(staged.height).await;
        }
    }

    /// Every held block through `through` → disk
    async fn finalize(&mut self, through: Height) {
        self.tiered.blocking(move |tiered| tiered.finalize(through)).await;
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(self.durable_tip().map(|tip| tip.height));
    }

    /// `blocks`' `Changes`, one each, folded on the CPU pool onto everything held
    async fn fold(&self, blocks: Vec<Arc<Block>>) -> Vec<Changes> {
        if blocks.is_empty() {
            return Vec::new();
        }
        let parent = self.reader();
        let folded = zaino_sync::compute(move || {
            let blocks: Vec<&Block> = blocks.iter().map(|block| &**block).collect();
            fold_run(&parent, &blocks)
        });
        folded.await.unwrap_or_else(|error| panic!("{NAME} index: {error}"))
    }

    fn publish(&self) {
        self.published.view(self.reader(), self.tiered.get().applied());
    }

    fn reader(&self) -> TreeStateReader<LayeredView<S::View>> {
        let tiered = self.tiered.get();
        TreeStateReader::new(tiered.view(), tiered.schema().network)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{collections::VecDeque, path::Path, time::Duration};

    use incrementalmerkletree::{
        frontier::{CommitmentTree, Frontier},
        Hashable, Level,
    };
    use orchard::tree::MerkleHashOrchard;
    use proptest::strategy::Strategy as _;
    use tokio::sync::watch;
    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine};
    use zaino_primitives::testing::linked;
    use zaino_primitives::types::{
        CommitmentTreeBytes, CompactCiphertext, OrchardAction, OrchardData, SaplingData,
        SaplingOutput, ShieldedPool, SubtreeRoot, Transaction, TransactionId, TreeRoot,
    };
    use zaino_sync::BlockSink;
    use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree, HashSer};
    use zcash_protocol::consensus::NetworkType;

    use crate::{schema, ServeError};

    const PATH: &str = "/ts";
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    /// Final blocks wait for a whole bulk batch
    const BULK: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    /// Every final block its own write
    const EACH: NonZeroUsize = NonZeroUsize::MIN;

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

    fn open(fs: &Arc<SimFs>, batch: NonZeroUsize) -> TreeStateIndexWriter<DiskStore> {
        let store =
            DiskEngine::new(fs.clone()).open(Path::new(PATH), &schema(NetworkType::Regtest));
        TreeStateIndexWriter::new(store.expect("open"), batch)
    }

    fn apply(block: &Arc<Block>, finalized: bool) -> zaino_sync::Step<Block> {
        zaino_sync::Step::Apply {
            height: block.header().height,
            finalized,
            data: Arc::clone(block),
        }
    }

    /// Durable tip = a height, applied tip = a block
    trait AtHeight {
        fn height(&self) -> Height;
    }

    impl AtHeight for Height {
        fn height(&self) -> Height {
            *self
        }
    }

    impl AtHeight for BlockRef {
        fn height(&self) -> Height {
            self.height
        }
    }

    /// `tip` at `want` (a stuck index fails the test, never hangs it)
    async fn reached<T: AtHeight>(tip: &mut watch::Receiver<Option<T>>, want: Option<Height>) {
        let at = |at: &Option<T>| at.as_ref().map(T::height) == want;
        match tokio::time::timeout(Duration::from_secs(30), tip.wait_for(at)).await {
            Ok(reached) => _ = reached.expect("index alive"),
            Err(_) => panic!("never reached {want:?}"),
        }
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

    fn chain() -> Vec<Arc<Block>> {
        linked((0u32..).zip(&CHAIN).map(|(height, (s, o, i))| vec![pools(height, s, o, i)]))
    }

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

    /// Four final blocks, each its own write, crashed after every operation: each state reopens
    /// (proven) to an acknowledged or attempted write, the committed tip's trees equal the naive
    /// oracle's, and the next block, sent through a fresh sink, folds on correctly
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_proven_prefix_that_keeps_folding() {
        let fs = SimFs::recording();
        let chain = chain();
        {
            let index = open(&fs, EACH);
            let mut durable = index.published().subscribe_finalized();
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            for (acked, block) in (1u64..).zip(&chain[..4]) {
                sink.send(apply(block, true)).await;
                reached(&mut durable, Some(block.header().height)).await;
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("followed through Shutdown");
        }

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let index = open(&state.fs, EACH);
            let count = index.durable_tip().map_or(0, |tip| u64::from(tip.height) + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(4));
            assert!(acked.contains(&count), "{label}: recovered {count}");
            let served = index.published().served();
            if count > 0 {
                let tip = h(count as u32 - 1);
                let trees = served.pin_any().treestate(tip).expect("committed tip");
                let trees = (trees.sapling, trees.orchard, trees.ironwood);
                assert_eq!(trees, seen(count as usize), "{label}: trees at {tip}");
            }

            let next = count as usize;
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            sink.send(apply(&chain[next], true)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let trees = served.pin_any().treestate(h(next as u32)).expect("folded on");
            let trees = (trees.sapling, trees.orchard, trees.ironwood);
            assert_eq!(trees, seen(next + 1), "{label}: folding on after recovery");
        }
    }

    /// What the proptest's producer does next (always a sequence a real producer sends)
    #[derive(Debug, Clone)]
    enum Move {
        /// Next block, non-final (the tip)
        Apply,
        /// `n` blocks final: the window's oldest first (`Finalized`), then bulk final `Apply`s
        Finalize(usize),
        /// `Reorg` with nothing replayed (a bare retreat onto the final boundary)
        Reorg,
        /// Shutdown, reopen, a fresh sink from the durable tip
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random chains through random producer step sequences and reopens: after every move,
        /// every applied height serves exactly the naive frontier's three trees and nothing past
        /// it is served; a reorg or a reopen lands applied on durable
        #[test]
        fn random_histories_serve_the_naive_trees_at_every_applied_height(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            moves in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => proptest::strategy::Just(Move::Apply),
                    2 => (1usize..=4).prop_map(Move::Finalize),
                    1 => proptest::strategy::Just(Move::Reorg),
                    1 => proptest::strategy::Just(Move::Reopen),
                ],
                1..16,
            ),
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(random_history(counts, moves));
        }
    }

    /// Leaves unique per pool: sapling 1.., orchard 10_001.., ironwood 20_001..
    async fn random_history(counts: Vec<(usize, usize, usize)>, moves: Vec<Move>) {
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
        // the producer's side: next height to send, the non-final window, blocks made final
        let (mut sent, mut window, mut finals) = (0usize, VecDeque::new(), 0usize);
        let tip = |count: usize| count.checked_sub(1).map(|last| h(last as u32));
        let boot = || {
            let index = open(&fs, EACH);
            let published = index.published();
            let watched = (published.served(), published.subscribe_applied());
            let (served, applied, durable) =
                (watched.0, watched.1, published.subscribe_finalized());
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            (sink, running, served, applied, durable)
        };
        let (mut sink, mut running, mut served, mut applied, mut durable) = boot();
        for (at, move_) in moves.iter().enumerate() {
            match *move_ {
                Move::Apply if sent < chain.len() => {
                    sink.send(apply(&chain[sent], false)).await;
                    window.push_back(sent);
                    sent += 1;
                }
                Move::Finalize(count) => {
                    for _ in 0..count {
                        if let Some(oldest) = window.pop_front() {
                            sink.send(zaino_sync::Step::Finalized { height: h(oldest as u32) })
                                .await;
                        } else if sent < chain.len() {
                            sink.send(apply(&chain[sent], true)).await;
                            sent += 1;
                        } else {
                            break;
                        }
                        finals += 1;
                    }
                }
                Move::Reorg => {
                    sink.send(zaino_sync::Step::Reorg).await;
                    window.clear();
                    sent = finals;
                }
                Move::Reopen => {
                    sink.shutdown();
                    running.await.expect("followed through Shutdown");
                    (sink, running, served, applied, durable) = boot();
                    window.clear();
                    sent = finals;
                }
                _ => {}
            }
            reached(&mut applied, tip(sent)).await;
            reached(&mut durable, tip(finals)).await;
            let view = served.pin_any();

            let label = format!("move {at} {move_:?}");
            let (applied, finalized) = (view.tip(), tip(finals));
            assert_eq!(applied, tip(sent), "{label}: view = the applied tip");
            assert_eq!(*durable.borrow(), view.finalized(), "{label}: durable = the store's tip");
            assert!(finalized <= applied, "{label}: durable past applied");
            if matches!(move_, Move::Reorg | Move::Reopen) {
                assert_eq!(applied, finalized, "{label}: nothing non-final survives");
            }
            for height in applied.into_iter().flat_map(|tip| Height::GENESIS.up_to(tip)) {
                let height = u32::from(height);
                let served = view.treestate(h(height));
                let served = served.unwrap_or_else(|error| panic!("{label}: {error}"));
                let trees = (served.sapling, served.orchard, served.ironwood);
                assert_eq!(trees, trees_through(height as usize), "{label}: trees at {height}");
            }
            let past = applied.map_or(Height::GENESIS, Height::next);
            let absent = Err(ServeError::NotFound { height: past });
            assert_eq!(view.treestate(past), absent, "{label}: past applied");
        }
        sink.shutdown();
        running.await.expect("followed through Shutdown");
    }

    /// A gap, or a first block above the durable tip = every later commitment silently
    /// mis-positioned → the index panics, never skips
    #[tokio::test]
    async fn a_gap_in_the_block_stream_panics() {
        for (steps, expected) in [
            (vec![(0, false), (2, false)], "tree_state: blocks must arrive contiguously"),
            (vec![(1, true)], "tree_state: final blocks must arrive contiguously"),
        ] {
            let index = open(&SimFs::new(), BULK);
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            let blocks = linked((0..3).map(|seed| vec![pools(seed, &[1], &[101], &[201])]));
            for (height, finalized) in steps {
                sink.send(apply(&blocks[height], finalized)).await;
            }
            sink.shutdown();
            let panic = running.await.expect_err("panicked").into_panic();
            let message = panic.downcast_ref::<String>().map(String::as_str);
            assert!(message.is_some_and(|m| m.contains(expected)), "{message:?}");
        }
    }

    /// Real 2^16-leaf subtrees: each root = the served tree's own level-16 root at its completing
    /// height, named by that block, from the non-finalized tier and after the split to disk alike;
    /// resumable from any `start_index` (`start_index == count` = empty, pepper-sync's probe); a
    /// reopen appends the next boundary after the ones on disk
    #[tokio::test]
    async fn subtree_roots_resume_from_start_index() {
        const HALF: u32 = 1 << 15;
        let fs = SimFs::new();
        let orchard = |first: u32, count: u32| (first..first + count).collect::<Vec<u32>>();
        // orchard subtree 0 completes at height 1, subtree 1 at height 3, subtree 2 at 4 (sent
        // after a reopen)
        let mut blocks: Vec<Arc<Block>> = linked(
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
        let fifth = blocks.pop().expect("five blocks");

        let completing =
            |block: &Block| BlockRef { hash: block.header().hash, height: block.header().height };
        // level-16 root of the subtree holding the tip of the orchard tree served at `height`
        let tree_root = |view: &TreeStateReader<LayeredView<DiskView>>, height: u32| {
            let tree = view.treestate(h(height)).expect("served").orchard;
            let frontier = read_commitment_tree::<MerkleHashOrchard, _, 32>(tree.as_bytes())
                .expect("parses")
                .to_frontier();
            let root = frontier.value().expect("non-empty").root(Some(Level::from(16)));
            let mut bytes = [0u8; 32];
            root.write(&mut bytes[..]).expect("32 bytes");
            TreeRoot::from(bytes)
        };
        let expected = |view: &TreeStateReader<LayeredView<DiskView>>| {
            let first =
                SubtreeRoot { root: tree_root(view, 1), completing: completing(&blocks[1]) };
            let second =
                SubtreeRoot { root: tree_root(view, 3), completing: completing(&blocks[3]) };
            vec![first, second]
        };

        let index = open(&fs, BULK);
        let served = index.published().served();
        let (mut applied, mut durable) =
            (index.published().subscribe_applied(), index.published().subscribe_finalized());
        let mut sink = BlockSink::new("blocks");
        let feed = sink.subscribe(NAME, QUEUE);
        let running = tokio::spawn(index.run(feed));
        for pending in &blocks {
            sink.send(apply(pending, false)).await;
        }
        reached(&mut applied, Some(h(3))).await;
        let buffered = served.pin_any();
        let roots = buffered.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");
        assert_eq!(roots, expected(&buffered), "non-finalized: two boundaries, completing blocks");

        for height in [0, 1] {
            sink.send(zaino_sync::Step::Finalized { height: h(height) }).await;
        }
        reached(&mut durable, Some(h(1))).await;
        let split = served.pin_any();
        assert_eq!(split.subtree_roots(ShieldedPool::Orchard, 0, 0), Ok(roots.clone()), "split");

        // slot = subtree index → resume = a seek, bound = a prefix
        use ShieldedPool::{Orchard, Sapling};
        assert_eq!(split.subtree_roots(Orchard, 1, 0).expect("resume"), roots[1..]);
        assert_eq!(split.subtree_roots(Orchard, 0, 1).expect("bounded"), roots[..1]);
        assert_eq!(split.subtree_roots(Orchard, 2, 0).expect("probe"), Vec::new());
        assert_eq!(split.subtree_roots(Sapling, 0, 0).expect("untouched pool"), Vec::new());

        // reopen rebuilds the subtree cursor from the files; the next boundary follows it
        for height in [2, 3] {
            sink.send(zaino_sync::Step::Finalized { height: h(height) }).await;
        }
        sink.shutdown();
        running.await.expect("followed through Shutdown");
        let resumed = open(&fs, BULK);
        let served = resumed.published().served();
        let mut sink = BlockSink::new("blocks");
        let feed = sink.subscribe(NAME, QUEUE);
        let running = tokio::spawn(resumed.run(feed));
        sink.send(apply(&fifth, true)).await;
        sink.shutdown();
        running.await.expect("followed through Shutdown");

        let view = served.pin_any();
        let resumed_roots = view.subtree_roots(Orchard, 0, 0).expect("roots");
        let third = SubtreeRoot { root: tree_root(&view, 4), completing: completing(&fifth) };
        assert_eq!(resumed_roots, [roots, vec![third]].concat(), "earlier entries untouched");
    }

    /// Every pool served as its real tree's serialization, never an empty field (both clients map
    /// an absent field onto `CommitmentTree::empty()` silently)
    #[tokio::test]
    async fn ironwood_serves_a_real_tree_not_an_empty_field() {
        let index = open(&SimFs::new(), BULK);
        let served = index.published().served();
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));
        // ironwood-only block: sapling and orchard empty at this height
        let genesis = linked([vec![pools(0, &[], &[], &[201, 202, 203])]]);
        sink.send(apply(&genesis[0], true)).await;
        sink.shutdown();
        running.await.expect("followed through Shutdown");
        let trees = served.pin_any().treestate(h(0)).expect("served");

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
