//! [`IndexWriter`]: one fold per block into the non-finalized tier, written out on `finalize`
//!
//! - two carries: `applied` moved by [`apply`](TreeStateIndexWriter::apply), `durable` by
//!   [`finalize`](TreeStateIndexWriter::finalize)
//! - `reset` = `applied = durable.clone()` (no reverse fold, no disk read)
//! - `finalize` folds whatever `apply` never saw (bulk sync skips the non-finalized tier)
//! - fold → `compute` (Merkle hashing, every core), write → `blocking`
//!
//! Tiering: `docs/design/non-finalized-state.md`

use std::sync::Arc;

use incrementalmerkletree::Hashable;
use orchard::tree::MerkleHashOrchard;
use zaino_primitives::types::{
    Block, BlockRef, Height, PerPool, ShieldedPool, TreeSize, TreeSizes,
};
use zaino_sync::{IndexWriter, Offloaded};
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

/// Finalised batch, folded: `chunk` goes to disk; `durable` + the tier above `cut` (the batch's
/// last height, inclusive) replace the writer's state once it is there
struct Landing {
    chunk: NonFinalizedTrees,
    cut: Height,
    sizes: PoolSizes,
    durable: Carries,
}

impl Folds {
    /// Folds what the non-finalized tier lacks from `blocks`, takes the batch through `cut`
    /// (inclusive) as the chunk to write (the tier keeps it until the write lands)
    ///
    /// - `durable` = the carry at `cut`, from the chunk over what is already on disk (no read-back)
    fn land(
        &mut self,
        blocks: &[Arc<Block>],
        cut: Height,
        on_disk: &Snapshot,
    ) -> Result<Landing, IndexWriterError> {
        // non-finalized covers a prefix (applied); the rest never went through `apply` (bulk sync)
        let applied = blocks
            .iter()
            .take_while(|block| self.non_finalized.heights.contains_key(&block.header().height))
            .count();
        for block in &blocks[..applied] {
            let (height, hash) = (block.header().height, block.header().hash);
            let held = self.non_finalized.heights[&height].hash;
            assert_eq!(held, hash, "finalising {height} over another branch");
        }
        self.applied.fold(&blocks[applied..], &mut self.non_finalized)?;

        let sizes = self.non_finalized.heights[&cut].positions();
        let (chunk, _) = self.non_finalized.split(cut, sizes);
        let durable = Carries::seed(&chunk, on_disk, sizes)?;
        Ok(Landing { chunk, cut, sizes, durable })
    }
}

/// A finished `finalize` write: the store back, and the landing it wrote
pub struct Written {
    store: TreeStateStore,
    cut: Height,
    sizes: PoolSizes,
    durable: Carries,
}

/// - `durable` = the store as of the last landing (answered without the store while a write
///   has it)
pub struct TreeStateIndexWriter {
    folds: Offloaded<Folds>,
    store: Offloaded<TreeStateStore>,
    durable: Durable,
    /// Non-finalized tier as last published (what [`view`](IndexWriter::view) pins)
    view: NonFinalizedTrees,
}

/// What the store committed, pinned at a landing (`tip` = last committed block, inclusive;
/// `None` = nothing committed)
struct Durable {
    tip: Option<BlockRef>,
    snapshot: Arc<Snapshot>,
}

impl Durable {
    fn of(store: &TreeStateStore) -> Self {
        Self { tip: store.finalized_tip(), snapshot: store.snapshot() }
    }
}

impl TreeStateIndexWriter {
    /// Carries seeded from what `store` holds (the restart path: every boot)
    pub fn new(store: TreeStateStore) -> Result<Self, IndexWriterError> {
        let finalized = store.finalized_height();
        let snapshot = store.snapshot();
        let sizes = match finalized {
            None => PoolSizes::default(),
            Some(last) => {
                snapshot.height(last).expect("a store holds its committed tip record").positions()
            }
        };

        let view = NonFinalizedTrees::empty_at(finalized);
        let durable = Carries::seed(&view, &snapshot, sizes)?;

        Ok(Self {
            folds: Offloaded::new(Folds {
                applied: durable.clone(),
                durable,
                non_finalized: view.clone(),
            }),
            durable: Durable::of(&store),
            store: Offloaded::new(store),
            view,
        })
    }
}

