//! Sync engine — the orchestrator.
//!
//! The engine sits between supply (blocks in a [`BlockBuffer`]) and
//! demand (the [`Scheduler`]'s ready-work queue). Its loop is:
//!
//! 1. Ask the scheduler for ready [`Task`]s.
//! 2. For extraction tasks: look up the block in the buffer, call
//!    `extract_one`, report completion to the scheduler.
//! 3. When a batch is fully extracted, merge + persist + commit and
//!    report back.
//! 4. Evict buffer entries once all indexes commit past a batch.
//!
//! Cross-phase dependencies are enforced by the scheduler's firing
//! rules — downstream indexes only become ready after their
//! dependencies commit.
//!
//! Contains no blockchain knowledge.
//!
//! [`BlockBuffer`]: crate::block_buffer::BlockBuffer
//! [`Scheduler`]: crate::scheduler::Scheduler
//! [`Task`]: crate::scheduler::Task

use std::collections::HashMap;

use rayon::prelude::*;

use crate::backend::{Backend, BackendReader, BackendWriter, Namespace, WriteOp};
use crate::block_buffer::BlockBuffer;
use crate::dag::DagError;
use crate::encode::{Decode, Encode};
use crate::index_set::IndexSet;
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::primitives::{BatchIndex, BlockHeight, BlockOffset, IndexId};
use crate::scheduler::{ExtractJob, Scheduler, Task};

/// Namespace for engine metadata (watermark, etc.) — not an index.
const METADATA_NS: Namespace = Namespace::new("_engine_meta");

/// Key for the committed-height watermark entry.
const WATERMARK_KEY: &[u8] = b"committed_height";

/// Configuration for the sync engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Number of blocks per persistence batch.
    pub batch_size: u32,
    /// The block height of the first block in this sync run.
    ///
    /// Used to compute absolute committed heights for the watermark.
    /// On a fresh sync this is genesis (0); on resume the caller
    /// reads the prior watermark and sets this to `watermark + 1`.
    pub start_height: BlockHeight,
}

