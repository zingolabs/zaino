//! block_hash writer: the final stream → one [`fold`] per block → its store

use std::num::NonZeroUsize;

use zaino_persistence::{Changes, MapRead, Store};
use zaino_primitives::types::Block;
use zaino_sync::{Committer, IndexHandle, Subscription};

use crate::{
    by_hash::{encode_height, BY_HASH},
    BlockHashReader, HASH,
};

pub struct BlockHashIndexWriter<S: Store> {
    store: Committer<S>,
}

impl<S: Store<View: MapRead>> BlockHashIndexWriter<S> {
    /// Over `store` (opened with [`TABLES`](crate::TABLES)); `batch_bytes` = buffered bytes per
    /// bulk commit (one fsync)
    pub fn new(store: S, batch_bytes: NonZeroUsize) -> Self {
        Self { store: Committer::new(store, batch_bytes) }
    }

    /// For `Nfs::add`: committed view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.store.handle()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    pub async fn run(mut self, mut blocks: Subscription<Block>) {
        while let Some(run) = self.store.next(&mut blocks).await {
            let applied = move |store: &mut S| {
                run.apply(store, |store, block, out| {
                    fold(&BlockHashReader::new(store.staged()), block, out)
                });
            };
            self.store.compute(applied).await;
        }
    }
}

