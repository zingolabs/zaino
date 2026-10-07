//! [`Committer`]: one index writer's store + the committed-view watch the NFS reads (`nfs.md` §5)
//!
//! - Commit = batch full, after each folded (tip) run, or the stream quiet for [`IDLE`]
//! - Idle commit = lockstep's need (the NFS folds its first tip block once every index holds all
//!   it sent)
//! - Store on a pool for each hop ([`Offloaded`]), never worked on the async loop

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use tokio::sync::watch;
use zaino_persistence::{Changes, Store, View};
use zaino_primitives::types::{Block, Height};

use crate::{Applied, Final, Folds, Offloaded, Step, Subscription};

/// Stream quiet this long = commit what is buffered (bulk arrivals never pause this long)
const IDLE: Duration = Duration::from_secs(1);

/// `folded` = a folded step seen: every later one folded too, each run committed at once (tip)
pub struct Committer<S: Store> {
    store: Offloaded<S>,
    committed: watch::Sender<S::View>,
    batch: NonZeroUsize,
    folded: bool,
}

/// One run of final steps, in height order: unfolded first (the writer folds), then folded
pub struct Run {
    pub unfolded: Vec<(Height, Arc<Block>)>,
    pub(crate) folded: Vec<(Height, Arc<Folds>)>,
}

impl Run {
    /// Panics: a gap, or unfolded after folded (the NFS never sends one over a folded parent)
    fn new(steps: Vec<Applied<Final>>, folded: bool) -> Self {
        let mut run = Self { unfolded: Vec::new(), folded: Vec::new() };
        let mut next = None;
        for (height, data) in steps {
            assert!(next.is_none_or(|next| next == height), "final stream gap at {height}");
            next = Some(height.next());
            match &data.folds {
                Some(folds) => run.folded.push((height, Arc::clone(folds))),
                None => {
                    let after = folded || !run.folded.is_empty();
                    assert!(!after, "unfolded {height} after a folded step");
                    run.unfolded.push((height, Arc::clone(&data.block)));
                }
            }
        }
        run
    }

    fn first(&self) -> Height {
        let first = self.unfolded.first().map(|(height, _)| height);
        *first.or(self.folded.first().map(|(height, _)| height)).expect("a run holds a step")
    }

    /// Every step `store` lacks, in order: each unfolded one folded by `fold` into the delta opened
    /// for it (parent = `store.staged()`, earlier blocks applied), then folded ones as sent
    pub fn apply<S: Store>(&self, store: &mut S, mut fold: impl FnMut(&S, &Block, &mut Changes)) {
        for (height, block) in &self.unfolded {
            if !held(store, *height) {
                let mut changes = store.changes(block.at());
                fold(store, block, &mut changes);
                store.apply(changes);
            }
        }
        self.apply_folded(store);
    }

    /// [`apply`](Self::apply) with the unfolded steps `store` lacks folded as one batch: `fold`
    /// fills one delta per block (parent = `store.staged()` before the batch); its answer returned
    pub fn apply_batch<S: Store, T>(
        &self,
        store: &mut S,
        fold: impl FnOnce(&S, &[&Block], &mut [Changes]) -> T,
    ) -> T {
        let fresh = self.unfolded.iter().filter(|(height, _)| !held(store, *height));
        let fresh: Vec<&Block> = fresh.map(|(_, block)| &**block).collect();
        let mut out: Vec<Changes> = fresh.iter().map(|block| store.changes(block.at())).collect();
        let answer = fold(store, &fresh, &mut out);
        for changes in out {
            store.apply(changes);
        }
        self.apply_folded(store);
        answer
    }

    /// Each folded step `store` does not hold yet, applied (its own index's `Changes`)
    fn apply_folded<S: Store>(&self, store: &mut S) {
        let kind = store.schema().kind;
        for (height, folds) in &self.folded {
            if !held(store, *height) {
                let changes = folds.get(kind);
                let changes = changes.unwrap_or_else(|| panic!("{}: not folded", kind.name()));
                store.apply(changes.clone());
            }
        }
    }
}