/// Errors during sync.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The dependency graph is invalid.
    #[error(transparent)]
    Dag(#[from] DagError),
    /// An index's extract or merge step failed.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
    /// Failed to open a backend reader or writer.
    #[error(transparent)]
    BackendOpen(#[from] crate::backend::OpenError),
    /// Failed to read from the backend.
    #[error(transparent)]
    BackendRead(#[from] crate::backend::ReadError),
    /// Failed to commit a batch to the backend.
    #[error(transparent)]
    BackendCommit(#[from] crate::backend::CommitError),
    /// Failed to flush the backend.
    #[error(transparent)]
    BackendFlush(#[from] crate::backend::FlushError),
}

/// The sync engine.
///
/// Generic over:
/// - `Ctx`: the provisioner's block context type (concrete, shared across
///   all indexes — no type erasure).
/// - `B`: the storage backend.
///
/// The engine owns a [`Scheduler`] that tracks progress and enforces
/// ordering via phantom-typed [`BatchHandle`](crate::scheduler::BatchHandle)s.
/// Pipelines are looked up by `IndexId` for O(1) dispatch.
pub struct SyncEngine<Ctx, B: Backend> {
    scheduler: Scheduler,
    pipelines: HashMap<IndexId, Box<dyn IndexPipeline<Ctx>>>,
    backend: B,
    buffer: BlockBuffer<Ctx>,
    start_height: BlockHeight,
    /// Write ops waiting for an atomic batch commit. Each index's
    /// persist step pushes ops here; the actual backend write happens
    /// when all indexes have persisted for that batch.
    pending_ops: HashMap<BatchIndex, Vec<WriteOp>>,
    evicted_through: Option<BatchIndex>,
}

impl<Ctx: Send + Sync + 'static, B: Backend> SyncEngine<Ctx, B> {
    /// Create an engine from a declarative [`IndexSet`].
    ///
    /// Builds the dependency DAG, constructs the scheduler, indexes
    /// pipelines by name for O(1) lookup, and hydrates pipeline state
    /// from the backend. SelfCumulative pipelines decode their running
    /// accumulators from previously committed data; BlockLocal
    /// pipelines have no state to load (no-op). If the backend is
    /// empty the load is a no-op for all pipelines.
    pub fn from_index_set(
        set: IndexSet<Ctx>,
        backend: B,
        config: EngineConfig,
    ) -> Result<Self, SyncError> {
        let (dag, index_vec) = set.build()?;

        let pipelines: HashMap<IndexId, Box<dyn IndexPipeline<Ctx>>> = index_vec
            .into_iter()
            .map(|p| (p.descriptor().name, p))
            .collect();

        let reader = backend.reader()?;
        for pipeline in pipelines.values() {
            pipeline.load_state(&reader)?;
        }

        let batch_size = config.batch_size;
        let scheduler = Scheduler::new(dag, batch_size);

        Ok(Self {
            scheduler,
            pipelines,
            backend,
            buffer: BlockBuffer::new(batch_size),
            start_height: config.start_height,
            pending_ops: HashMap::new(),
            evicted_through: None,
        })
    }

    /// The committed-height watermark from a prior sync run, if any.
    ///
    /// Read from the backend before engine construction. Returns `None`
    /// on a fresh backend. The caller uses this to decide what
    /// `start_height` to pass and where to begin provisioning.
    pub fn committed_height(backend: &B) -> Result<Option<BlockHeight>, SyncError> {
        let reader = backend.reader()?;
        let raw = reader.get(METADATA_NS, WATERMARK_KEY)?;
        match raw {
            Some(bytes) => {
                let height = BlockHeight::decode(&bytes)
                    .map_err(|e| PipelineError::Persist(e.to_string()))?;
                Ok(Some(height))
            }
            None => Ok(None),
        }
    }

    /// Sync a pre-loaded range of blocks.
    ///
    /// Convenience wrapper around [`sync_streaming`](Self::sync_streaming)
    /// for the common case where all blocks are available upfront.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, fields(block_count = blocks.len())))]
    pub fn sync_range(&mut self, blocks: Vec<Ctx>) -> Result<(), SyncError> {
        let result = self.sync_streaming(blocks);
        if result.is_ok() {
            self.assert_post_sync_invariants();
        }
        result
    }

    /// Sync blocks from an incremental source.
    ///
    /// Pulls blocks from `source` in batches, pushes them into the
    /// internal [`BlockBuffer`], and interleaves with task processing.
    /// Each iteration:
    ///
    /// 1. **Supply**: pull up to `batch_size` blocks from the source.
    /// 2. **Demand**: execute all ready [`Task`]s from the scheduler.
    /// 3. **Evict**: drop buffer entries once all indexes commit past a batch.
    ///
    /// The loop terminates when the source is exhausted and no work
    /// remains. This is the core sync loop — [`sync_range`](Self::sync_range)
    /// delegates to it.
    ///
    /// **Current shape:** single-threaded, synchronous. The scheduler
    /// infrastructure supports parallel dispatch — swapping in an async
    /// executor is a future step.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub(crate) fn sync_streaming<I>(&mut self, source: I) -> Result<(), SyncError>
    where
        I: IntoIterator<Item = Ctx>,
    {
        let mut source = source.into_iter();
        let mut provisioner_done = false;

        loop {
            // Supply: pull up to one batch of blocks from the source.
            if !provisioner_done {
                for _ in 0..self.scheduler.batch_size() {
                    match source.next() {
                        Some(ctx) => {
                            let offset = BlockOffset::new(self.buffer.total_pushed());
                            self.buffer.push(offset, ctx);
                        }
                        None => {
                            self.scheduler.provisioner_done(self.buffer.total_pushed());
                            provisioner_done = true;
                            break;
                        }
                    }
                }
                if !provisioner_done {
                    self.scheduler.set_blocks_available(self.buffer.total_pushed());
                }
            }

            // Demand: execute all ready tasks.
            let tasks = self.scheduler.ready_work();

            if tasks.is_empty() {
                if provisioner_done {
                    break;
                }
                continue;
            }

            self.dispatch_tasks(tasks)?;
        }

        self.backend.flush()?;
        Ok(())
    }

    /// Sync blocks arriving through an async channel.
    ///
    /// The provisioner runs independently (typically a spawned task) and
    /// sends blocks through the channel. The engine drains available
    /// blocks, processes ready tasks, and awaits more blocks when idle.
    /// The channel closing signals provisioner completion.
    ///
    /// This is the production entry point — the provisioner and engine
    /// run concurrently, with the [`BlockBuffer`] absorbing the rate
    /// difference between supply and demand.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    pub async fn sync_channel(
        &mut self,
        mut rx: tokio::sync::mpsc::Receiver<Ctx>,
    ) -> Result<(), SyncError> {
        let mut provisioner_done = false;

        loop {
            if !provisioner_done {
                provisioner_done = self.drain_channel(&mut rx);
            }

            let tasks = self.scheduler.ready_work();

            if tasks.is_empty() {
                if provisioner_done {
                    break;
                }
                provisioner_done = self.await_block(&mut rx).await;
                continue;
            }

            self.dispatch_tasks(tasks)?;
        }

        self.backend.flush()?;
        self.assert_post_sync_invariants();
        Ok(())
    }

    /// Non-blocking drain: pull all available blocks from the channel.
    ///
    /// Returns `true` if the channel disconnected (provisioner done).
    fn drain_channel(
        &mut self,
        rx: &mut tokio::sync::mpsc::Receiver<Ctx>,
    ) -> bool {
        loop {
            match rx.try_recv() {
                Ok(ctx) => self.push_block(ctx),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return false,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.scheduler
                        .provisioner_done(self.buffer.total_pushed());
                    return true;
                }
            }
        }
    }

    /// Blocking wait for the next block from the channel.
    ///
    /// Returns `true` if the channel closed (provisioner done).
    async fn await_block(
        &mut self,
        rx: &mut tokio::sync::mpsc::Receiver<Ctx>,
    ) -> bool {
        match rx.recv().await {
            Some(ctx) => {
                self.push_block(ctx);
                false
            }
            None => {
                self.scheduler
                    .provisioner_done(self.buffer.total_pushed());
                true
            }
        }
    }

    /// Push a single block into the buffer and update availability.
    fn push_block(&mut self, ctx: Ctx) {
        let offset = BlockOffset::new(self.buffer.total_pushed());
        self.buffer.push(offset, ctx);
        self.scheduler
            .set_blocks_available(self.buffer.total_pushed());
    }

    /// Execute a batch of tasks from the scheduler.
    ///
    /// Handles batch completions first, then runs all extractions in
    /// parallel via rayon, then reports completions sequentially.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, fields(task_count = tasks.len())))]
    fn dispatch_tasks(&mut self, tasks: Vec<Task>) -> Result<(), SyncError> {
        let jobs = self.flush_batch_completions(tasks)?;
        self.run_extractions_parallel(&jobs)?;
        self.report_extractions(jobs)
    }

    /// Handle all batch-completion tasks, return remaining extract jobs.
    fn flush_batch_completions(
        &mut self,
        tasks: Vec<Task>,
    ) -> Result<Vec<ExtractJob>, SyncError> {
        let mut extract_jobs = Vec::new();
        for task in tasks {
            match task {
                Task::Extract(job) => extract_jobs.push(job),
                Task::CompleteBatch { index, .. } => {
                    let handle = self.scheduler.ready_for_merge()
                        .into_iter()
                        .find(|h| h.index == index);
                    if let Some(handle) = handle {
                        self.merge_persist(handle)?;
                        self.try_commit()?;
                    }
                }
            }
        }
        Ok(extract_jobs)
    }

    /// Run extractions in parallel via rayon's work-stealing pool.
    ///
    /// Prepares (pipeline, context) pairs on the calling thread, then
    /// fans out via `par_iter`. Borrows from `self` — no Arc cloning.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, fields(job_count = jobs.len())))]
    fn run_extractions_parallel(&self, jobs: &[ExtractJob]) -> Result<(), SyncError> {
        let work: Vec<_> = jobs.iter().map(|job| {
            let ctx = self.buffer.get(job.global_offset)
                .expect("block available — scheduler verified watermark");
            let pipeline = self.pipelines.get(&job.index)
                .expect("scheduler only emits registered indexes");
            (pipeline, ctx)
        }).collect();

        work.par_iter()
            .try_for_each(|(pipeline, ctx)| pipeline.extract_one(ctx))?;

        Ok(())
    }

    /// Report completed extractions to the scheduler.
    ///
    /// May trigger merges when a batch becomes fully extracted.
    fn report_extractions(&mut self, jobs: Vec<ExtractJob>) -> Result<(), SyncError> {
        for job in jobs {
            if let Some(handle) = self.scheduler.extraction_done(job.index) {
                self.merge_persist(handle)?;
                self.try_commit()?;
            }
        }
        Ok(())
    }

    /// Merge and persist a fully-extracted batch for one index.
    ///
    /// Combines deltas into domain state, serializes to [`WriteOp`]s,
    /// and stashes the ops in [`pending_ops`](Self::pending_ops). The
    /// actual backend write happens later in [`try_commit_and_evict`]
    /// when ALL indexes have persisted for the batch — a single atomic
    /// commit covers every index's data plus the watermark.
    #[cfg_attr(feature = "tracing", tracing::instrument(
        skip_all,
        fields(index = %handle.index, batch = handle.batch.value())
    ))]
    fn merge_persist(
        &mut self,
        handle: crate::scheduler::BatchHandle<crate::scheduler::FullyExtracted>,
    ) -> Result<(), SyncError> {
        let batch = handle.batch;
        let index_id = handle.index;

        // The pipeline must exist — the scheduler only emits registered indexes.
        debug_assert!(
            self.pipelines.contains_key(&index_id),
            "merge_persist called for unknown index {index_id}",
        );
        let pipeline = self.pipelines.get(&index_id)
            .expect("scheduler only emits registered indexes");

        // Merge: combine deltas (domain types).
        pipeline.merge()?;

        // Persist: domain → WriteOps, stash for atomic commit.
        let ops = pipeline.persist()?;
        #[cfg(feature = "tracing")]
        tracing::debug!(op_count = ops.len(), "persist produced ops");
        self.pending_ops.entry(batch).or_default().extend(ops);

        // Advance the scheduler — downstream indexes can proceed.
        let merged = self.scheduler.merge_done(handle);
        self.scheduler.batch_committed(merged);

        Ok(())
    }

    /// Atomically commit all stashed ops for fully-persisted batches.
    ///
    /// When all indexes have persisted for a batch, drains the
    /// stashed [`WriteOp`]s, appends the watermark (highest committed
    /// block height), and writes everything in a single backend call.
    /// The watermark never leads any index's data.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
    fn try_commit(&mut self) -> Result<(), SyncError> {
        loop {
            let candidate = match self.evicted_through {
                None => BatchIndex::new(0),
                Some(b) => BatchIndex::new(b.value() + 1),
            };
            if !self.scheduler.all_committed_through(candidate) {
                break;
            }

            let mut ops = self.pending_ops.remove(&candidate).unwrap_or_default();

            // Watermark: highest committed height for this batch.
            let batch_size = u64::from(self.scheduler.batch_size());
            let max_offset =
                ((u64::from(candidate.value()) + 1) * batch_size)
                    .min(u64::from(self.buffer.total_pushed()));
            let committed_height =
                BlockHeight::new(self.start_height.value() + max_offset - 1);

            ops.push(WriteOp::Put {
                namespace: METADATA_NS,
                key: WATERMARK_KEY.to_vec(),
                value: committed_height.encode(),
            });

            #[cfg(feature = "tracing")]
            tracing::info!(
                batch = candidate.value(),
                op_count = ops.len(),
                committed_height = committed_height.value(),
                "atomic batch commit"
            );

            // Watermark must advance monotonically.
            debug_assert!(
                self.evicted_through.map_or(true, |prev| candidate.value() > prev.value()),
                "try_commit batch {} but already committed through {:?}",
                candidate.value(),
                self.evicted_through.map(|b| b.value()),
            );

            let mut writer = self.backend.writer()?;
            writer.commit(ops)?;

            self.try_evict(candidate);
        }
        Ok(())
    }

    /// Evict a committed batch's blocks from the buffer.
    ///
    /// Called after the atomic commit succeeds. Drops block contexts
    /// that are no longer needed by any index.
    fn try_evict(&mut self, batch: BatchIndex) {
        // Eviction must be monotonic.
        debug_assert!(
            self.evicted_through.map_or(true, |prev| batch.value() > prev.value()),
            "eviction must advance: evicting batch {} but already evicted through {:?}",
            batch.value(),
            self.evicted_through.map(|b| b.value()),
        );

        self.buffer.evict_through_batch(batch);
        self.evicted_through = Some(batch);
    }

    // -----------------------------------------------------------------------
    // Invariant assertions
    // -----------------------------------------------------------------------

    /// Invariants that must hold after a successful sync run.
    fn assert_post_sync_invariants(&self) {
        debug_assert!(
            self.buffer.is_empty(),
            "buffer must be empty after sync, has {} blocks remaining",
            self.buffer.len(),
        );

        debug_assert!(
            self.pending_ops.is_empty(),
            "pending_ops must be drained after sync, {} batches remain: {:?}",
            self.pending_ops.len(),
            self.pending_ops.keys().map(|b| b.value()).collect::<Vec<_>>(),
        );

        // If any blocks were processed, eviction must have covered all batches.
        if self.buffer.total_pushed() > 0 {
            let batch_size = self.scheduler.batch_size();
            let total = self.buffer.total_pushed();
            let expected_last_batch = (total - 1) / batch_size;
            debug_assert_eq!(
                self.evicted_through.map(|b| b.value()),
                Some(expected_last_batch),
                "eviction must cover all batches: expected through batch {}, got {:?}",
                expected_last_batch,
                self.evicted_through.map(|b| b.value()),
            );
        }
    }
}

#[cfg(test)]
impl<Ctx: Send + Sync + 'static, B: Backend> SyncEngine<Ctx, B> {
    pub(crate) fn buffer_len(&self) -> usize {
        self.buffer.len()
    }

    pub(crate) fn evicted_through(&self) -> Option<BatchIndex> {
        self.evicted_through
    }
}
