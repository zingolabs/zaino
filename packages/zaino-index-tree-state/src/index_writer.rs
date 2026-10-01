//! tree_state index: one fold per block into the non-finalized tier, written out by its own loop
//!
//! - two carries: `applied` moved by `apply`, `durable` by a commit
//! - reorg = `applied = durable.clone()` (no reverse fold, no disk read)
//! - a commit folds whatever `apply` never saw (bulk sync skips the non-finalized tier)
//! - fold → `compute` (Merkle hashing, every core), write → the blocking pool
//!
//! Tiering: `docs/design/non-finalized-state.md`

use std::{num::NonZeroUsize, sync::Arc};

use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use zaino_primitives::types::{
    Block, BlockRef, Height, PerPool, ShieldedPool, TreeSize, TreeSizes,
};
use zaino_sync::{Offloaded, Published, Step, Subscription, Weight};
use zcash_primitives::merkle_tree::HashSer;

use crate::{
    fold::{node_from_bytes, PoolFold},
    heights::TreeStateHeight,
    view::PoolSizes,
    IndexWriterError, NonFinalizedTrees, ReadView, Snapshot, TreeStateStore,
};

/// One running frontier per pool (the state a fold carries forward)
#[derive(Debug, Clone)]
struct Carries {
    sapling: PoolFold<sapling_crypto::Node>,
    orchard: PoolFold<MerkleHashOrchard>,
    ironwood: PoolFold<MerkleHashOrchard>,
}

impl Carries {
    /// Rebuilt at `sizes` through `frontier_at` over `non_finalized` then `durable` (no hashing)
    fn seed(
        non_finalized: &NonFinalizedTrees,
        durable: &Snapshot,
        sizes: PoolSizes,
    ) -> Result<Self, IndexWriterError> {
        fn pool<H: Hashable + HashSer + Clone>(
            non_finalized: &NonFinalizedTrees,
            durable: &Snapshot,
            size: u64,
            pool: ShieldedPool,
        ) -> Result<PoolFold<H>, IndexWriterError> {
            PoolFold::seed(non_finalized.nodes(durable, pool, size))
                .ok_or(IndexWriterError::Inconsistent { size })
        }

        Ok(Self {
            sapling: pool(non_finalized, durable, sizes.sapling, ShieldedPool::Sapling)?,
            orchard: pool(non_finalized, durable, sizes.orchard, ShieldedPool::Orchard)?,
            ironwood: pool(non_finalized, durable, sizes.ironwood, ShieldedPool::Ironwood)?,
        })
    }

    /// Folds `blocks` (contiguous from the carry) into `out`, one height record per block
    ///
    /// - every leaf decoded before any append: a refused block leaves the carry untouched
    /// - pools fold concurrently; wide levels split across cores inside each node type's
    ///   `Hashable::combine_pairs`
    fn fold(
        &mut self,
        blocks: &[Arc<Block>],
        out: &mut NonFinalizedTrees,
    ) -> Result<(), IndexWriterError> {
        let Some(last) = blocks.last() else {
            return Ok(());
        };
        let sapling = PoolBatch::<sapling_crypto::Node>::decode(blocks, ShieldedPool::Sapling)?;
        let orchard = PoolBatch::<MerkleHashOrchard>::decode(blocks, ShieldedPool::Orchard)?;
        let ironwood = PoolBatch::<MerkleHashOrchard>::decode(blocks, ShieldedPool::Ironwood)?;

        let before = PerPool {
            sapling: self.sapling.size(),
            orchard: self.orchard.size(),
            ironwood: self.ironwood.size(),
        };
        let mut sizes: TreeSizes = before.map(|size| {
            TreeSize::try_from(size).expect("pool size within u32 (consensus caps the note count)")
        });
        for block in blocks {
            let header = block.header();
            sizes =
                sizes.advance(block).expect("pool size within u32 (consensus caps the note count)");
            out.heights.insert(
                header.height,
                TreeStateHeight { hash: header.hash, time: header.time, sizes },
            );
        }

        let PerPool { sapling: into_sapling, orchard: into_orchard, ironwood: into_ironwood } =
            &mut out.pools;
        rayon::join(
            || self.sapling.append_batch(sapling.leaves, &sapling.ends, into_sapling),
            || {
                rayon::join(
                    || self.orchard.append_batch(orchard.leaves, &orchard.ends, into_orchard),
                    || self.ironwood.append_batch(ironwood.leaves, &ironwood.ends, into_ironwood),
                )
            },
        );
        out.tip = Some(last.header().height);

        Ok(())
    }
}

