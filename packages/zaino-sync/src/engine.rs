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
    pipelines: HashMap<IndexId, Box<dyn IndexPipeline<Ctx>>>,
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

        let pipelines: HashMap<IndexId, Box<dyn IndexPipeline<Ctx>>> = index_vec
            .into_iter()
            .map(|p| (p.descriptor().name, p))
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

    /// Sync a range of blocks through the full pipeline.
    ///
    /// Blocks must be in chain order. The engine loads them into an
    /// internal [`BlockBuffer`], signals the scheduler that all blocks
    /// are available, and then runs a demand-driven loop:
    ///
    /// 1. Ask the scheduler for ready [`Task`]s.
    /// 2. Execute each task (extract via buffer lookup, or merge/persist/commit).
    /// 3. Report completions back to the scheduler.
    /// 4. Evict buffer entries once all indexes commit past a batch.
    ///
    /// **Current shape:** single-threaded, synchronous. The scheduler
    /// infrastructure supports parallel dispatch — swapping in an async
    /// executor is a future step that does not change this method's
    /// contract.
    pub fn sync_range(&mut self, blocks: Vec<Ctx>) -> Result<(), SyncError> {
        let total_blocks = u32::try_from(blocks.len())
            .expect("block count fits in u32");

        if total_blocks == 0 {
            return Ok(());
        }

        // Supply: load all blocks into the buffer.
        for (i, ctx) in blocks.into_iter().enumerate() {
            self.buffer.push(
                BlockOffset::new(u32::try_from(i).expect("block index fits in u32")),
                ctx,
            );
        }
        self.scheduler.provisioner_done(total_blocks);

        let total_batches = total_blocks.div_ceil(self.scheduler.batch_size());

        // Demand loop: pull tasks, execute, report.
        loop {
            if !self.scheduler.has_pending_work(total_batches) {
                break;
            }

            let tasks = self.scheduler.ready_work();
            if tasks.is_empty() {
                break;
            }

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
        }

        self.backend.flush()?;
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