/// `height` at or below `store`'s staged tip (a restart resends from the lowest durable tip)
pub fn held<S: Store>(store: &S, height: Height) -> bool {
    Some(height) <= store.staged().tip().map(|tip| tip.height)
}

impl<S: Store> Committer<S> {
    /// `batch` = buffered bytes per bulk commit, and one run's stream bytes
    pub fn new(store: S, batch: NonZeroUsize) -> Self {
        let committed = watch::Sender::new(store.view());
        Self { store: Offloaded::new(store), committed, batch, folded: false }
    }

    /// For `Nfs::subscribe`: the store's committed view after every commit (tip = durable tip)
    pub fn committed(&self) -> watch::Receiver<S::View> {
        self.committed.subscribe()
    }

    /// Next run off `blocks` (queued steps to `batch` bytes); `None` = `Shutdown`, all committed
    ///
    /// - panics: a gap above the staged tip
    pub async fn next(&mut self, blocks: &mut Subscription<Final>) -> Option<Run> {
        if self.folded || self.store.get().buffered_bytes() >= self.batch.get() {
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
        let run = Run::new(blocks.run((height, data), self.batch), self.folded);
        let staged = self.store.get().staged().tip();
        let next = staged.map_or(Height::GENESIS, |tip| tip.height.next());
        let name = self.store.get().schema().kind.name();
        assert!(run.first() <= next, "{name}: final stream gap: {} sent, {next} next", run.first());
        self.folded |= !run.folded.is_empty();
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
        fs::SimFs, DiskEngine, DiskStore, DiskView, IndexKind, Layer, PersistenceEngine, Schema,
        SequenceRead, SequenceTable, Tables, Width,
    };
    use zaino_primitives::testing::MockChain;
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::IndexerDataSink;

    const ROWS: SequenceTable = SequenceTable::new(0, "rows", Width::fixed(4));
    const SCHEMA: Schema =
        Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, Tables::new(&[ROWS], &[]));
    /// Folded rows carry this bit: which path wrote a row stays visible
    const FOLDED: u32 = 1 << 31;

    /// Toy index (one 4-byte row per block, its height) behind a `Committer`, as a writer runs it,
    /// folding block by block (`Run::apply`) and as one batch (`Run::apply_batch`)
    ///
    /// - Batch 8 bytes = two rows; paused clock: a commit's time shows its trigger (idle = +1 s)
    /// - Run 0: unfolded 0..=3 queued at once (batch, twice), unfolded 4 (idle), folded 5 (tip)
    /// - Run 1, reopened: unfolded 4 + folded 5 resent (held: skipped, 5's resend marked), folded 6
    /// - A gap or an unfolded step after a folded one panics the writer
    #[tokio::test(start_paused = true)]
    async fn a_writer_folds_unfolded_steps_applies_folded_ones_skips_held_and_commits_on_cue() {
        for batched in [false, true] {
            a_writer_run(batched).await;
        }
    }

