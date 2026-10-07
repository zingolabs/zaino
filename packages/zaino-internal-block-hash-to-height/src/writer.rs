//! block_hash index: one [`fold`] per block, kept by its own loop
//!
//! - storage tiers = `zaino_persistence::Tiered`

use std::num::NonZeroUsize;

use zaino_persistence::{MapRead, Store, Tiered, TieredView};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_sync::{Offloaded, Published, Step, Subscription, Weight};

use crate::{fold, BlockHashReader};

pub struct BlockHashIndexWriter<S: Store> {
    tiered: Offloaded<Tiered<S>>,
    published: Published<BlockHashReader<TieredView<S::View>>>,
}

impl<S: Store<View: MapRead>> BlockHashIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = final blocks per
    /// bulk commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        let tiered = Tiered::new(store, batch_bytes);
        let published = Published::new(BlockHashReader::new(tiered.view()), tiered.durable_tip());
        Self { tiered: Offloaded::new(tiered), published }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.tiered.get().durable_tip()
    }

    /// Reader, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<BlockHashReader<TieredView<S::View>>> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown` (a failure panics: its dropped queue fails the rest)
    pub async fn run(mut self, mut blocks: Subscription<Block>) {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized: true, data } => {
                    self.apply_final(height, &data).await
                }
                Step::Apply { finalized: false, data, .. } => self.apply_tip(&data).await,
                Step::Finalized { height } => self.finalize(height).await,
                Step::Reorg => self.reorg(),
                Step::Shutdown => return self.finalize_staged().await,
            }
        }
    }

    /// Final block (bulk sync): staged for the next batch commit
    async fn apply_final(&mut self, height: Height, block: &Block) {
        // replay for an index behind this one: already on disk
        if Some(height) <= self.durable_height() {
            return;
        }
        let changes = fold(block, self.tiered.get().schema().network);
        let full = self.tiered.get_mut().stage(changes, block.weight());
        self.published.merged(height);
        if full {
            self.finalize(height).await;
        }
    }

    async fn apply_tip(&mut self, block: &Block) {
        self.finalize_staged().await;
        let changes = fold(block, self.tiered.get().schema().network);
        self.tiered.get_mut().apply(changes);
        self.publish();
    }

    /// Back to the durable tip (the winning branch applies from there)
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
        self.published.durable(self.durable_height());
    }

    fn durable_height(&self) -> Option<Height> {
        self.durable_tip().map(|tip| tip.height)
    }

    fn publish(&self) {
        let tiered = self.tiered.get();
        self.published.view(BlockHashReader::new(tiered.view()), tiered.applied());
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use zaino_persistence::{fs::SimFs, DiskEngine, DiskStore, DiskView, PersistenceEngine};
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::BlockRef;
    use zaino_sync::BlockSink;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::schema;

    const NAME: &str = zaino_persistence::IndexKind::BlockHash.name();

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn open(fs: &Arc<SimFs>, batch: NonZeroUsize) -> BlockHashIndexWriter<DiskStore> {
        let store =
            DiskEngine::new(fs.clone()).open(Path::new("/bh"), &schema(NetworkType::Regtest));
        BlockHashIndexWriter::new(store.expect("open"), batch)
    }

    fn located(
        reader: &BlockHashReader<TieredView<DiskView>>,
        blocks: &[BlockRef],
    ) -> Vec<Option<Height>> {
        blocks.iter().map(|at| reader.height_of(&at.hash)).collect()
    }

    /// Steps sent as the producer sends them: bulk, the tip above it, a block finalized, a losing
    /// branch reorged away, a reopen; every published view locates exactly the winning chain's
    /// hashes, finalized or not
    #[tokio::test]
    async fn locates_both_tiers_across_a_finalize_a_reorg_and_a_reopen() {
        let fs = SimFs::new();
        let batch = NonZeroUsize::new(1 << 20).expect("non-zero");
        let index = open(&fs, batch);
        let served = index.published().served();
        let (mut durable, mut applied) =
            (index.published().subscribe_finalized(), index.published().subscribe_applied());
        let mut chain = Chain::new();
        let one = chain.mine(chain.genesis().hash);
        let two = chain.mine(one.hash);
        let (losing, winning) = (chain.mine(two.hash), chain.mine(two.hash));
        let mut sink = BlockSink::new("blocks");
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let blocks = sink.subscribe(NAME, queue);
        let running = tokio::spawn(index.run(blocks));
        let apply = |finalized, block: BlockRef| Step::Apply {
            height: block.height,
            finalized,
            data: Arc::new(chain.block(block.hash).clone()),
        };
        let within = std::time::Duration::from_secs(5);
        let through = |n| Some(h(n));

        for step in [apply(true, chain.genesis()), apply(true, one)] {
            sink.send(step).await;
        }
        for step in [apply(false, two), apply(false, losing)] {
            sink.send(step).await;
        }
        let tip = Some(losing);
        let tip = tokio::time::timeout(within, applied.wait_for(|at| *at == tip)).await;
        tip.expect("tip applied").expect("index alive");
        assert_eq!(*durable.borrow(), through(1), "bulk written before the tip applies on it");
        let both = located(&served.pin_any(), &[chain.genesis(), one, two, losing]);
        assert_eq!(both, [through(0), through(1), through(2), through(3)], "both tiers");

        sink.send(Step::Finalized { height: h(2) }).await;
        let finalized = tokio::time::timeout(within, durable.wait_for(|at| *at == through(2)));
        finalized.await.expect("2 written once final").expect("index alive");
        // 3 = a losing branch: dropped, the winner replayed from the first non-final height
        sink.send(Step::Reorg).await;
        sink.send(apply(false, winning)).await;
        sink.shutdown();
        running.await.expect("followed through Shutdown");
        let after = located(&served.pin_any(), &[two, winning, losing]);
        assert_eq!(after, [through(2), through(3), None], "losing branch gone");

        let index = open(&fs, batch);
        assert_eq!(index.durable_tip(), Some(two), "resumes at the durable tip");
        let reopened = located(&index.published().served().pin_any(), &[two, winning]);
        assert_eq!(reopened, [through(2), None], "non-finalized gone");
    }

    /// Five final blocks, each its own commit, crashed after every operation: each state reopens
    /// to an acknowledged or the attempted commit, locates every hash it holds and none past it,
    /// and commits the next block
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_committed_prefix_that_keeps_committing() {
        let fs = SimFs::recording();
        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 5);
        let blocks: Vec<Arc<Block>> = chain.path(tip.hash).into_iter().map(Arc::new).collect();
        let at =
            |block: &Block| BlockRef { hash: block.header().hash, height: block.header().height };
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        {
            let index = open(&fs, NonZeroUsize::MIN);
            let mut durable = index.published().subscribe_finalized();
            let mut sink = BlockSink::new("blocks");
            let blocks_queue = sink.subscribe(NAME, queue);
            let running = tokio::spawn(index.run(blocks_queue));
            for (acked, block) in (1u64..).zip(&blocks[..5]) {
                let (height, data) = (block.header().height, Arc::clone(block));
                sink.send(Step::Apply { height, finalized: true, data }).await;
                durable.wait_for(|at| *at == Some(height)).await.expect("index alive");
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("followed through Shutdown");
        }

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let index = open(&state.fs, NonZeroUsize::MIN);
            let count = index.durable_tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5) as usize);
            assert!(acked.contains(&count), "{label}: recovered {count} blocks");
            assert_eq!(index.durable_tip(), count.checked_sub(1).map(|last| at(&blocks[last])));
            let view = index.published().served().pin_any();
            let held: Vec<_> = blocks[..=count].iter().map(|block| at(block)).collect();
            let expected: Vec<_> = (0..count as u32).map(|n| Some(h(n))).chain([None]).collect();
            assert_eq!(located(&view, &held), expected, "{label}: held hashes, none past them");

            let served = index.published().served();
            let mut sink = BlockSink::new("blocks");
            let queue = sink.subscribe(NAME, queue);
            let running = tokio::spawn(index.run(queue));
            let next = Arc::clone(&blocks[count]);
            sink.send(Step::Apply { height: h(count as u32), finalized: true, data: next }).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let located_next = located(&served.pin_any(), &[at(&blocks[count])]);
            assert_eq!(located_next, [Some(h(count as u32))], "{label}: commits continue");
        }
    }

    /// Commit I/O failure = panic naming the index and its directory (never an `Err` to drain)
    #[tokio::test]
    async fn a_failed_commit_panics_naming_the_index_and_its_directory() {
        let fs = SimFs::new();
        let index = open(&fs, NonZeroUsize::MIN);
        fs.fail_from(fs.mutations());
        let mut sink = BlockSink::new("blocks");
        let blocks = sink.subscribe(NAME, NonZeroUsize::MIN);
        let running = tokio::spawn(index.run(blocks));
        let chain = Chain::new();
        let data = Arc::new(chain.block(chain.genesis().hash).clone());
        sink.send(Step::Apply { height: h(0), finalized: true, data }).await;

        let payload = running.await.expect_err("commit failure panics").into_panic();
        let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
        assert!(message.starts_with("block_hash index commit failed at /bh: "), "{message}");
        assert!(message.contains("sim: injected EIO"), "{message}");
    }
}
