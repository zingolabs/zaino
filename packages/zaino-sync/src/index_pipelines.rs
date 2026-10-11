//! Index set — declarative collection of indexes passed to the engine.
//!
//! The user defines indexes (descriptor + extract + merge), registers them
//! into an `IndexPipelines`, and hands the set to the engine. The set handles
//! DAG construction and validation internally.

use crate::backend::{KeyOrder, NamespaceSpec};
use crate::dag::{DagError, DependencyDag};
use crate::pipeline::{IndexPipeline, IntoIndexPipeline};
use crate::primitives::IndexId;

/// The dependency DAG and the boxed pipelines a built index set hands to the engine.
type BuiltIndexSet<Ctx> = (DependencyDag, Vec<Box<dyn IndexPipeline<Ctx>>>);

/// A collection of indexes to be processed by the sync engine.
///
/// Built via the [`with`](Self::with) method, which accepts any type
/// implementing [`IntoIndexPipeline`]. The index set collects pipelines
/// and, on [`build`](Self::build), constructs the dependency DAG.
///
/// # Example
///
/// ```text
/// let set = IndexPipelines::new()
///     .with::<ValueIndex>()
///     .with::<CountIndex>()
///     .with::<RunningSumIndex>();
///
/// let engine = SyncEngine::from_pipelines(set, backend, config)?;
/// ```
pub struct IndexPipelines<Ctx: Send + Sync + 'static> {
    pipelines: Vec<Box<dyn IndexPipeline<Ctx>>>,
}

impl<Ctx: Send + Sync + 'static> IndexPipelines<Ctx> {
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

    /// The [`NamespaceSpec`] of every registered index, in registration order —
    /// each index's namespace paired with the key order its codec states.
    ///
    /// These map one-to-one onto the persistence namespaces the engine writes
    /// each index under. A backend that must declare its namespaces before use
    /// (e.g. LMDB) opens exactly these, plus the engine's reserved bookkeeping
    /// namespaces ([`zaino_persistence_codec::reserved_namespaces`], tagged
    /// [`KeyOrder::Meta`](crate::backend::KeyOrder::Meta) by the caller). A
    /// backend that creates namespaces lazily (e.g. the in-memory one) can
    /// ignore this.
    pub fn namespace_specs(&self) -> Vec<NamespaceSpec> {
        self.pipelines
            .iter()
            .map(|pipeline| {
                let descriptor = pipeline.descriptor();
                NamespaceSpec {
                    namespace: descriptor.name.into(),
                    key_order: descriptor.key_order,
                }
            })
            .collect()
    }

    /// The scattered-key indexes that another registered index reads as a
    /// dependency while it extracts.
    ///
    /// A backend that defers scattered writes during a bulk load cannot answer
    /// reads of a deferred namespace until the load finishes, so a set where this
    /// is non-empty cannot be bulk-loaded with deferral: its cross index would
    /// read namespaces that are not there yet.
    pub fn scattered_dependencies(&self) -> Vec<IndexId> {
        let scattered: Vec<IndexId> = self
            .pipelines
            .iter()
            .map(|pipeline| pipeline.descriptor())
            .filter(|descriptor| matches!(descriptor.key_order, KeyOrder::Scattered))
            .map(|descriptor| descriptor.name)
            .collect();
        let mut read: Vec<IndexId> = self
            .pipelines
            .iter()
            .flat_map(|pipeline| pipeline.descriptor().dependencies.iter().copied())
            .filter(|dependency| scattered.contains(dependency))
            .collect();
        read.sort_unstable_by_key(|id| id.to_string());
        read.dedup();
        read
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
    pub(crate) fn build(self) -> Result<BuiltIndexSet<Ctx>, DagError> {
        let descriptors: Vec<_> = self
            .pipelines
            .iter()
            .map(|p| p.descriptor().clone())
            .collect();

        let dag = DependencyDag::build(descriptors)?;
        Ok((dag, self.pipelines))
    }
}

impl<Ctx: Send + Sync + 'static> Default for IndexPipelines<Ctx> {
    fn default() -> Self {
        Self::new()
    }
}