    async fn a_writer_run(batched: bool) {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(6);
        let blocks = chain.blocks(tip);
        let bytes =
            |block: &Block, mark: u32| (u32::from(block.header().height) | mark).to_le_bytes();
        let row = |height: Height, mark: u32| {
            let block = &blocks[u32::from(height) as usize];
            let mut changes = Layer::empty(&SCHEMA).changes(block.at());
            changes.sequence(ROWS).append(&bytes(block, mark));
            changes
        };
        let unfolded = |at: u32| {
            let block = Arc::clone(&blocks[at as usize]);
            Step::Apply {
                height: block.header().height,
                data: Arc::new(Final { block, folds: None }),
            }
        };
        let folded = |at: u32, mark: u32| {
            let block = Arc::clone(&blocks[at as usize]);
            let mut folds = Folds::default();
            folds.insert(IndexKind::BlockHash, row(block.header().height, mark));
            let data = Final { block: Arc::clone(&block), folds: Some(Arc::new(folds)) };
            Step::Apply { height: block.header().height, data: Arc::new(data) }
        };
        let fs = SimFs::new();
        let engine = DiskEngine::new(fs.clone());
        let open = || engine.open(Path::new("/toy"), &SCHEMA).expect("open");
        let start = |store: DiskStore| {
            let mut committer = Committer::new(store, NonZeroUsize::new(8).expect("nonzero"));
            let committed = committer.committed();
            let mut sink = IndexerDataSink::new("final");
            let mut blocks = sink.subscribe("toy", NonZeroUsize::MAX);
            let writer = tokio::spawn(async move {
                while let Some(run) = committer.next(&mut blocks).await {
                    let applied = move |store: &mut DiskStore| match batched {
                        false => run.apply(store, |_, block, out| {
                            out.sequence(ROWS).append(&bytes(block, 0));
                        }),
                        true => run.apply_batch(store, |_, blocks, out| {
                            for (block, out) in blocks.iter().zip(out) {
                                out.sequence(ROWS).append(&bytes(block, 0));
                            }
                        }),
                    };
                    committer.compute(applied).await;
                }
            });
            (sink, committed, writer)
        };
        let commit = async |committed: &mut watch::Receiver<DiskView>, since: Instant| {
            committed.changed().await.expect("writer alive");
            let tip = committed.borrow_and_update().tip().map(|tip| u32::from(tip.height));
            (tip, since.elapsed())
        };
        let rows = || {
            let view = open().view();
            let rows = view.sequence(ROWS);
            let rows = rows.records(0..rows.count());
            let rows = rows.iter().map(|row| u32::from_le_bytes(row[..].try_into().expect("4")));
            rows.collect::<Vec<u32>>()
        };

        let (sink, mut committed, writer) = start(open());
        let at = Instant::now();
        for height in 0..=3 {
            sink.send(unfolded(height)).await;
        }
        let (now, idle) = (Duration::ZERO, Duration::from_secs(1));
        assert_eq!(commit(&mut committed, at).await, (Some(1), now), "{batched}: 8 bytes: batch");
        assert_eq!(commit(&mut committed, at).await, (Some(3), now), "{batched}: 8 bytes again");
        sink.send(unfolded(4)).await;
        let at = Instant::now();
        let landed = commit(&mut committed, at).await;
        assert_eq!(landed, (Some(4), idle), "{batched}: 4 of 8 bytes: idle");
        let at = Instant::now();
        sink.send(folded(5, FOLDED)).await;
        let landed = commit(&mut committed, at).await;
        assert_eq!(landed, (Some(5), now), "{batched}: folded: at once");
        sink.shutdown();
        writer.await.expect("writer stops at Shutdown");

        let (sink, committed, writer) = start(open());
        let reopened = committed.borrow().tip().map(|tip| u32::from(tip.height));
        assert_eq!(reopened, Some(5), "{batched}: reopened");
        sink.send(unfolded(4)).await;
        sink.send(folded(5, FOLDED | 0x4000_0000)).await;
        sink.send(folded(6, FOLDED)).await;
        sink.shutdown();
        writer.await.expect("writer stops at Shutdown");
        let expected = [0, 1, 2, 3, 4, 5 | FOLDED, 6 | FOLDED];
        assert_eq!(rows(), expected, "{batched}: each row once, unfolded by the writer");

        for (case, steps, message) in [
            ("gap", vec![unfolded(1)], "block_hash: final stream gap: 1 sent, 0 next"),
            ("unfolded after folded", vec![folded(0, FOLDED), unfolded(1)], "unfolded 1 after"),
        ] {
            let store = DiskEngine::new(SimFs::new()).open(Path::new("/toy"), &SCHEMA);
            let (sink, _committed, writer) = start(store.expect("open"));
            for step in steps {
                sink.send(step).await;
            }
            sink.shutdown();
            let panic = writer.await.expect_err(case).into_panic();
            let got = panic.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(got.contains(message), "{batched}: {case}: {got}");
        }
    }
}