/// One pool's leaves over a batch of blocks; `ends` = `(height, leaves through that block)`
struct PoolBatch<H> {
    leaves: Vec<H>,
    ends: Vec<(Height, usize)>,
}

impl<H: HashSer> PoolBatch<H> {
    /// Note commitments → nodes (non-canonical = the block refused, typed)
    fn decode(blocks: &[Arc<Block>], pool: ShieldedPool) -> Result<Self, IndexWriterError> {
        let mut leaves = Vec::new();
        let mut ends = Vec::with_capacity(blocks.len());
        for block in blocks {
            let height = block.header().height;
            let leaf = |bytes: [u8; 32]| {
                node_from_bytes(&bytes).ok_or(IndexWriterError::Commitment { height })
            };
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
        Ok(Self { leaves, ends })
    }
}

/// Everything a fold owns: both carries + the non-finalized tier
#[derive(Debug)]
struct Folds {
    durable: Carries,
    applied: Carries,
    non_finalized: NonFinalizedTrees,
}

/// Final blocks through `cut`, folded: `chunk` goes to disk; `durable` + the tier above `cut`
/// replace the writer's state once it is there
struct Landing {
    chunk: NonFinalizedTrees,
    sizes: PoolSizes,
    durable: Carries,
}

impl Folds {
    /// Folds `bulk` (final blocks `apply` never saw) onto the non-finalized tier, takes the tier
    /// through `cut` (inclusive) as the chunk to write (the tier keeps it until written)
    ///
    /// - `durable` = the carry at `cut`, from the chunk over what is already on disk (no read-back)
    fn land(
        &mut self,
        bulk: &[Arc<Block>],
        cut: Height,
        on_disk: &Snapshot,
    ) -> Result<Landing, IndexWriterError> {
        self.applied.fold(bulk, &mut self.non_finalized)?;
        let tip = self.non_finalized.tip;
        assert!(Some(cut) <= tip, "tree_state: committing {cut} past the applied tip {tip:?}");
        let sizes = self.non_finalized.heights[&cut].positions();
        let (chunk, _) = self.non_finalized.split(cut, sizes);
        let durable = Carries::seed(&chunk, on_disk, sizes)?;
        Ok(Landing { chunk, sizes, durable })
    }
}

/// - `durable` / `snapshot` = the store as of the last commit (what views pin)
/// - `view` = the non-finalized tier as last published
/// - `bulk` = final blocks not yet committed, `bulk_bytes` their [`Weight`]
pub struct TreeStateIndexWriter {
    folds: Offloaded<Folds>,
    store: Offloaded<TreeStateStore>,
    durable: Option<BlockRef>,
    snapshot: Arc<Snapshot>,
    view: NonFinalizedTrees,
    bulk: Vec<Arc<Block>>,
    bulk_bytes: usize,
    batch_bytes: NonZeroUsize,
    published: Published<ReadView>,
}

impl TreeStateIndexWriter {
    pub const NAME: &'static str = "tree_state";