impl IndexWriter for TreeStateIndexWriter {
    type Input = Block;
    type View = ReadView;
    type Error = IndexWriterError;
    type Done = Written;

    const NAME: &'static str = "tree_state";

    fn finalized_tip(&self) -> Option<BlockRef> {
        self.durable.tip
    }

    fn applied_height(&self) -> Option<Height> {
        self.view.tip
    }

    /// Non-finalized + committed, pinned at one consistent moment
    fn view(&self) -> ReadView {
        ReadView::new(self.view.clone(), Arc::clone(&self.durable.snapshot))
    }

    async fn apply(&mut self, block: &Arc<Block>) -> Result<(), IndexWriterError> {
        let expected = self.applied_height().map_or(Height::GENESIS, Height::next);
        let height = block.header().height;
        assert_eq!(height, expected, "tree_state: blocks must arrive contiguously");

        let block = Arc::clone(block);
        self.folds
            .compute(move |folds| {
                folds.applied.fold(std::slice::from_ref(&block), &mut folds.non_finalized)
            })
            .await?;
        self.view = self.folds.get().non_finalized.clone();
        Ok(())
    }

    async fn finalize(
        &mut self,
        blocks: &[Arc<Block>],
    ) -> Result<impl FnOnce() -> Result<Written, IndexWriterError> + Send + 'static, IndexWriterError>
    {
        let mut reached = self.finalized_height();
        for block in blocks {
            let height = block.header().height;
            let next = reached.map_or(Height::GENESIS, Height::next);
            assert_eq!(height, next, "tree_state: batch off the committed height");
            reached = Some(height);
        }
        let reached = reached.expect("tree_state: finalize with no blocks");

        let (blocks, on_disk) = (blocks.to_vec(), Arc::clone(&self.durable.snapshot));
        let Landing { chunk, cut, sizes, durable } =
            self.folds.compute(move |folds| folds.land(&blocks, reached, &on_disk)).await?;
        let mut store = self.store.lend();
        Ok(move || {
            store.write(&chunk)?;
            Ok(Written { store, cut, sizes, durable })
        })
    }

    async fn committed(
        &mut self,
        Written { store, cut, sizes, durable }: Written,
    ) -> Result<(), IndexWriterError> {
        self.durable = Durable::of(&store);
        self.store.restore(store);

        // written prefix leaves the non-finalized tier only once durable (split now, not at
        // `finalize`: blocks applied while the write was out stay above the cut)
        let folds = self.folds.get_mut();
        folds.durable = durable;
        folds.non_finalized = folds.non_finalized.split(cut, sizes).1;
        self.view = folds.non_finalized.clone();
        Ok(())
    }

