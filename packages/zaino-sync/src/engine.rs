//! Sync engine — the orchestrator.
//!
//! Owns the DAG, dispatches extraction to workers, drives the
//! merge → commit → flush pipeline, and enforces phase gates.
//! Contains no blockchain knowledge.

use std::collections::HashSet;

use crate::backend::{Backend, BackendError, BackendWriter};
use crate::dag::DependencyDag;
use crate::pipeline::{IndexPipeline, PipelineError};
use crate::primitives::IndexId;

/// Configuration for the sync engine.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Number of blocks per persistence batch.
    pub batch_size: u32,
}

/// Errors during sync.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
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

    /// Process a range of blocks through the full pipeline.
    ///
    /// `blocks` must be in chain order. The engine splits them into
    /// batches, processes each batch phase-by-phase, commits results
    /// to the backend, and flushes after each batch.
    ///
    /// **MVP shape.** Single-threaded, synchronous. Each phase processes
    /// sequentially; indexes within a phase also process sequentially.
    /// The true north parallelises both across indexes in a phase and
    /// across blocks within an index (for BlockLocal scopes).
    pub fn sync_range(&mut self, blocks: &[Ctx]) -> Result<(), SyncError> {
        let batch_size = self.config.batch_size as usize;

        for batch in blocks.chunks(batch_size) {
            self.process_batch(batch)?;
            self.backend.flush()?;
        }

        Ok(())
    }

    fn process_batch(&mut self, batch: &[Ctx]) -> Result<(), SyncError> {
        let phases = self.dag.phases();

        for phase_nodes in &phases {
            let phase_names: HashSet<IndexId> = phase_nodes
                .iter()
                .map(|node| node.descriptor.name)
                .collect();

            let mut batch_ops = Vec::new();

            for pipeline in &self.indexes {
                if phase_names.contains(&pipeline.descriptor().name) {
                    let ops = pipeline.process_batch(batch, None)?;
                    batch_ops.extend(ops);
                }
            }

            if !batch_ops.is_empty() {
                let mut writer = self.backend.writer()?;
                writer.commit(batch_ops)?;
            }
        }

        Ok(())
    }
}
