//! [`Committer`]: one index writer's store + its [`IndexHandle`] (`data-sink.md`)
//!
//! - Commit = batch full, or the stream quiet for [`IDLE`]
//! - Store on a pool for each hop ([`Offloaded`]), never worked on the async loop

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use tokio::sync::watch;
use zaino_persistence::{Changes, Store, View};
use zaino_primitives::types::{Block, Height};

use crate::{IndexHandle, Offloaded, Step, Subscription};

/// Stream quiet this long = commit what is buffered (bulk arrivals never pause this long)
const IDLE: Duration = Duration::from_secs(1);

pub struct Committer<S: Store> {
    store: Offloaded<S>,
    committed: watch::Sender<S::View>,
    batch: NonZeroUsize,
}

/// One run of final blocks for one index, in height order (held ones included: a restart resends)
pub struct Run {
    pub blocks: Vec<(Height, Arc<Block>)>,
}

impl Run {
    /// Each block `store` lacks: `fold` into the delta opened for it, then applied
    pub fn apply<S: Store>(&self, store: &mut S, mut fold: impl FnMut(&S, &Block, &mut Changes)) {
        for (height, block) in &self.blocks {
            if !held(store, *height) {
                let mut changes = store.changes(block.at());
                fold(store, block, &mut changes);
                store.apply(changes);
            }
        }
    }

    /// [`apply`](Self::apply) with every block `store` lacks folded as one batch (one delta per
    /// block, parent = `store.staged()` before the batch)
    pub fn apply_batch<S: Store, T>(
        &self,
        store: &mut S,
        fold: impl FnOnce(&S, &[&Block], &mut [Changes]) -> T,
    ) -> T {
        let fresh = self.blocks.iter().filter(|(height, _)| !held(store, *height));
        let fresh: Vec<&Block> = fresh.map(|(_, block)| &**block).collect();
        let mut out: Vec<Changes> = fresh.iter().map(|block| store.changes(block.at())).collect();
        let answer = fold(store, &fresh, &mut out);
        for changes in out {
            store.apply(changes);
        }
        answer
    }
}

/// `height` at or below `store`'s staged tip (a restart resends from the lowest durable tip)
pub fn held<S: Store>(store: &S, height: Height) -> bool {
    Some(height) <= store.staged().tip().map(|tip| tip.height)
}

impl<S: Store> Committer<S> {
    /// `batch` = buffered bytes per commit, and one run's stream bytes
    pub fn new(store: S, batch: NonZeroUsize) -> Self {
        let committed = watch::Sender::new(store.view());
        Self { store: Offloaded::new(store), committed, batch }
    }

    /// For the NFS + snapshots: the committed view after every commit (tip = durable tip)
    pub fn handle(&self) -> IndexHandle<S::View> {
        IndexHandle::new(self.committed.subscribe())
    }

    /// Next run off `blocks` (queued steps to `batch` bytes); `None` = `Shutdown`, all committed
    ///
    /// - panics: a gap above the staged tip
    pub async fn next(&mut self, blocks: &mut Subscription<Block>) -> Option<Run> {
        if self.store.get().buffered_bytes() >= self.batch.get() {
            self.commit().await;
        }
        let step = match tokio::time::timeout(IDLE, blocks.next()).await {
            Ok(step) => step,
            Err(_quiet) => {
                self.commit().await;
                blocks.next().await
            }
        };
        let Step::Apply { height, data } = step else {
            self.commit().await;
            return None;
        };
        let run = Run { blocks: blocks.run((height, data), self.batch) };
        let staged = self.store.get().staged().tip();
        let next = staged.map_or(Height::GENESIS, |tip| tip.height.next());
        let name = self.store.get().schema().kind.name();
        assert!(height <= next, "{name}: final stream gap: {height} sent, {next} next");
        Some(run)
    }