    /// Carries seeded from what `store` holds (the restart path: every boot); `batch_bytes` =
    /// final blocks per bulk commit (one fsync)
    pub fn new(store: TreeStateStore, batch_bytes: NonZeroUsize) -> Result<Self, IndexWriterError> {
        let durable = store.finalized_tip();
        let finalized = durable.map(|tip| tip.height);
        let snapshot = store.snapshot();
        let sizes = match finalized {
            None => PoolSizes::default(),
            Some(last) => {
                snapshot.height(last).expect("a store holds its committed tip record").positions()
            }
        };

        let view = NonFinalizedTrees::empty_at(finalized);
        let carry = Carries::seed(&view, &snapshot, sizes)?;
        let published =
            Published::new(ReadView::new(view.clone(), Arc::clone(&snapshot)), finalized);

        Ok(Self {
            folds: Offloaded::new(Folds {
                applied: carry.clone(),
                durable: carry,
                non_finalized: view.clone(),
            }),
            store: Offloaded::new(store),
            durable,
            snapshot,
            view,
            bulk: Vec::new(),
            bulk_bytes: 0,
            batch_bytes,
            published,
        })
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.durable
    }

    /// View, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<ReadView> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown` (a failure panics: its dropped queue fails the rest)
    pub async fn run(mut self, mut blocks: Subscription<Block>) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized: true, data } => {
                    // replay for an index behind this one: already on disk
                    if Some(height) <= self.durable.map(|tip| tip.height) {
                        continue;
                    }
                    let next = self.bulk.last().map_or(self.view.tip, |b| Some(b.header().height));
                    let next = next.map_or(Height::GENESIS, Height::next);
                    assert_eq!(height, next, "tree_state: final blocks must arrive contiguously");
                    self.bulk_bytes += data.weight();
                    self.bulk.push(data);
                    if self.bulk_bytes >= self.batch_bytes.get() {
                        self.commit(height).await;
                    }
                }
                Step::Apply { height, finalized: false, data } => {
                    // bulk → tip: what bulk staged commits before the first apply builds on it
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height).await;
                    }
                    let next = self.view.tip.map_or(Height::GENESIS, Height::next);
                    assert_eq!(height, next, "tree_state: blocks must arrive contiguously");
                    let folded = self
                        .folds
                        .compute(move |folds| {
                            folds
                                .applied
                                .fold(std::slice::from_ref(&data), &mut folds.non_finalized)
                        })
                        .await;
                    if let Err(error) = folded {
                        panic!("{} index: {error}", Self::NAME);
                    }
                    self.view = self.folds.get().non_finalized.clone();
                }
                Step::Finalized { height } => self.commit(height).await,
                Step::Reorg => {
                    assert!(self.bulk.is_empty(), "tree_state: reorg with bulk blocks staged");
                    // back to the durable carry, the winning branch applied from there (= restart)
                    let folds = self.folds.get_mut();
                    folds.applied = folds.durable.clone();
                    folds.non_finalized =
                        NonFinalizedTrees::empty_at(self.durable.map(|tip| tip.height));
                    self.view = folds.non_finalized.clone();
                    self.publish();
                    self.published.reorged();
                }
                Step::Shutdown => {
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height).await;
                    }
                    return;
                }
            }
            self.publish();
        }
    }

    /// Every final block through `through` → folded (bulk ones), written, then the written prefix
    /// leaves the non-finalized tier
    async fn commit(&mut self, through: Height) {
        let bulk = std::mem::take(&mut self.bulk);
        self.bulk_bytes = 0;
        let on_disk = Arc::clone(&self.snapshot);
        let landed = self.folds.compute(move |folds| folds.land(&bulk, through, &on_disk)).await;
        let Landing { chunk, sizes, durable } =
            landed.unwrap_or_else(|error| panic!("{} index: {error}", Self::NAME));
        let first = self.durable.map_or(Height::GENESIS, |tip| tip.height.next());
        assert_eq!(
            chunk.heights.keys().next().copied(),
            Some(first),
            "tree_state: final blocks not contiguous from durable"
        );
        let written = self.store.blocking(move |store| store.write(&chunk)).await;
        let store = self.store.get();
        if let Err(error) = written {
            error.commit_failed(Self::NAME, store.path());
        }
        self.durable = store.finalized_tip();
        self.snapshot = store.snapshot();
        assert_eq!(self.durable.map(|tip| tip.height), Some(through), "tree_state: wrote short");
        let folds = self.folds.get_mut();
        folds.durable = durable;
        folds.non_finalized = folds.non_finalized.split(through, sizes).1;
        self.view = folds.non_finalized.clone();
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(Some(through));
    }

    fn publish(&self) {
        let view = ReadView::new(self.view.clone(), Arc::clone(&self.snapshot));
        self.published.view(view, self.view.tip);
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
    use proptest::strategy::Strategy as _;
    use tokio::sync::watch;
    use zaino_persistence::{fs::SimFs, pages::PageError, StoreError};
    use zaino_primitives::types::{
        BlockHeader, BlockRef, CommitmentTreeBytes, CompactCiphertext, OrchardAction, OrchardData,
        SaplingData, SaplingOutput, SubtreeRoot, Transaction, TransactionId, TreeRoot,
    };
    use zaino_sync::BlockSink;
    use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree};
    use zcash_protocol::consensus::NetworkType;

    use crate::ServeError;

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

    /// One transaction per pool's commitment list
    fn block(height: u32, sapling: &[u32], orchard: &[u32], ironwood: &[u32]) -> Arc<Block> {
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

        Arc::new(Block::new(
            BlockHeader::for_tests(
                height,
                [height as u8; 32],
                [height.wrapping_sub(1) as u8; 32],
                1_700_000_000 + height,
            ),
            vec![Transaction {
                txid: TransactionId::from([height as u8; 32]),
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
            }],
        ))
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

    fn open(
        fs: &Arc<SimFs>,
        batch: NonZeroUsize,
    ) -> Result<TreeStateIndexWriter, IndexWriterError> {
        let store = TreeStateStore::open(fs.clone(), Path::new(PATH), NetworkType::Regtest)?;
        TreeStateIndexWriter::new(store, batch)
    }

    fn apply(block: &Arc<Block>, finalized: bool) -> zaino_sync::Step<Block> {
        zaino_sync::Step::Apply {
            height: block.header().height,
            finalized,
            data: Arc::clone(block),
        }
    }

    /// `tip` at `want` (a stuck index fails the test, never hangs it)
    async fn reached(tip: &mut watch::Receiver<Option<Height>>, want: Option<Height>) {
        match tokio::time::timeout(Duration::from_secs(30), tip.wait_for(|at| *at == want)).await {
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
        (0u32..).zip(&CHAIN).map(|(height, (s, o, i))| block(height, s, o, i)).collect()
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
            let index = open(&fs, EACH).expect("open");
            let mut durable = index.published().subscribe_finalized();
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
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
            let index = open(&state.fs, EACH).unwrap_or_else(|error| panic!("{label}: {error}"));
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
            let blocks = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            sink.send(apply(&chain[next], true)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let trees = served.pin_any().treestate(h(next as u32)).expect("folded on");
            let trees = (trees.sapling, trees.orchard, trees.ironwood);
            assert_eq!(trees, seen(next + 1), "{label}: folding on after recovery");
        }
    }

    /// Reopen: a committed node missing or rewritten = refused (never zero-filled: zero reads back
    /// as a real node), bytes past the commit (a crash before the manifest) = truncated
    #[tokio::test]
    async fn reopen_refuses_a_lost_or_torn_node_file_and_truncates_surplus() {
        let fs = SimFs::new();
        let index = open(&fs, BULK).expect("open");
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));
        for block in &chain() {
            sink.send(apply(block, true)).await;
        }
        sink.shutdown();
        running.await.expect("bulk written at Shutdown");

        let leaves = Path::new(PATH).join("sapling").join("l00.dat");
        // a reopen truncates to the seal: what is left = exactly the committed bytes (the zeroed
        // reserve a writer grows past them is uncommitted)
        drop(open(&fs, BULK).expect("reopen"));
        let committed = fs.contents(&leaves).expect("leaves");
        let surplus = [committed.as_slice(), &[0xff; 64]].concat();
        let reopen = |bytes: Vec<u8>| {
            fs.corrupt(&leaves, |file| *file = bytes);
            open(&fs, BULK)
        };

        let mut torn = committed.clone();
        *torn.last_mut().expect("committed leaves") ^= 0x01;
        let lost = reopen(committed[..committed.len() - 32].to_vec()).err();
        let torn = reopen(torn).err();
        use {IndexWriterError::Store, PageError::Lost, PageError::Tail, StoreError::Page};
        const L00: &str = "sapling/l00.dat";
        assert!(matches!(lost, Some(Store(Page(Lost { path, .. }))) if path.ends_with(L00)));
        assert!(matches!(torn, Some(Store(Page(Tail { path }))) if path.ends_with(L00)));

        let resumed = reopen(surplus).expect("surplus is not an error");
        assert_eq!(
            resumed.durable_tip().map(|tip| tip.height),
            Some(h(4)),
            "resumes where it stopped"
        );
        assert_eq!(fs.contents(&leaves).expect("leaves"), committed, "surplus truncated");
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
            (0u32..).zip(&leaves).map(|(height, (s, o, i))| block(height, s, o, i)).collect();
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
            let index = open(&fs, EACH).expect("open");
            let published = index.published();
            let watched = (published.served(), published.subscribe_applied());
            let (served, applied, durable) =
                (watched.0, watched.1, published.subscribe_finalized());
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
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
            let index = open(&SimFs::new(), BULK).expect("open");
            let mut sink = BlockSink::new("blocks");
            let blocks = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
            let running = tokio::spawn(index.run(blocks));
            for (height, finalized) in steps {
                sink.send(apply(&block(height, &[1], &[101], &[201]), finalized)).await;
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
        // orchard subtree 0 completes at height 1, subtree 1 at height 3
        let blocks: Vec<Arc<Block>> = [
            (0, orchard(1, HALF)),
            (1, orchard(1 + HALF, HALF)),
            (2, orchard(1 + 2 * HALF, 1)),
            (3, orchard(2 + 2 * HALF, 2 * HALF - 1)),
        ]
        .into_iter()
        .map(|(height, leaves)| block(height, &[], &leaves, &[]))
        .collect();

        let completing =
            |block: &Block| BlockRef { hash: block.header().hash, height: block.header().height };
        // level-16 root of the subtree holding the tip of the orchard tree served at `height`
        let tree_root = |view: &ReadView, height: u32| {
            let tree = view.treestate(h(height)).expect("served").orchard;
            let frontier = read_commitment_tree::<MerkleHashOrchard, _, 32>(tree.as_bytes())
                .expect("parses")
                .to_frontier();
            let root = frontier.value().expect("non-empty").root(Some(Level::from(16)));
            let mut bytes = [0u8; 32];
            root.write(&mut bytes[..]).expect("32 bytes");
            TreeRoot::from(bytes)
        };
        let expected = |view: &ReadView| {
            let first =
                SubtreeRoot { root: tree_root(view, 1), completing: completing(&blocks[1]) };
            let second =
                SubtreeRoot { root: tree_root(view, 3), completing: completing(&blocks[3]) };
            vec![first, second]
        };

        let index = open(&fs, BULK).expect("open");
        let served = index.published().served();
        let (mut applied, mut durable) =
            (index.published().subscribe_applied(), index.published().subscribe_finalized());
        let mut sink = BlockSink::new("blocks");
        let feed = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
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
        let resumed = open(&fs, BULK).expect("reopen");
        let served = resumed.published().served();
        let mut sink = BlockSink::new("blocks");
        let feed = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(resumed.run(feed));
        let fifth = block(4, &[], &orchard(1 + 4 * HALF, 2 * HALF), &[]);
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
        let index = open(&SimFs::new(), BULK).expect("open");
        let served = index.published().served();
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(TreeStateIndexWriter::NAME, QUEUE);
        let running = tokio::spawn(index.run(blocks));
        // ironwood-only block: sapling and orchard empty at this height
        sink.send(apply(&block(0, &[], &[], &[201, 202, 203]), true)).await;
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
