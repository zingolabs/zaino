//! Sync engine — the orchestrator.
//!
//! Owns the DAG, dispatches extraction to workers, drives the
//! merge → commit → flush pipeline, and enforces phase gates.
//! Contains no blockchain knowledge.

use crate::backend::Backend;
use crate::dag::DependencyDag;
use crate::erased::ErasedIndex;

/// Configuration for the sync engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Number of blocks per persistence batch.
    pub batch_size: u32,
    /// Maximum number of concurrent extraction tasks.
    pub extraction_parallelism: usize,
}

/// The sync engine.
///
/// Generic over the backend. Holds the DAG, the registered (erased) indexes,
/// and drives the pipeline.
pub struct SyncEngine<B: Backend> {
    dag: DependencyDag,
    indexes: Vec<Box<dyn ErasedIndex>>,
    backend: B,
    config: EngineConfig,
}

impl<B: Backend> SyncEngine<B> {
    /// Create a new engine from a built DAG, registered indexes, and backend.
    pub fn new(
        dag: DependencyDag,
        indexes: Vec<Box<dyn ErasedIndex>>,
        backend: B,
        config: EngineConfig,
    ) -> Self {
        Self {
            dag,
            indexes,
            backend,
            config,
        }
    }

    // NOTE: the main `sync_range` / `sync_to_height` method is omitted
    // from this initial sketch. It will orchestrate:
    //
    // 1. Configure provisioner with union of SourceRequirements.
    // 2. Spawn provisioner task (streams BlockContexts).
    // 3. Per phase:
    //    a. Spawn extraction workers (respecting scope constraints).
    //    b. Accumulate deltas in bounded channel.
    //    c. On batch boundary: merge → commit → flush → advance watermark.
    //    d. Signal downstream phases via phase gate.
    // 4. On completion: final flush, report progress.
}
