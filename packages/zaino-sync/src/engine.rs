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
use std::sync::Arc;

use crate::backend::{Backend, BackendError, BackendWriter};
use crate::block_buffer::BlockBuffer;
use crate::dag::DagError;
use crate::index_set::IndexSet;
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::primitives::{BatchIndex, BlockOffset, IndexId};
use crate::scheduler::{Scheduler, Task};

/// Configuration for the sync engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Number of blocks per persistence batch.
    pub batch_size: u32,
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
    /// The storage backend failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
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
    pipelines: HashMap<IndexId, Arc<dyn IndexPipeline<Ctx>>>,
    backend: B,
    buffer: BlockBuffer<Ctx>,
    evicted_through: Option<BatchIndex>,
}

impl<Ctx: Send + Sync + 'static, B: Backend> SyncEngine<Ctx, B> {
    /// Create an engine from a declarative [`IndexSet`].
    ///
    /// Builds the dependency DAG, constructs the scheduler, and indexes
    /// pipelines by name for O(1) lookup during dispatch.
    pub fn from_index_set(
        set: IndexSet<Ctx>,
        backend: B,
        config: EngineConfig,
    ) -> Result<Self, SyncError> {
        let (dag, index_vec) = set.build()?;

        let pipelines: HashMap<IndexId, Arc<dyn IndexPipeline<Ctx>>> = index_vec
            .into_iter()
            .map(|p| {
                let name = p.descriptor().name;
                (name, Arc::from(p))
            })
            .collect();

        let batch_size = config.batch_size;
        let scheduler = Scheduler::new(dag, batch_size);

        Ok(Self {
            scheduler,
            pipelines,
            backend,
            buffer: BlockBuffer::new(batch_size),
            evicted_through: None,
        })
    }

    /// Sync a pre-loaded range of blocks.
    ///
    /// Convenience wrapper around [`sync_streaming`](Self::sync_streaming)
    /// for the common case where all blocks are available upfront.
    pub fn sync_range(&mut self, blocks: Vec<Ctx>) -> Result<(), SyncError> {
        self.sync_streaming(blocks)
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
    fn dispatch_tasks(&mut self, tasks: Vec<Task>) -> Result<(), SyncError> {
        for task in tasks {
            match task {
                Task::Extract(job) => {
                    let ctx = self.buffer.get(job.global_offset)
                        .expect("block available — scheduler verified watermark");
                    let pipeline = self.pipelines.get(&job.index)
                        .expect("scheduler only emits registered indexes");

                    pipeline.extract_one(&ctx)?;

                    if let Some(handle) = self.scheduler.extraction_done(job.index) {
                        self.merge_persist_commit(handle)?;
                        self.try_evict();
                    }
                }
                Task::CompleteBatch { index, .. } => {
                    let handle = self.scheduler.ready_for_merge()
                        .into_iter()
                        .find(|h| h.index == index);

                    if let Some(handle) = handle {
                        self.merge_persist_commit(handle)?;
                        self.try_evict();
                    }
                }
            }
        }
        Ok(())
    }

    /// Merge, persist, and commit a fully-extracted batch for one index.
    fn merge_persist_commit(
        &mut self,
        handle: crate::scheduler::BatchHandle<crate::scheduler::FullyExtracted>,
    ) -> Result<(), SyncError> {
        let index_id = handle.index;

        let pipeline = self.pipelines.get(&index_id)
            .expect("scheduler only emits registered indexes");

        // Merge: combine deltas (domain types).
        pipeline.merge()?;

        // Persist: domain → WriteOps.
        let ops = pipeline.persist()?;

        // Commit to backend.
        if !ops.is_empty() {
            let mut writer = self.backend.writer()?;
            writer.commit(ops)?;
        }

        // Tell the scheduler this batch is done.
        let merged = self.scheduler.merge_done(handle);
        self.scheduler.batch_committed(merged);

        Ok(())
    }

    /// Advance the eviction frontier.
    ///
    /// After each batch commit, checks whether all indexes have moved
    /// past the next unevicted batch. If so, drops those blocks from
    /// the buffer — they are no longer needed by any index.
    fn try_evict(&mut self) {
        loop {
            let candidate = match self.evicted_through {
                None => BatchIndex::new(0),
                Some(b) => BatchIndex::new(b.value() + 1),
            };
            if self.scheduler.all_committed_through(candidate) {
                self.buffer.evict_through_batch(candidate);
                self.evicted_through = Some(candidate);
            } else {
                break;
            }
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
