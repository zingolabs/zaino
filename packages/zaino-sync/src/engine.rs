//! Sync engine — the orchestrator.
//!
//! Owns the DAG, dispatches extraction to workers, drives the
//! merge → commit → flush pipeline, and enforces phase gates.
//! Contains no blockchain knowledge.

use crate::backend::Backend;
use crate::dag::DependencyDag;
use crate::pipeline::IndexPipeline;

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
/// Generic over:
/// - `Ctx`: the provisioner's block context type (concrete, shared across
///   all indexes — no type erasure).
/// - `B`: the storage backend.
///
/// Holds the DAG and a heterogeneous collection of indexes via
/// `dyn IndexPipeline<Ctx>`. Delta types stay inside each index's
/// pipeline — the engine only sees `Ctx` in and `Vec<WriteOp>` out.
pub struct SyncEngine<Ctx, B: Backend> {
    dag: DependencyDag,
    indexes: Vec<Box<dyn IndexPipeline<Ctx>>>,
    backend: B,
    config: EngineConfig,
}

impl<Ctx: Send + Sync + 'static, B: Backend> SyncEngine<Ctx, B> {
    /// Create a new engine from a built DAG, registered indexes, and backend.
    pub fn new(
        dag: DependencyDag,
        indexes: Vec<Box<dyn IndexPipeline<Ctx>>>,
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
}
