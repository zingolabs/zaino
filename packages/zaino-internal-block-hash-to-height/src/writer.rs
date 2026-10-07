//! block_hash writer: the final stream → one [`fold`] row per block → its store

use std::num::NonZeroUsize;

use tokio::sync::watch;
use zaino_persistence::{MapRead, Store};
use zaino_sync::{Committer, Final, Subscription};

use crate::fold;

pub struct BlockHashIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: MapRead>> BlockHashIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema)); `batch_bytes` = buffered bytes per
    /// bulk commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
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
                let network = store.schema().network;
                run.apply(store, |_, block| fold(block, network));
            };
            self.store.compute(applied).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, View,
    };
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{Block, Height};
    use zaino_sync::{Folds, IndexerDataSink, Step};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{schema, BlockHashReader};

    const NAME: &str = IndexKind::BlockHash.name();
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        let store =
            DiskEngine::new(fs.clone()).open(Path::new("/bh"), &schema(NetworkType::Regtest));
        store.expect("open")
    }

    /// The writer on `fs`, its final stream and committed view
    fn start(
        store: DiskStore,
        batch: NonZeroUsize,
    ) -> (IndexerDataSink<Final>, watch::Receiver<DiskView>, tokio::task::JoinHandle<()>) {
        let writer = BlockHashIndexWriter::new(store, batch);
        let committed = writer.committed();
        let mut sink = IndexerDataSink::new("final");
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE)));
        (sink, committed, running)
    }

    fn step(block: &Block, folds: Option<Folds>) -> Step<Final> {
        let (height, block) = (block.header().height, Arc::new(block.clone()));
        Step::Apply { height, data: Arc::new(Final { block, folds: folds.map(Arc::new) }) }
    }

    /// Bulk 0..=2 unfolded (committed once the stream idles), folded 3 (committed at once), then
    /// a restart resending 2 and 3 (held: skipped) before folded 4; every hash located at its
    /// height after a reopen, a never-sent one nowhere
    #[tokio::test(start_paused = true)]
    async fn unfolded_and_folded_steps_locate_every_hash_and_a_restart_skips_what_it_holds() {
        let fs = SimFs::new();
        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 4);
        let blocks = chain.path(tip.hash);
        let sibling = chain.mine(blocks[3].header().hash);
        let folded = |block: &Block| {
            let mut folds = Folds::default();
            folds.insert(IndexKind::BlockHash, fold(block, NetworkType::Regtest));
            Some(folds)
        };
        let tip_of = |view: &DiskView| view.tip().map(|tip| u32::from(tip.height));

        let (sink, mut committed, running) = start(open(&fs), QUEUE);
        for block in &blocks[..=2] {
            sink.send(step(block, None)).await;
        }
        committed.wait_for(|view| tip_of(view) == Some(2)).await.expect("writer alive");
        sink.send(step(&blocks[3], folded(&blocks[3]))).await;
        committed.wait_for(|view| tip_of(view) == Some(3)).await.expect("writer alive");
        sink.shutdown();
        running.await.expect("stops at Shutdown");

        let (sink, committed, running) = start(open(&fs), QUEUE);
        assert_eq!(tip_of(&committed.borrow()), Some(3), "resumes at the committed tip");
        sink.send(step(&blocks[2], None)).await;
        sink.send(step(&blocks[3], None)).await;
        sink.send(step(&blocks[4], folded(&blocks[4]))).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");

        let reader = BlockHashReader::new(open(&fs).view());
        let located = blocks.iter().map(|block| reader.height_of(&block.header().hash));
        let located: Vec<_> = located.chain([reader.height_of(&sibling.hash)]).collect();
        let heights = (0..=4u32).map(|n| Some(Height::try_from(n).expect("h")));
        assert_eq!(located, heights.chain([None]).collect::<Vec<_>>());
    }

    /// Five final blocks, each its own commit, crashed after every operation: each state reopens
    /// to an acknowledged or the attempted commit, locates every hash it holds and none past it,
    /// and commits the next block
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_committed_prefix_that_keeps_committing() {
        let fs = SimFs::recording();
        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 5);
        let blocks = chain.path(tip.hash);
        let located = |view: DiskView, blocks: &[Block]| -> Vec<Option<Height>> {
            let reader = BlockHashReader::new(view);
            blocks.iter().map(|block| reader.height_of(&block.header().hash)).collect()
        };
        {
            let (sink, mut committed, running) = start(open(&fs), NonZeroUsize::MIN);
            for (acked, block) in (1u64..).zip(&blocks[..5]) {
                sink.send(step(block, None)).await;
                let height = Some(block.header().height);
                let landed = committed.wait_for(|view| view.tip().map(|tip| tip.height) == height);
                landed.await.expect("writer alive");
                fs.set_tag(acked);
            }
            sink.shutdown();
            running.await.expect("stops at Shutdown");
        }

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let label = &state.label;
            let store = open(&state.fs);
            let count = store.view().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5) as usize);
            assert!(acked.contains(&count), "{label}: recovered {count} blocks");
            let expected: Vec<_> = (0..count as u32).map(|n| Height::try_from(n).ok()).collect();
            let expected = [expected, vec![None]].concat();
            assert_eq!(located(store.view(), &blocks[..=count]), expected, "{label}: held only");

            let (sink, committed, running) = start(store, NonZeroUsize::MIN);
            sink.send(step(&blocks[count], None)).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let next = located(committed.borrow().clone(), &blocks[count..=count]);
            assert_eq!(next, [Height::try_from(count as u32).ok()], "{label}: commits continue");
        }
    }

    /// Commit I/O failure = panic naming the index and its directory (never an `Err` to drain)
    #[tokio::test]
    async fn a_failed_commit_panics_naming_the_index_and_its_directory() {
        let fs = SimFs::new();
        let (sink, _committed, running) = start(open(&fs), NonZeroUsize::MIN);
        fs.fail_from(fs.mutations());
        let chain = Chain::new();
        sink.send(step(chain.block(chain.genesis().hash), None)).await;
        sink.shutdown();

        let payload = running.await.expect_err("commit failure panics").into_panic();
        let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
        assert!(message.starts_with("block_hash index commit failed at /bh: "), "{message}");
        assert!(message.contains("sim: injected EIO"), "{message}");
    }
}
