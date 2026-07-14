//! Scheduler — the brain of the sync engine.
//!
//! The scheduler combines the static [`DependencyDag`] with runtime state
//! to answer: "what work is ready right now?" It tracks which extractions
//! have completed, which batches are pending merge, and which batches
//! have been committed — using this to evaluate per-edge firing rules
//! and emit ready work.
//!
//! The scheduler is passive. It does not execute work — it only decides
//! what CAN run. The engine drives it in a loop:
//!
//! 1. Ask for ready extraction jobs.
//! 2. Hand jobs to an executor.
//! 3. Report completed extractions back.
//! 4. Ask which indexes are ready for merge.
//! 5. Report completed merges.
//! 6. Report committed batches (unlocks downstream firing rules).

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

use crate::dag::{DependencyDag, FiringRule};
use crate::primitives::{BatchIndex, BlockOffset, IndexId};

// ===========================================================================
// State markers — phantom types for index batch lifecycle
// ===========================================================================

/// The batch has been fully extracted — all block deltas are available.
pub struct FullyExtracted(());

/// The batch has been merged — deltas combined into index state.
pub struct Merged(());

/// A typed handle to an index at a specific batch lifecycle stage.
///
/// The engine receives these from scheduler transition methods and must
/// pass them to the next step. Invalid orderings (e.g. committing
/// before merging) are compile errors — the engine can only obtain a
/// `BatchHandle<Merged>` by calling `merge_done` with a
/// `BatchHandle<FullyExtracted>`.
pub struct BatchHandle<State> {
    /// Which index this handle is for.
    pub index: IndexId,
    /// Which batch.
    pub batch: BatchIndex,
    _state: PhantomData<State>,
}

impl<S> BatchHandle<S> {
    fn new(index: IndexId, batch: BatchIndex) -> Self {
        Self {
            index,
            batch,
            _state: PhantomData,
        }
    }

    fn transition<T>(self) -> BatchHandle<T> {
        BatchHandle::new(self.index, self.batch)
    }
}

/// A single extraction job the engine can schedule.
#[derive(Debug, Clone)]
pub struct ExtractJob {
    /// Which index to extract for.
    pub index: IndexId,
    /// Which batch this block belongs to.
    pub batch: BatchIndex,
    /// Offset of this block within the batch (0-based).
    pub block_offset: u32,
    /// Global offset into the sync range — the key for looking up the
    /// block context in the buffer.
    pub global_offset: BlockOffset,
}

/// A unit of work the scheduler declares safe to execute.
///
/// Workers consume these without knowledge of ordering or dependencies —
/// the scheduler guarantees anything it emits can run right now.
#[derive(Debug, Clone)]
pub enum Task {
    /// Extract a delta for one index at one block.
    Extract(ExtractJob),
    /// Merge + persist + commit a fully-extracted batch.
    CompleteBatch {
        /// Which index.
        index: IndexId,
        /// Which batch.
        batch: BatchIndex,
    },
}

/// The scheduler: static DAG + runtime progress tracking.
pub struct Scheduler {
    dag: DependencyDag,
    batch_size: u32,

    /// How many blocks are currently available for extraction. Updated
    /// by the engine as the provisioner supplies blocks. Extractions
    /// are only emitted for blocks below this watermark.
    blocks_available: u32,

    /// Total number of blocks in the sync range. Set when the
    /// provisioner signals completion. Determines the effective size
    /// of the final (possibly partial) batch.
    total_blocks: Option<u32>,

    /// All index IDs, cached for iteration.
    all_indexes: Vec<IndexId>,

    /// Dependencies per index (cached from DAG edges).
    deps: HashMap<IndexId, Vec<(IndexId, FiringRule)>>,

    /// How many blocks have been extracted for each index in its current batch.
    extracted_in_batch: HashMap<IndexId, u32>,

    /// The current batch each index is working on.
    current_batch: HashMap<IndexId, BatchIndex>,

    /// The last batch each index has committed.
    /// `None` means nothing committed yet.
    committed_through: HashMap<IndexId, Option<BatchIndex>>,

