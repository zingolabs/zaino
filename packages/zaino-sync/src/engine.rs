//! Sync engine — the orchestrator.
//!
//! The engine is a thin driver loop. The [`Scheduler`] decides what work
//! is ready; the engine executes it. The flow per block:
//!
//! 1. Scheduler emits ready extraction jobs.
//! 2. Engine calls `extract_one` on the corresponding pipelines.
//! 3. Engine reports each extraction to the scheduler.
//! 4. When a batch is fully extracted (scheduler returns a
//!    [`BatchHandle<FullyExtracted>`]), engine calls `merge` + `persist`
//!    on that pipeline, commits to backend, and reports back.
//!
//! Cross-phase dependencies are enforced by the scheduler's firing
//! rules — downstream indexes only become ready after their
//! dependencies commit.
//!
//! Contains no blockchain knowledge.
//!
//! [`Scheduler`]: crate::scheduler::Scheduler

use std::collections::HashMap;

use crate::backend::{Backend, BackendError, BackendWriter};
use crate::dag::DagError;
use crate::index_set::IndexSet;
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::primitives::IndexId;
use crate::scheduler::Scheduler;

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

        let scheduler = Scheduler::new(dag, config.batch_size);

        Ok(Self {
            scheduler,
            pipelines,
            backend,
        })
    }

    /// Sync a range of blocks through the full pipeline.
    ///
    /// `blocks` must be in chain order. The engine feeds blocks to the
    /// scheduler one at a time, executing extraction jobs as they become
    /// ready. Merge, persist, and commit happen at batch boundaries.
    ///
    /// **Current shape:** single-threaded, synchronous. Extraction jobs
    /// are executed sequentially. The scheduler infrastructure supports
    /// parallel dispatch — swapping in a parallel executor is a future
    /// step that does not change this method's signature.
    pub fn sync_range(&mut self, blocks: &[Ctx]) -> Result<(), SyncError> {
        let total_blocks = u32::try_from(blocks.len())
            .expect("block count fits in u32");
        self.scheduler.set_total_blocks(total_blocks);
        let total_batches = total_blocks.div_ceil(self.scheduler.batch_size());

        // Feed blocks to ready indexes, one at a time.
        let mut block_cursor = 0u32;

        loop {
            // Get all ready extraction jobs.
            let jobs = self.scheduler.ready_extractions();

            if jobs.is_empty() {
                // No extractions ready — check if we're done.
                if !self.scheduler.has_pending_work(total_batches) {
                    break;
                }

                // Try to flush any pending merges/commits that might
                // unblock downstream indexes.
                let merge_handles = self.scheduler.ready_for_merge();
                if merge_handles.is_empty() {
                    // Nothing to do — shouldn't happen in a correct DAG.
                    break;
                }

                for handle in merge_handles {
                    self.merge_persist_commit(handle)?;
                }
                continue;
            }

            for job in &jobs {
                // Resolve the block index within the full range.
                let global_offset = job.batch.value() * self.scheduler.batch_size()
                    + job.block_offset;

                if global_offset >= total_blocks {
                    // Past the end of the range — this index has fewer
                    // blocks than a full batch. Force the batch complete
                    // by reporting extraction done for the remaining slots.
                    //
                    // TODO: handle partial final batches more cleanly.
                    // For now, skip and let the batch complete naturally.
                    continue;
                }

                let ctx = &blocks[global_offset as usize];
                let pipeline = self.pipelines.get(&job.index)
                    .expect("scheduler only emits registered indexes");

                pipeline.extract_one(ctx)?;

                if let Some(handle) = self.scheduler.extraction_done(job.index) {
                    self.merge_persist_commit(handle)?;
                }
            }

            // Advance the block cursor for bookkeeping.
            block_cursor = block_cursor.max(
                jobs.iter()
                    .map(|j| j.batch.value() * self.scheduler.batch_size() + j.block_offset + 1)
                    .max()
                    .unwrap_or(block_cursor),
            );
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
}
