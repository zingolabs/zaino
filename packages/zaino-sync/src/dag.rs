//! Dependency DAG construction and phase assignment.

use std::collections::HashMap;

use crate::descriptor::{CompositionType, Descriptor, InputScope};
use crate::primitives::{IndexId, PhaseIndex};

/// A node in the dependency DAG, holding the descriptor and computed
/// scheduling metadata.
#[derive(Debug)]
pub struct DagNode {
    /// The index's declarative descriptor.
    pub descriptor: Descriptor,
    /// Topological phase (layer in the DAG). Phase 0 has no dependencies.
    pub phase: PhaseIndex,
}

/// The dependency DAG over registered indexes.
///
/// Built at startup from the set of descriptors. The engine uses the DAG
/// to determine phase assignment, per-edge firing rules, and batch
/// scheduling.
#[derive(Debug)]
pub struct DependencyDag {
    nodes: HashMap<IndexId, DagNode>,
    edges: Vec<DagEdge>,
}

/// A directed edge in the DAG: `from` must commit before `to` can extract.
#[derive(Debug)]
pub struct DagEdge {
    /// The dependency (upstream index).
    pub from: IndexId,
    /// The dependent (downstream index).
    pub to: IndexId,
    /// Scheduling rule derived from the dependency's composition type
    /// and the dependent's read pattern.
    pub firing: FiringRule,
}

/// Per-edge firing rule — when may the downstream index begin extracting
/// blocks that depend on the upstream index's output?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiringRule {
    /// Downstream can begin as soon as the upstream commits a batch
    /// containing the needed height (pipelined).
    Pipelined,
    /// Downstream must wait until the upstream has committed the entire
    /// chain range (barrier). Required when the downstream reads
    /// forward or globally from the upstream.
    Barrier,
}

/// Errors during DAG construction.
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    /// A dependency references an index that was not registered.
    #[error("unknown dependency: {from} -> {to}")]
    UnknownDependency {
        /// The index that declared the dependency.
        from: IndexId,
        /// The dependency that was not found.
        to: IndexId,
    },
    /// The dependency graph contains a cycle.
    #[error("cycle detected involving: {participants:?}")]
    CycleDetected {
        /// The indexes involved in the cycle.
        participants: Vec<IndexId>,
    },
}

impl DependencyDag {
    /// Build the DAG from a set of descriptors.
    ///
    /// Validates acyclicity and computes phase assignments.
    pub fn build(_descriptors: Vec<Descriptor>) -> Result<Self, DagError> {
        todo!()
    }

    /// Return indexes grouped by phase, in phase order.
    pub fn phases(&self) -> Vec<Vec<&DagNode>> {
        todo!()
    }

    /// Return the firing rule for the edge between two indexes.
    pub fn firing_rule(&self, _from: IndexId, _to: IndexId) -> Option<FiringRule> {
        todo!()
    }

    /// Compute the scheduling properties of a given cell (scope × composition).
    pub fn cell_properties(
        _scope: InputScope,
        _composition: CompositionType,
    ) -> CellSchedulingProps {
        CellSchedulingProps {
            parallel_extract: matches!(_scope, InputScope::BlockLocal),
            parallel_merge: matches!(_composition, CompositionType::Monoidal),
            sequential_merge: matches!(_composition, CompositionType::Fold),
            requires_phase_gate: matches!(_scope, InputScope::CrossIndex),
            requires_self_feedback: matches!(_scope, InputScope::SelfCumulative),
        }
    }
}

/// Scheduling properties mechanically derived from a (scope, composition) cell.
/// No runtime state — these are static properties of the index type.
#[derive(Debug, Clone, Copy)]
pub struct CellSchedulingProps {
    /// Can extraction run in parallel across blocks?
    pub parallel_extract: bool,
    /// Can the merge step run as a parallel reduce?
    pub parallel_merge: bool,
    /// Must the merge step apply deltas sequentially?
    pub sequential_merge: bool,
    /// Must this index wait for dependency phases to commit?
    pub requires_phase_gate: bool,
    /// Must this index read its own prior committed state per block?
    pub requires_self_feedback: bool,
}