    /// Indexes that have completed extraction for their current batch
    /// and are waiting for merge.
    pending_merge: HashSet<IndexId>,

    /// Indexes that have completed merge and are waiting for persist + commit.
    pending_commit: HashSet<IndexId>,
}

impl Scheduler {
    /// Create a scheduler from a DAG and batch size.
    pub fn new(dag: DependencyDag, batch_size: u32) -> Self {
        let all_indexes: Vec<IndexId> = dag
            .phases()
            .iter()
            .flat_map(|phase| phase.iter().map(|node| node.descriptor.name))
            .collect();

        let mut deps: HashMap<IndexId, Vec<(IndexId, FiringRule)>> = HashMap::new();
        for id in &all_indexes {
            deps.insert(*id, Vec::new());
        }
        for edge in dag.edges() {
            deps.entry(edge.to)
                .or_default()
                .push((edge.from, edge.firing));
        }

        let mut extracted_in_batch = HashMap::new();
        let mut current_batch = HashMap::new();
        let mut committed_through = HashMap::new();

        for &id in &all_indexes {
            extracted_in_batch.insert(id, 0u32);
            current_batch.insert(id, BatchIndex::new(0));
            committed_through.insert(id, None);
        }

        Self {
            dag,
            batch_size,
            blocks_available: 0,
            total_blocks: None,
            all_indexes,
            deps,
            extracted_in_batch,
            current_batch,
            committed_through,
            pending_merge: HashSet::new(),
            pending_commit: HashSet::new(),
        }
    }

    /// Which indexes can accept extraction jobs right now?
    ///
    /// An index is ready for extraction when:
    /// - It is not pending merge or commit.
    /// - Its current batch has not yet been fully extracted.
    /// - The block is available (provisioner has supplied it).
    /// - All dependency firing rules are satisfied for its current batch.
    ///
    /// Returns one `ExtractJob` per ready (index, block_offset) pair.
    /// The engine decides how many to dispatch (all at once for
    /// BlockLocal, one at a time for SelfCumulative).
    pub fn ready_extractions(&self) -> Vec<ExtractJob> {
        let mut jobs = Vec::new();

        for &id in &self.all_indexes {
            if self.pending_merge.contains(&id) || self.pending_commit.contains(&id) {
                continue;
            }

            let batch = self.current_batch[&id];
            let effective = self.effective_batch_size(batch);
            let extracted = self.extracted_in_batch[&id];
            if extracted >= effective {
                continue;
            }

            // Check block availability — the provisioner may not have
            // supplied this block yet.
            let global = batch.value() * self.batch_size + extracted;
            if global >= self.blocks_available {
                continue;
            }

            if !self.firing_rules_satisfied(id, batch) {
                continue;
            }

            jobs.push(ExtractJob {
                index: id,
                batch,
                block_offset: extracted,
                global_offset: BlockOffset::new(global),
            });
        }

        #[cfg(feature = "tracing")]
        if !jobs.is_empty() {
            tracing::debug!(
                job_count = jobs.len(),
                blocks_available = self.blocks_available,
                "ready_extractions"
            );
        }

        jobs
    }

    /// Record that one extraction completed for an index.
    ///
    /// Returns `Some(BatchHandle<FullyExtracted>)` when the batch is
    /// fully extracted. The engine must pass this handle to
    /// [`merge_done`](Self::merge_done) — it cannot be skipped or
    /// reordered.
    pub fn extraction_done(&mut self, index: IndexId) -> Option<BatchHandle<FullyExtracted>> {
        let batch = self.current_batch[&index];
        let effective = self.effective_batch_size(batch);

        let count = self.extracted_in_batch.get_mut(&index)
            .expect("index exists in scheduler");
        *count += 1;

        debug_assert!(
            *count <= effective,
            "extraction count {} exceeds effective batch size {} for index {} batch {}",
            *count, effective, index, batch.value(),
        );

        if *count >= effective {
            #[cfg(feature = "tracing")]
            tracing::info!(
                index = %index,
                batch = batch.value(),
                block_count = effective,
                "batch fully extracted"
            );
            self.pending_merge.insert(index);
            Some(BatchHandle::new(index, batch))
        } else {
            None
        }
    }