    async fn reset(&mut self) -> Result<(), IndexWriterError> {
        // winning branch arrives as ordinary `apply` calls from here (= the restart path)
        let finalized = self.finalized_height();
        let folds = self.folds.get_mut();
        folds.applied = folds.durable.clone();
        folds.non_finalized = NonFinalizedTrees::empty_at(finalized);
        self.view = folds.non_finalized.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use incrementalmerkletree::{
        frontier::{CommitmentTree, Frontier},
        Hashable, Level,
    };
    use proptest::strategy::Strategy as _;
    use zaino_persistence::{fs::SimFs, pages::PageError, StoreError};
    use zaino_primitives::types::{
        BlockHeader, BlockRef, CommitmentTreeBytes, CompactCiphertext, OrchardAction, OrchardData,
        SaplingData, SaplingOutput, SubtreeRoot, Transaction, TransactionId, TreeRoot,
    };
    use zcash_primitives::merkle_tree::{read_commitment_tree, write_commitment_tree};
    use zcash_protocol::consensus::NetworkType;

    use crate::ServeError;

    const PATH: &str = "/ts";

    /// Canonical field element for every pool (small LE value < both moduli, unlike a
    /// repeated-byte filler)
    fn leaf(seed: u32) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&seed.to_le_bytes());
        bytes
    }

    /// One transaction per pool's commitment list
    fn block(height: u32, sapling: &[u32], orchard: &[u32], ironwood: &[u32]) -> Block {
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

        Block::new(
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
        )
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

    fn open(fs: &Arc<SimFs>) -> Result<TreeStateIndexWriter, IndexWriterError> {
        let store = TreeStateStore::open(fs.clone(), Path::new(PATH), NetworkType::Regtest)?;
        TreeStateIndexWriter::new(store)
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

    /// Three finalize batches crashed after every operation: each state reopens (proven) to an
    /// acknowledged or attempted batch, every committed height's trees equal the naive oracle's,
    /// and the next block folds on correctly
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_proven_prefix_that_keeps_folding() {
        let fs = SimFs::recording();
        let batches = [0..2usize, 2..3, 3..4];
        let chain: Vec<Arc<Block>> = (0u32..)
            .zip(&CHAIN)
            .map(|(height, (sapling, orchard, ironwood))| {
                Arc::new(block(height, sapling, orchard, ironwood))
            })
            .collect();
        {
            let mut writer = open(&fs).expect("open");
            for (acked, batch) in (1u64..).zip(batches.clone()) {
                zaino_sync::finalize_now(&mut writer, &chain[batch]).await.expect("finalize");
                fs.set_tag(acked);
            }
        }
        let committed_after = |acked: u64| match acked {
            0 => 0,
            1 => 2,
            2 => 3,
            _ => 4,
        };
        // the oracle's three trees after `CHAIN[..through]`
        let seen = |through: usize| {
            let mut pools = (Vec::new(), Vec::new(), Vec::new());
            for (sapling, orchard, ironwood) in &CHAIN[..through] {
                pools.0.extend_from_slice(sapling);
                pools.1.extend_from_slice(orchard);
                pools.2.extend_from_slice(ironwood);
            }
            (sapling_tree(&pools.0), orchard_tree(&pools.1), orchard_tree(&pools.2))
        };

        let states = fs.crash_states();
        assert!(states.len() > 20, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let mut writer = open(&state.fs).unwrap_or_else(|error| panic!("{label}: {error}"));
            let count = writer.finalized_height().map_or(0, |tip| u64::from(tip) + 1);
            let acked = [committed_after(state.tag), committed_after(state.tag + 1)];
            assert!(acked.contains(&count), "{label}: recovered {count}");
            if count > 0 {
                let tip = h(count as u32 - 1);
                let served = writer.view().treestate(tip).expect("committed tip");
                let trees = (served.sapling, served.orchard, served.ironwood);
                assert_eq!(trees, seen(count as usize), "{label}: trees at {tip}");
            }

            let next = count as usize;
            let finalized = zaino_sync::finalize_now(&mut writer, &chain[next..=next]).await;
            finalized.unwrap_or_else(|error| panic!("{label}: {error}"));
            let served = writer.view().treestate(h(next as u32)).expect("folded on");
            let trees = (served.sapling, served.orchard, served.ironwood);
            assert_eq!(trees, seen(next + 1), "{label}: folding on after recovery");
        }
    }

    /// Reopen: a committed node missing or rewritten = refused (never zero-filled: zero reads back
    /// as a real node), bytes past the commit (a crash before the manifest) = truncated
    #[tokio::test]
    async fn reopen_refuses_a_lost_or_torn_node_file_and_truncates_surplus() {
        let fs = SimFs::new();
        let chain: Vec<Arc<Block>> = (0u32..)
            .zip(&CHAIN)
            .map(|(height, (sapling, orchard, ironwood))| {
                Arc::new(block(height, sapling, orchard, ironwood))
            })
            .collect();
        let mut writer = open(&fs).expect("open");
        zaino_sync::finalize_now(&mut writer, &chain).await.expect("finalize");
        drop(writer);

        let leaves = Path::new(PATH).join("sapling").join("l00.dat");
        let committed = fs.contents(&leaves).expect("leaves");
        let surplus = [committed.as_slice(), &[0xff; 64]].concat();
        let reopen = |bytes: Vec<u8>| {
            fs.corrupt(&leaves, |file| *file = bytes);
            open(&fs)
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
        assert_eq!(resumed.finalized_height(), Some(h(4)), "resumes where it stopped");
        assert_eq!(fs.contents(&leaves).expect("leaves"), committed, "surplus truncated");
    }

    #[derive(Debug, Clone)]
    enum Step {
        Apply,
        Finalize(usize),
        Reset,
        Reopen,
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 64,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Random chains through random apply / finalize / reset / reopen sequences: after every
        /// step, every applied height serves exactly the naive frontier's three trees and nothing
        /// past it is served; a reset or a reopen lands applied on durable
        #[test]
        fn random_histories_serve_the_naive_trees_at_every_applied_height(
            counts in proptest::collection::vec((0usize..=3, 0usize..=3, 0usize..=3), 1..10),
            steps in proptest::collection::vec(
                proptest::prop_oneof![
                    3 => proptest::strategy::Just(Step::Apply),
                    2 => (1usize..=4).prop_map(Step::Finalize),
                    1 => proptest::strategy::Just(Step::Reset),
                    1 => proptest::strategy::Just(Step::Reopen),
                ],
                1..16,
            ),
        ) {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime")
                .block_on(random_history(counts, steps));
        }
    }

    /// Leaves unique per pool: sapling 1.., orchard 10_001.., ironwood 20_001..
    async fn random_history(counts: Vec<(usize, usize, usize)>, steps: Vec<Step>) {
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
        let chain: Vec<Arc<Block>> = (0u32..)
            .zip(&leaves)
            .map(|(height, (s, o, i))| Arc::new(block(height, s, o, i)))
            .collect();
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
        let mut writer = open(&fs).expect("open");
        for (at, step) in steps.iter().enumerate() {
            // block counts from genesis (= index of the next block to apply / finalize)
            let count = |tip: Option<Height>| tip.map_or(0, |h| u32::from(h) as usize + 1);
            let (applied, finalized) =
                (count(writer.applied_height()), count(writer.finalized_height()));
            match *step {
                Step::Apply if applied < chain.len() => {
                    writer.apply(&chain[applied]).await.expect("apply");
                }
                Step::Finalize(count) if finalized < chain.len() => {
                    let end = (finalized + count).min(chain.len());
                    let write = writer.finalize(&chain[finalized..end]).await.expect("finalize");
                    let done = write().expect("written");
                    // the follower applies while a write is out only above an applied batch (tip)
                    if end <= applied && applied < chain.len() {
                        writer.apply(&chain[applied]).await.expect("apply, write out");
                    }
                    writer.committed(done).await.expect("landed");
                }
                Step::Reset => writer.reset().await.expect("reset"),
                Step::Reopen => {
                    drop(writer);
                    writer = open(&fs).expect("reopen");
                }
                _ => {}
            }

            let view = writer.view();
            let (applied, finalized) = (writer.applied_height(), writer.finalized_height());
            assert_eq!(view.tip(), applied, "step {at} {step:?}");
            assert!(finalized <= applied, "step {at} {step:?}: durable past applied");
            if matches!(step, Step::Reset | Step::Reopen) {
                assert_eq!(applied, finalized, "step {at} {step:?}: nothing buffered survives");
            }
            for height in applied.into_iter().flat_map(|tip| Height::GENESIS.up_to(tip)) {
                let height = u32::from(height);
                let served = view.treestate(h(height));
                let served = served.unwrap_or_else(|error| panic!("step {at} {step:?}: {error}"));
                let trees = (served.sapling, served.orchard, served.ironwood);
                let expected = trees_through(height as usize);
                assert_eq!(trees, expected, "step {at} {step:?}: trees at {height}");
            }
            let past = applied.map_or(Height::GENESIS, Height::next);
            let absent = Err(ServeError::NotFound { height: past });
            assert_eq!(view.treestate(past), absent, "step {at} {step:?}: past applied");
        }
    }

    /// Gap = every later commitment silently mis-positioned → panic, never a skip
    #[tokio::test]
    #[should_panic(expected = "blocks must arrive contiguously")]
    async fn a_gap_in_the_block_stream_panics() {
        let mut writer = open(&SimFs::new()).expect("open");
        writer.apply(&Arc::new(block(0, &[1], &[101], &[201]))).await.expect("apply");
        let _ = writer.apply(&Arc::new(block(2, &[2], &[102], &[202]))).await;
    }

    /// Batch starting off the committed height = every record it writes shifted
    #[tokio::test]
    #[should_panic(expected = "batch off the committed height")]
    async fn finalize_off_the_committed_height_panics() {
        let mut writer = open(&SimFs::new()).expect("open");
        let _ = zaino_sync::finalize_now(&mut writer, &[Arc::new(block(1, &[1], &[101], &[201]))])
            .await;
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
        .map(|(height, leaves)| Arc::new(block(height, &[], &leaves, &[])))
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

        let mut writer = open(&fs).expect("open");
        for pending in &blocks {
            writer.apply(pending).await.expect("apply");
        }
        let buffered = writer.view();
        let roots = buffered.subtree_roots(ShieldedPool::Orchard, 0, 0).expect("roots");
        assert_eq!(roots, expected(&buffered), "non-finalized: two boundaries, completing blocks");

        zaino_sync::finalize_now(&mut writer, &blocks[..2]).await.expect("finalize");
        let split = writer.view();
        assert_eq!(split.subtree_roots(ShieldedPool::Orchard, 0, 0), Ok(roots.clone()), "split");

        // slot = subtree index → resume = a seek, bound = a prefix
        use ShieldedPool::{Orchard, Sapling};
        assert_eq!(split.subtree_roots(Orchard, 1, 0).expect("resume"), roots[1..]);
        assert_eq!(split.subtree_roots(Orchard, 0, 1).expect("bounded"), roots[..1]);
        assert_eq!(split.subtree_roots(Orchard, 2, 0).expect("probe"), Vec::new());
        assert_eq!(split.subtree_roots(Sapling, 0, 0).expect("untouched pool"), Vec::new());

        // reopen rebuilds the subtree cursor from the files; the next boundary follows it
        zaino_sync::finalize_now(&mut writer, &blocks[2..]).await.expect("finalize");
        drop(writer);
        let mut resumed = open(&fs).expect("reopen");
        let fifth = Arc::new(block(4, &[], &orchard(1 + 4 * HALF, 2 * HALF), &[]));
        zaino_sync::finalize_now(&mut resumed, std::slice::from_ref(&fifth))
            .await
            .expect("finalize");

        let view = resumed.view();
        let resumed_roots = view.subtree_roots(Orchard, 0, 0).expect("roots");
        let third = SubtreeRoot { root: tree_root(&view, 4), completing: completing(&fifth) };
        assert_eq!(resumed_roots, [roots, vec![third]].concat(), "earlier entries untouched");
    }

    /// Every pool served as its real tree's serialization, never an empty field (both clients map
    /// an absent field onto `CommitmentTree::empty()` silently)
    #[tokio::test]
    async fn ironwood_serves_a_real_tree_not_an_empty_field() {
        let mut writer = open(&SimFs::new()).expect("open");

        // ironwood-only block: sapling and orchard empty at this height
        let ironwood_only = [Arc::new(block(0, &[], &[], &[201, 202, 203]))];
        zaino_sync::finalize_now(&mut writer, &ironwood_only).await.expect("finalize");
        let served = writer.view().treestate(h(0)).expect("served");

        let parsed = read_commitment_tree::<MerkleHashOrchard, _, 32>(served.ironwood.as_bytes())
            .expect("the clients' own parser accepts it");
        assert_eq!(parsed.to_frontier().tree_size(), 3);

        // empty pool = the three-byte empty tree, not "" (active pool with no notes != a pool
        // below its activation)
        for empty in [served.sapling, served.orchard] {
            assert_eq!(empty.as_bytes(), [0u8, 0, 0]);
            let parsed = read_commitment_tree::<MerkleHashOrchard, _, 32>(empty.as_bytes());
            assert_eq!(parsed.expect("parses").to_frontier().tree_size(), 0);
        }
    }
}