/// `block` onto `parent`: its one `by_hash` row (its own header only, nothing read)
pub fn fold<V: MapRead>(parent: &BlockHashReader<V>, block: &Block, out: &mut Changes) {
    out.assert_next(parent.view().tip(), block);
    let header = block.header();
    out.map(BY_HASH).insert(&<[u8; HASH]>::from(header.hash), &encode_height(header.height));
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        path::Path,
        sync::Arc,
    };

    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, DiskView, IndexKind, PersistenceEngine, Schema, View,
    };
    use zaino_primitives::testing::{h, MockChain};
    use zaino_primitives::types::{BlockHash, Height};
    use zaino_sync::{IndexerDataSink, Step};
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{FORMAT, TABLES};

    const NAME: &str = IndexKind::BlockHash.name();
    const QUEUE: NonZeroUsize = NonZeroUsize::new(1 << 20).expect("non-zero");
    const SCHEMA: Schema = Schema::new(IndexKind::BlockHash, FORMAT, NetworkType::Regtest, TABLES);

    fn open(fs: &Arc<SimFs>) -> DiskStore {
        DiskEngine::new(fs.clone()).open(Path::new("/bh"), &SCHEMA).expect("open")
    }

    /// Writer on `fs`, its final stream and handle
    fn start(
        store: DiskStore,
        batch: NonZeroUsize,
    ) -> (IndexerDataSink<Block>, IndexHandle<DiskView>, tokio::task::JoinHandle<()>) {
        let writer = BlockHashIndexWriter::new(store, batch);
        let handle = writer.handle();
        let mut sink = IndexerDataSink::new("final");
        let running = tokio::spawn(writer.run(sink.subscribe(NAME, QUEUE)));
        (sink, handle, running)
    }

    fn step(block: &Arc<Block>) -> Step<Block> {
        Step::Apply { height: block.header().height, data: Arc::clone(block) }
    }

    /// Until `handle`'s durable tip = `height`
    async fn durable_at(handle: &mut IndexHandle<DiskView>, height: u32) {
        while handle.tip().map(|tip| u32::from(tip.height)) != Some(height) {
            assert!(handle.changed().await, "writer alive");
        }
    }

    /// Each block → one golden row at its own tip; the reader locates every folded hash and
    /// nothing else; a delta opened for another block or a block off the parent tip panics
    #[test]
    fn each_block_folds_to_its_golden_row_and_the_reader_locates_exactly_those() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(2);
        let blocks = chain.blocks(tip);
        let mut store = open(&SimFs::new());

        for (height, block) in (0u8..).zip(&blocks) {
            let hash = <[u8; HASH]>::from(block.header().hash);
            let mut changes = store.changes(block.at());
            fold(&BlockHashReader::new(store.staged()), block, &mut changes);
            let rows: Vec<(&[u8], &[u8])> = changes.inserts(BY_HASH).collect();
            assert_eq!(rows, [(&hash[..], &[0, 0, 0, height][..])], "block {height}: hash → BE");
            store.apply(changes);
        }

        let reader = BlockHashReader::new(store.staged());
        let located: Vec<_> =
            blocks.iter().map(|block| reader.height_of(&block.header().hash)).collect();
        let expected: Vec<_> = (0..3u32).map(|n| Some(Height::try_from(n).expect("h"))).collect();
        assert_eq!(located, expected);
        assert_eq!(reader.height_of(&BlockHash::from([0xee; HASH])), None, "never folded");

        let next = chain.mine_empty(2);
        let (three, four) = (chain.block(chain.at(h(3)).hash), chain.block(next.hash));
        for (case, opened_for, block, expected) in [
            ("another block", three, four, "changes opened for another block"),
            ("a gap", four, four, "does not extend the parent tip"),
        ] {
            let mut changes = store.changes(opened_for.at());
            let folded = catch_unwind(AssertUnwindSafe(|| fold(&reader, block, &mut changes)));
            let payload = folded.expect_err(case);
            let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
            let named = message.starts_with("block_hash: ") && message.contains(expected);
            assert!(named, "{case}: {message}");
        }
    }

    /// 0..=3 (committed once the stream idles), then a restart resending 2 and 3 (held: skipped)
    /// before 4; every hash located at its height after a reopen, a never-sent one nowhere
    #[tokio::test(start_paused = true)]
    async fn every_block_locates_its_hash_and_a_restart_skips_what_it_holds() {
        let fs = SimFs::new();
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(4);
        let blocks = chain.blocks(tip);
        let sibling = chain.fork(h(3)).mine_empty(1).tip();

        let (sink, mut handle, running) = start(open(&fs), QUEUE);
        for block in &blocks[..=3] {
            sink.send(step(block)).await;
        }
        durable_at(&mut handle, 3).await;
        sink.shutdown();
        running.await.expect("stops at Shutdown");

        let (sink, handle, running) = start(open(&fs), QUEUE);
        let resumed = handle.tip().map(|tip| u32::from(tip.height));
        assert_eq!(resumed, Some(3), "resumes at the committed tip");
        for block in &blocks[2..=4] {
            sink.send(step(block)).await;
        }
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
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(5);
        let blocks = chain.blocks(tip);
        let located = |view: DiskView, blocks: &[Arc<Block>]| -> Vec<Option<Height>> {
            let reader = BlockHashReader::new(view);
            blocks.iter().map(|block| reader.height_of(&block.header().hash)).collect()
        };
        {
            let (sink, mut handle, running) = start(open(&fs), NonZeroUsize::MIN);
            for (acked, block) in (1u64..).zip(&blocks[..5]) {
                sink.send(step(block)).await;
                durable_at(&mut handle, u32::from(block.header().height)).await;
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

            let (sink, handle, running) = start(store, NonZeroUsize::MIN);
            sink.send(step(&blocks[count])).await;
            sink.shutdown();
            running.await.unwrap_or_else(|error| panic!("{label}: {error}"));
            let next = located(handle.view(), &blocks[count..=count]);
            assert_eq!(next, [Height::try_from(count as u32).ok()], "{label}: commits continue");
        }
    }

    /// Commit I/O failure = panic naming the index and its directory (never an `Err` to drain)
    #[tokio::test]
    async fn a_failed_commit_panics_naming_the_index_and_its_directory() {
        let fs = SimFs::new();
        let (sink, _handle, running) = start(open(&fs), NonZeroUsize::MIN);
        fs.fail_from(fs.mutations());
        let chain = MockChain::regtest();
        sink.send(step(chain.block(chain.genesis().hash))).await;
        sink.shutdown();

        let payload = running.await.expect_err("commit failure panics").into_panic();
        let message = payload.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
        assert!(message.starts_with("block_hash index commit failed at /bh: "), "{message}");
        assert!(message.contains("sim: injected EIO"), "{message}");
    }
}