    /// Which indexes have a full batch of deltas ready for merge?
    ///
    /// Returns typed handles that can only be consumed by
    /// [`merge_done`](Self::merge_done).
    pub fn ready_for_merge(&self) -> Vec<BatchHandle<FullyExtracted>> {
        self.pending_merge
            .iter()
            .map(|&id| BatchHandle::new(id, self.current_batch[&id]))
            .collect()
    }

    /// Record that merge completed for an index.
    ///
    /// Consumes a `FullyExtracted` handle and returns a `Merged` handle.
    /// The engine must pass the `Merged` handle to
    /// [`batch_committed`](Self::batch_committed).
    pub fn merge_done(&mut self, handle: BatchHandle<FullyExtracted>) -> BatchHandle<Merged> {
        self.pending_merge.remove(&handle.index);
        self.pending_commit.insert(handle.index);
        handle.transition()
    }

    /// Record that a batch was committed for an index.
    ///
    /// Consumes a `Merged` handle. Updates committed-through tracking,
    /// resets extraction counter, and advances the index to the next
    /// batch.
    pub fn batch_committed(&mut self, handle: BatchHandle<Merged>) {
        let index = handle.index;
        let batch = handle.batch;

        // committed_through must advance monotonically per index.
        debug_assert!(
            self.committed_through[&index].map_or(true, |prev| batch.value() > prev.value()),
            "committed_through must advance for index {}: committing batch {} but already at {:?}",
            index,
            batch.value(),
            self.committed_through[&index].map(|b| b.value()),
        );

        // The index must be pending commit (came through merge_done).
        debug_assert!(
            self.pending_commit.contains(&index),
            "batch_committed called for index {} which is not pending commit",
            index,
        );

        self.pending_commit.remove(&index);
        self.committed_through.insert(index, Some(batch));

        #[cfg(feature = "tracing")]
        tracing::debug!(
            index = %index,
            batch = batch.value(),
            "batch committed, advancing"
        );

        // Advance to next batch.
        let next = BatchIndex::new(batch.value() + 1);
        self.current_batch.insert(index, next);
        self.extracted_in_batch.insert(index, 0);
    }

    /// All currently safe-to-execute work.
    ///
    /// Returns a mix of extraction and batch-completion tasks. The
    /// engine spawns all of them — the scheduler guarantees they can
    /// run concurrently.
    pub fn ready_work(&self) -> Vec<Task> {
        let mut tasks: Vec<Task> = self
            .ready_extractions()
            .into_iter()
            .map(Task::Extract)
            .collect();

        for handle in self.ready_for_merge() {
            tasks.push(Task::CompleteBatch {
                index: handle.index,
                batch: handle.batch,
            });
        }

        tasks
    }

    /// Update the block availability watermark.
    ///
    /// Called by the engine as the provisioner supplies blocks. The
    /// count is cumulative — "5" means blocks 0..5 are available.
    pub fn set_blocks_available(&mut self, count: u32) {
        self.blocks_available = count;
    }

    /// Signal that the provisioner has finished and no more blocks
    /// will arrive.
    ///
    /// This sets `total_blocks` so the scheduler can compute the
    /// effective size of the final (possibly partial) batch. Before
    /// this is called, the scheduler assumes every batch is full.
    pub fn provisioner_done(&mut self, total_blocks: u32) {
        self.total_blocks = Some(total_blocks);
        self.blocks_available = total_blocks;
    }

