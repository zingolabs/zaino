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

use crate::dag::{DependencyDag, FiringRule};
use crate::primitives::{BatchIndex, IndexId};

/// A single extraction job the engine can schedule.
#[derive(Debug, Clone)]
pub struct ExtractJob {
    /// Which index to extract for.
    pub index: IndexId,
    /// Which batch this block belongs to.
    pub batch: BatchIndex,
    /// Offset of this block within the batch (0-based).
    pub block_offset: u32,
}

/// The scheduler: static DAG + runtime progress tracking.
pub struct Scheduler {
    dag: DependencyDag,
    batch_size: u32,

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

            let extracted = self.extracted_in_batch[&id];
            if extracted >= self.batch_size {
                continue;
            }

            let batch = self.current_batch[&id];
            if !self.firing_rules_satisfied(id, batch) {
                continue;
            }

            // Emit one job for the next block to extract.
            // The engine may call this repeatedly, or take multiple
            // jobs at once for parallel dispatch.
            jobs.push(ExtractJob {
                index: id,
                batch,
                block_offset: extracted,
            });
        }

        jobs
    }

    /// Record that one extraction completed for an index.
    ///
    /// When the batch is fully extracted, the index transitions to
    /// pending-merge.
    pub fn extraction_done(&mut self, index: IndexId) {
        let count = self.extracted_in_batch.get_mut(&index)
            .expect("index exists in scheduler");
        *count += 1;

        if *count >= self.batch_size {
            self.pending_merge.insert(index);
        }
    }

    /// Which indexes have a full batch of deltas ready for merge?
    pub fn ready_for_merge(&self) -> Vec<IndexId> {
        self.pending_merge.iter().copied().collect()
    }

    /// Record that merge completed for an index.
    ///
    /// The index transitions to pending-commit.
    pub fn merge_done(&mut self, index: IndexId) {
        self.pending_merge.remove(&index);
        self.pending_commit.insert(index);
    }

    /// Record that a batch was committed for an index.
    ///
    /// Updates committed-through tracking, resets extraction counter,
    /// and advances the index to the next batch.
    pub fn batch_committed(&mut self, index: IndexId) {
        let batch = self.current_batch[&index];

        self.pending_commit.remove(&index);
        self.committed_through.insert(index, Some(batch));

        // Advance to next batch.
        let next = BatchIndex::new(batch.value() + 1);
        self.current_batch.insert(index, next);
        self.extracted_in_batch.insert(index, 0);
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
        let sched = Scheduler::new(dag, 3);

        let jobs = sched.ready_extractions();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].index, A);
        assert_eq!(jobs[0].batch, BatchIndex::new(0));
        assert_eq!(jobs[0].block_offset, 0);
    }

    #[test]
    fn downstream_blocked_until_upstream_commits() {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_A),
        ])
        .expect("valid dag");
        let sched = Scheduler::new(dag, 2);

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

        // Extract A's full batch.
        sched.extraction_done(A);
        sched.extraction_done(A);

        // A is pending merge, not extracting more.
        assert!(sched.ready_for_merge().contains(&A));

        // B still blocked — A hasn't committed yet.
        let jobs = sched.ready_extractions();
        assert!(jobs.iter().all(|j| j.index != B));

        // Merge and commit A.
        sched.merge_done(A);
        sched.batch_committed(A);

        // Now B is ready.
        let jobs = sched.ready_extractions();
        assert!(jobs.iter().any(|j| j.index == B));
    }

    #[test]
    fn extraction_advances_through_batch() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let mut sched = Scheduler::new(dag, 3);

        // First extraction.
        let jobs = sched.ready_extractions();
        assert_eq!(jobs[0].block_offset, 0);

        sched.extraction_done(A);

        let jobs = sched.ready_extractions();
        assert_eq!(jobs[0].block_offset, 1);

        sched.extraction_done(A);
        sched.extraction_done(A);

        // Batch fully extracted — A is pending merge, no more extract jobs.
        assert!(sched.ready_for_merge().contains(&A));
        let jobs = sched.ready_extractions();
        assert!(jobs.iter().all(|j| j.index != A));
    }

    #[test]
    fn commit_advances_to_next_batch() {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])
            .expect("valid dag");
        let mut sched = Scheduler::new(dag, 2);

        // Complete batch 0.
        sched.extraction_done(A);
        sched.extraction_done(A);
        sched.merge_done(A);
        sched.batch_committed(A);

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
        let sched = Scheduler::new(dag, 3);

        let jobs = sched.ready_extractions();
        let ready_ids: HashSet<IndexId> = jobs.iter().map(|j| j.index).collect();
        assert!(ready_ids.contains(&A));
        assert!(ready_ids.contains(&B));
    }
}