    /// `f` on the CPU pool with the store (a run's folds and applies)
    pub async fn compute<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut S) -> T + Send + 'static,
    ) -> T {
        self.store.compute(f).await
    }

    /// Buffered → disk (one fsync), then the new view out; nothing buffered = nothing done
    ///
    /// - failed commit = panic naming the index and its directory (store poisoned)
    async fn commit(&mut self) {
        let store = self.store.get();
        if store.staged().tip() == store.view().tip() {
            return;
        }
        self.store
            .blocking(|store| {
                if let Err(error) = store.commit() {
                    error.commit_failed(store.schema().kind.name(), store.path());
                }
            })
            .await;
        self.committed.send_replace(self.store.get().view());
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::time::Instant;
    use zaino_persistence::{
        fs::SimFs, DiskEngine, DiskStore, IndexKind, PersistenceEngine, Schema, SequenceRead,
        SequenceTable, Tables, Width,
    };
    use zaino_primitives::testing::MockChain;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::IndexerDataSink;

    const ROWS: SequenceTable = SequenceTable::new(0, "rows", Width::fixed(4));
    const SCHEMA: Schema =
        Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, Tables::new(&[ROWS], &[]));

    /// Toy index (one 4-byte row per block, its height) behind a `Committer`, as a writer runs it,
    /// folding block by block (`Run::apply`) and as one batch (`Run::apply_batch`)
    ///
    /// - Batch 8 bytes = two rows; paused clock: a commit's time shows its trigger (idle = +1 s)
    /// - 0..=3 queued at once (batch, twice), 4 alone (idle); reopened: 3 + 4 resent (held:
    ///   skipped), 5 new; a gap panics the writer
    #[tokio::test(start_paused = true)]
    async fn a_writer_folds_each_block_once_skips_held_and_commits_on_batch_or_idle() {
        for batched in [false, true] {
            let mut chain = MockChain::regtest();
            let tip = chain.mine_empty(5);
            let blocks = chain.blocks(tip);
            let step = |at: u32| {
                let block = Arc::clone(&blocks[at as usize]);
                Step::Apply { height: block.header().height, data: block }
            };
            let row = |block: &Block| u32::from(block.header().height).to_le_bytes();
            let engine = DiskEngine::new(SimFs::new());
            let open = || engine.open(Path::new("/toy"), &SCHEMA).expect("open");
            let start = |store: DiskStore| {
                let mut committer = Committer::new(store, NonZeroUsize::new(8).expect("nonzero"));
                let handle = committer.handle();
                let mut sink = IndexerDataSink::new("final");
                let mut blocks = sink.subscribe("toy", NonZeroUsize::MAX);
                let writer = tokio::spawn(async move {
                    while let Some(run) = committer.next(&mut blocks).await {
                        let applied = move |store: &mut DiskStore| match batched {
                            false => run.apply(store, |_, block, out| {
                                out.sequence(ROWS).append(&row(block));
                            }),
                            true => run.apply_batch(store, |_, blocks, out| {
                                for (block, out) in blocks.iter().zip(out) {
                                    out.sequence(ROWS).append(&row(block));
                                }
                            }),
                        };
                        committer.compute(applied).await;
                    }
                });
                (sink, handle, writer)
            };
            let commit = async |handle: &mut IndexHandle<_>, since: Instant| {
                assert!(handle.changed().await, "writer alive");
                (handle.tip().map(|tip| u32::from(tip.height)), since.elapsed())
            };

            let (sink, mut handle, writer) = start(open());
            let at = Instant::now();
            for height in 0..=3 {
                sink.send(step(height)).await;
            }
            let (now, idle) = (Duration::ZERO, Duration::from_secs(1));
            assert_eq!(commit(&mut handle, at).await, (Some(1), now), "{batched}: batch");
            assert_eq!(commit(&mut handle, at).await, (Some(3), now), "{batched}: batch again");
            sink.send(step(4)).await;
            let at = Instant::now();
            assert_eq!(commit(&mut handle, at).await, (Some(4), idle), "{batched}: idle");
            sink.shutdown();
            writer.await.expect("writer stops at Shutdown");

            let (sink, handle, writer) = start(open());
            assert_eq!(handle.tip().map(|tip| u32::from(tip.height)), Some(4), "reopened");
            for height in [3, 4, 5] {
                sink.send(step(height)).await;
            }
            sink.shutdown();
            writer.await.expect("writer stops at Shutdown");
            let view = open().view();
            let rows = view.sequence(ROWS);
            let rows = rows.records(0..rows.count());
            let rows: Vec<u32> =
                rows.iter().map(|row| u32::from_le_bytes(row[..].try_into().expect("4"))).collect();
            assert_eq!(rows, [0, 1, 2, 3, 4, 5], "{batched}: each row once");

            let (sink, _handle, writer) = start(
                DiskEngine::new(SimFs::new()).open(Path::new("/toy"), &SCHEMA).expect("open"),
            );
            sink.send(step(1)).await;
            sink.shutdown();
            let panic = writer.await.expect_err("gap").into_panic();
            let got = panic.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(got.contains("block_hash: final stream gap: 1 sent, 0 next"), "{got}");
        }
    }
}