    /// Check whether all firing rules are satisfied for an index at a
    /// given batch.
    fn firing_rules_satisfied(&self, index: IndexId, batch: BatchIndex) -> bool {
        let deps = &self.deps[&index];

        for &(dep_id, rule) in deps {
            match rule {
                FiringRule::Pipelined => {
                    // Dependency must have committed at least this batch.
                    match self.committed_through[&dep_id] {
                        Some(committed) if committed >= batch => {}
                        _ => return false,
                    }
                }
                FiringRule::Barrier => {
                    // Dependency must have completed the entire chain.
                    // For now, we can't check this — barrier means "wait
                    // until the dep finishes everything." The engine must
                    // signal when a dep's full range is done.
                    // Conservative: always block.
                    // TODO: add `completed` tracking for barrier deps.
                    return false;
                }
            }
        }

        true
    }

    /// Access the underlying DAG.
    pub fn dag(&self) -> &DependencyDag {
        &self.dag
    }

    /// The batch size this scheduler was configured with.
    pub fn batch_size(&self) -> u32 {
        self.batch_size
    }

    /// Set the total number of blocks in the sync range.
    ///
    /// Convenience for the synchronous engine path — sets both
    /// `total_blocks` and `blocks_available` at once (all blocks
    /// are available immediately when pre-loaded).
    pub fn set_total_blocks(&mut self, total: u32) {
        self.provisioner_done(total);
    }

    /// Effective batch size for a given batch index.
    ///
    /// All batches are `batch_size` except possibly the last one,
    /// which may be smaller if `total_blocks` is not a multiple of
    /// `batch_size`.
    fn effective_batch_size(&self, batch: BatchIndex) -> u32 {
        match self.total_blocks {
            Some(total) => {
                let start = batch.value() * self.batch_size;
                let remaining = total.saturating_sub(start);
                remaining.min(self.batch_size)
            }
            None => self.batch_size,
        }
    }

    /// Whether all indexes have finished the given batch
    /// (committed through it).
    pub fn all_committed_through(&self, batch: BatchIndex) -> bool {
        self.all_indexes.iter().all(|id| {
            matches!(self.committed_through[id], Some(b) if b >= batch)
        })
    }

