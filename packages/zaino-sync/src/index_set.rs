//! Index set — declarative collection of indexes passed to the engine.
//!
//! The user defines indexes (descriptor + extract + merge), registers them
//! into an `IndexSet`, and hands the set to the engine. The set handles
//! DAG construction and validation internally.

use crate::dag::{DagError, DependencyDag};
use crate::pipeline::{IndexPipeline, IntoIndexPipeline};

/// A collection of indexes to be processed by the sync engine.
///
/// Built via the [`with`](Self::with) method, which accepts any type
/// implementing [`IntoIndexPipeline`]. The index set collects pipelines
/// and, on [`build`](Self::build), constructs the dependency DAG.
///
/// # Example
///
/// ```text
/// let set = IndexSet::new()
///     .with::<ValueIndex>()
///     .with::<CountIndex>()
///     .with::<RunningSumIndex>();
///
/// let engine = SyncEngine::from_index_set(set, backend, config)?;
/// ```
pub struct IndexSet<Ctx: Send + Sync + 'static> {
    pipelines: Vec<Box<dyn IndexPipeline<Ctx>>>,
}

impl<Ctx: Send + Sync + 'static> IndexSet<Ctx> {
    /// Create an empty index set.
    pub fn new() -> Self {
        Self {
            pipelines: Vec::new(),
        }
    }

    /// Register an index. The index must implement [`IntoIndexPipeline`],
    /// which provides the bridge to the engine's runtime dispatch.
    pub fn with<I: IntoIndexPipeline<Ctx>>(mut self) -> Self {
        self.pipelines.push(I::into_pipeline());
        self
    }

    /// Return a description line for each registered index.
    pub fn describe(&self) -> Vec<String> {
        self.pipelines
            .iter()
            .map(|p| p.descriptor().to_string())
            .collect()
    }

    /// Build the dependency DAG and return the parts the engine needs.
    ///
    /// Validates uniqueness, dependency existence, and acyclicity.
    pub(crate) fn build(self) -> Result<(DependencyDag, Vec<Box<dyn IndexPipeline<Ctx>>>), DagError> {
        let descriptors: Vec<_> = self
            .pipelines
            .iter()
            .map(|p| p.descriptor().clone())
            .collect();

        let dag = DependencyDag::build(descriptors)?;
        Ok((dag, self.pipelines))
    }
}

impl<Ctx: Send + Sync + 'static> Default for IndexSet<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}