    /// Whether any index has work remaining (not all committed through
    /// the target batch).
    pub fn has_pending_work(&self, total_batches: u32) -> bool {
        if total_batches == 0 {
            return false;
        }
        let last = BatchIndex::new(total_batches - 1);
        !self.all_committed_through(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{
        CompositionType, Descriptor, InputScope, SourceAccess,
    };

    fn desc(name: &'static str, deps: &'static [IndexId]) -> Descriptor {
        Descriptor {
            name: IndexId::new(name),
            scope: InputScope::BlockLocal,
            composition: CompositionType::Append,
            dependencies: deps,
            source_access: SourceAccess::None,
        }
    }

    const A: IndexId = IndexId::new("a");
    const B: IndexId = IndexId::new("b");

    const DEPS_NONE: &[IndexId] = &[];
    const DEPS_A: &[IndexId] = &[A];

    #[test]
    fn phase_zero_indexes_ready_immediately() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let mut sched = Scheduler::new(dag, 3);
        sched.set_blocks_available(10);

        let jobs = sched.ready_extractions();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].index, A);
        assert_eq!(jobs[0].batch, BatchIndex::new(0));
        assert_eq!(jobs[0].block_offset, 0);
    }

    #[test]
    fn no_extractions_when_no_blocks_available() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let sched = Scheduler::new(dag, 3);

        // No blocks supplied yet — nothing is ready.
        assert!(sched.ready_extractions().is_empty());
        assert!(sched.ready_work().is_empty());
    }

    #[test]
    fn extractions_gate_on_availability() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let mut sched = Scheduler::new(dag, 3);

        // Only 1 block available out of a batch of 3.
        sched.set_blocks_available(1);
        let jobs = sched.ready_extractions();
        assert_eq!(jobs.len(), 1);

        // Extract it — now we need block 1 but only have 1 available.
        sched.extraction_done(A);
        assert!(sched.ready_extractions().is_empty());

        // Provisioner supplies more.
        sched.set_blocks_available(3);
        let jobs = sched.ready_extractions();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].block_offset, 1);
    }

    #[test]
    fn downstream_blocked_until_upstream_commits() {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_A),
        ])
        .expect("valid dag");
        let mut sched = Scheduler::new(dag, 2);
        sched.set_blocks_available(10);

        let jobs = sched.ready_extractions();
        // Only A is ready, B is blocked.
        let ready_ids: HashSet<IndexId> = jobs.iter().map(|j| j.index).collect();
        assert!(ready_ids.contains(&A));
        assert!(!ready_ids.contains(&B));
    }

    #[test]
    fn downstream_unblocks_after_upstream_commit() {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_A),
        ])
        .expect("valid dag");
        let mut sched = Scheduler::new(dag, 2);
        sched.set_blocks_available(10);

        // Extract A's full batch.
        assert!(sched.extraction_done(A).is_none());
        let handle = sched.extraction_done(A).expect("batch complete");

        // A is pending merge, not extracting more.
        assert!(!sched.ready_for_merge().is_empty());

        // B still blocked — A hasn't committed yet.
        let jobs = sched.ready_extractions();
        assert!(jobs.iter().all(|j| j.index != B));

        // Merge and commit A — typed handle chain.
        let merged = sched.merge_done(handle);
        sched.batch_committed(merged);

        // Now B is ready.
        let jobs = sched.ready_extractions();
        assert!(jobs.iter().any(|j| j.index == B));
    }

    #[test]
    fn extraction_advances_through_batch() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let mut sched = Scheduler::new(dag, 3);
        sched.set_blocks_available(10);

        // First extraction.
        let jobs = sched.ready_extractions();
        assert_eq!(jobs[0].block_offset, 0);

        assert!(sched.extraction_done(A).is_none());

        let jobs = sched.ready_extractions();
        assert_eq!(jobs[0].block_offset, 1);

        assert!(sched.extraction_done(A).is_none());
        let handle = sched.extraction_done(A).expect("batch complete");

        // Batch fully extracted — A is pending merge, no more extract jobs.
        assert!(!sched.ready_for_merge().is_empty());
        let jobs = sched.ready_extractions();
        assert!(jobs.iter().all(|j| j.index != A));

        // Consume handle to prove it exists.
        let merged = sched.merge_done(handle);
        sched.batch_committed(merged);
    }

    #[test]
    fn commit_advances_to_next_batch() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let mut sched = Scheduler::new(dag, 2);
        sched.set_blocks_available(10);

        // Complete batch 0 — full handle chain.
        assert!(sched.extraction_done(A).is_none());
        let extracted = sched.extraction_done(A).expect("batch complete");
        let merged = sched.merge_done(extracted);
        sched.batch_committed(merged);

        // Now on batch 1.
        let jobs = sched.ready_extractions();
        assert_eq!(jobs[0].batch, BatchIndex::new(1));
        assert_eq!(jobs[0].block_offset, 0);
    }

    #[test]
    fn independent_indexes_both_ready() {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_NONE),
        ])
        .expect("valid dag");
        let mut sched = Scheduler::new(dag, 3);
        sched.set_blocks_available(10);

        let jobs = sched.ready_extractions();
        let ready_ids: HashSet<IndexId> = jobs.iter().map(|j| j.index).collect();
        assert!(ready_ids.contains(&A));
        assert!(ready_ids.contains(&B));
    }

    #[test]
    fn ready_work_includes_both_extract_and_batch_tasks() {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_NONE),
        ])
        .expect("valid dag");
        let mut sched = Scheduler::new(dag, 2);
        sched.set_blocks_available(10);

        // Extract A's full batch.
        sched.extraction_done(A);
        sched.extraction_done(A);

        // B still extracting. A is ready to merge.
        let tasks = sched.ready_work();
        let has_extract = tasks.iter().any(|t| matches!(t, Task::Extract(_)));
        let has_batch = tasks.iter().any(|t| matches!(t, Task::CompleteBatch { .. }));
        assert!(has_extract, "should have extract tasks for B");
        assert!(has_batch, "should have batch task for A");
    }
}
