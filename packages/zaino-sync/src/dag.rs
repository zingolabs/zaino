//! Dependency DAG construction and phase assignment.
//!
//! Built at startup from the set of [`Descriptor`]s. The DAG determines:
//! - **Phase assignment**: topological layers. Phase 0 has no dependencies;
//!   each subsequent phase depends only on earlier phases.
//! - **Edges and firing rules**: when downstream indexes may begin work
//!   relative to upstream commits.
//! - **Cell scheduling properties**: static parallelism/sequentiality
//!   characteristics derived from (scope × composition).

use std::collections::{HashMap, HashSet, VecDeque};

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
    phase_count: u32,
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
    /// Two descriptors share the same index name.
    #[error("duplicate index name: {0}")]
    DuplicateName(IndexId),
}

impl DependencyDag {
    /// Build the DAG from a set of descriptors.
    ///
    /// Validates uniqueness, dependency existence, and acyclicity, then
    /// computes phase assignments and firing rules.
    pub fn build(descriptors: Vec<Descriptor>) -> Result<Self, DagError> {
        let names = validate_unique_names(&descriptors)?;
        let raw_edges = collect_edges(&descriptors, &names)?;
        let phase_map = toposort_phases(&descriptors, &raw_edges)?;

        let phase_count = phase_map
            .values()
            .map(|p| p.value() + 1)
            .max()
            .unwrap_or(0);

        let desc_map: HashMap<IndexId, &Descriptor> =
            descriptors.iter().map(|d| (d.name, d)).collect();

        let edges = raw_edges
            .iter()
            .map(|&(from, to)| DagEdge {
                from,
                to,
                firing: derive_firing_rule(
                    desc_map.get(&from).expect("validated"),
                    desc_map.get(&to).expect("validated"),
                ),
            })
            .collect();

        let nodes = descriptors
            .into_iter()
            .map(|desc| {
                let phase = phase_map[&desc.name];
                (desc.name, DagNode { descriptor: desc, phase })
            })
            .collect();

        Ok(Self { nodes, edges, phase_count })
    }

    /// Return indexes grouped by phase, in phase order.
    pub fn phases(&self) -> Vec<Vec<&DagNode>> {
        let mut result: Vec<Vec<&DagNode>> = (0..self.phase_count).map(|_| Vec::new()).collect();
        for node in self.nodes.values() {
            let idx = usize::try_from(node.phase.value())
                .expect("phase index fits in usize");
            result[idx].push(node);
        }
        result
    }

    /// Total number of phases.
    pub fn phase_count(&self) -> u32 {
        self.phase_count
    }

    /// Look up a node by index id.
    pub fn node(&self, id: IndexId) -> Option<&DagNode> {
        self.nodes.get(&id)
    }

    /// All edges in the DAG.
    pub fn edges(&self) -> &[DagEdge] {
        &self.edges
    }

    /// Return the firing rule for the edge between two indexes.
    pub fn firing_rule(&self, from: IndexId, to: IndexId) -> Option<FiringRule> {
        self.edges
            .iter()
            .find(|e| e.from == from && e.to == to)
            .map(|e| e.firing)
    }

    /// Compute the scheduling properties of a given cell (scope × composition).
    pub fn cell_properties(
        scope: InputScope,
        composition: CompositionType,
    ) -> CellSchedulingProps {
        CellSchedulingProps {
            parallel_extract: matches!(scope, InputScope::BlockLocal),
            parallel_merge: matches!(composition, CompositionType::Monoidal),
            sequential_merge: matches!(composition, CompositionType::Fold),
            requires_phase_gate: matches!(scope, InputScope::CrossIndex),
            requires_self_feedback: matches!(scope, InputScope::SelfCumulative),
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

/// Validate that all descriptor names are unique.
fn validate_unique_names(descriptors: &[Descriptor]) -> Result<HashSet<IndexId>, DagError> {
    let mut names = HashSet::new();
    for desc in descriptors {
        if !names.insert(desc.name) {
            return Err(DagError::DuplicateName(desc.name));
        }
    }
    Ok(names)
}

/// Extract (from, to) edges from descriptors, validating that all
/// declared dependencies reference registered indexes.
fn collect_edges(
    descriptors: &[Descriptor],
    registered: &HashSet<IndexId>,
) -> Result<Vec<(IndexId, IndexId)>, DagError> {
    let mut edges = Vec::new();
    for desc in descriptors {
        for &dep in desc.dependencies {
            if !registered.contains(&dep) {
                return Err(DagError::UnknownDependency {
                    from: desc.name,
                    to: dep,
                });
            }
            edges.push((dep, desc.name));
        }
    }
    Ok(edges)
}

/// Kahn's algorithm: topological sort producing a phase assignment.
///
/// Returns a map from index id to its phase. Phase 0 has no
/// dependencies, phase 1 depends only on phase 0, etc.
///
/// Detects cycles: if any nodes remain after all zero-in-degree nodes
/// are exhausted, those nodes form a cycle.
fn toposort_phases(
    descriptors: &[Descriptor],
    edges: &[(IndexId, IndexId)],
) -> Result<HashMap<IndexId, PhaseIndex>, DagError> {
    let mut in_degree: HashMap<IndexId, usize> = descriptors.iter().map(|d| (d.name, 0)).collect();
    let mut dependents: HashMap<IndexId, Vec<IndexId>> = HashMap::new();

    for &(from, to) in edges {
        *in_degree.entry(to).or_insert(0) += 1;
        dependents.entry(from).or_default().push(to);
    }

    let mut queue: VecDeque<IndexId> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut phase_map: HashMap<IndexId, PhaseIndex> = HashMap::new();
    let mut visited = 0usize;
    let mut phase_idx = 0u32;

    while !queue.is_empty() {
        let layer: Vec<IndexId> = queue.drain(..).collect();
        let phase = PhaseIndex::new(phase_idx);
        for &id in &layer {
            visited += 1;
            phase_map.insert(id, phase);
            if let Some(downstream) = dependents.get(&id) {
                for &dep in downstream {
                    let deg = in_degree
                        .get_mut(&dep)
                        .expect("all nodes in in_degree map");
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(dep);
                    }
                }
            }
        }
        phase_idx += 1;
    }

    if visited != descriptors.len() {
        let participants: Vec<IndexId> = in_degree
            .iter()
            .filter(|(_, &deg)| deg > 0)
            .map(|(&id, _)| id)
            .collect();
        return Err(DagError::CycleDetected { participants });
    }

    Ok(phase_map)
}

/// Derive the firing rule for an edge.
///
/// Conservative default: `Pipelined` (downstream can start as soon as
/// upstream commits a batch). `Barrier` when the upstream uses
/// `NonLocal` source access, signalling incomplete output until the
/// full chain is processed.
///
/// Simplification: the formal model derives firing rules from the
/// dependency's read pattern (R≤ backward = pipelined, R* global =
/// barrier). We use source_access as a proxy until read-pattern
/// declarations are added to the descriptor.
fn derive_firing_rule(upstream: &Descriptor, _downstream: &Descriptor) -> FiringRule {
    if upstream.source_access == crate::descriptor::SourceAccess::NonLocal {
        FiringRule::Barrier
    } else {
        FiringRule::Pipelined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{SourceAccess, SourceRequirements};

    const A: IndexId = IndexId::new("a");
    const B: IndexId = IndexId::new("b");
    const C: IndexId = IndexId::new("c");
    const MISSING: IndexId = IndexId::new("missing");

    const DEPS_NONE: &[IndexId] = &[];
    const DEPS_A: &[IndexId] = &[A];
    const DEPS_B: &[IndexId] = &[B];
    const DEPS_AB: &[IndexId] = &[A, B];
    const DEPS_MISSING: &[IndexId] = &[MISSING];

    fn desc(name: &'static str, deps: &'static [IndexId]) -> Descriptor {
        Descriptor {
            name: IndexId::new(name),
            scope: InputScope::BlockLocal,
            composition: CompositionType::Append,
            dependencies: deps,
            requirements: SourceRequirements::BLOCK,
            source_access: SourceAccess::None,
        }
    }

    #[test]
    fn single_index_phase_zero() -> Result<(), DagError> {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE)])?;
        assert_eq!(dag.phase_count(), 1);
        let phases = dag.phases();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].len(), 1);
        assert_eq!(phases[0][0].descriptor.name, A);
        Ok(())
    }

    #[test]
    fn two_independent_same_phase() -> Result<(), DagError> {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE), desc("b", DEPS_NONE)])?;
        assert_eq!(dag.phase_count(), 1);
        let phases = dag.phases();
        assert_eq!(phases[0].len(), 2);
        Ok(())
    }

    #[test]
    fn linear_chain_three_phases() -> Result<(), DagError> {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_A),
            desc("c", DEPS_B),
        ])?;
        assert_eq!(dag.phase_count(), 3);

        let a_node = dag.node(A).expect("a exists");
        let b_node = dag.node(B).expect("b exists");
        let c_node = dag.node(C).expect("c exists");
        assert_eq!(a_node.phase, PhaseIndex::new(0));
        assert_eq!(b_node.phase, PhaseIndex::new(1));
        assert_eq!(c_node.phase, PhaseIndex::new(2));
        Ok(())
    }

    #[test]
    fn diamond_two_phases() -> Result<(), DagError> {
        let dag = DependencyDag::build(vec![
            desc("a", DEPS_NONE),
            desc("b", DEPS_NONE),
            desc("c", DEPS_AB),
        ])?;
        assert_eq!(dag.phase_count(), 2);

        let c_node = dag.node(C).expect("c exists");
        assert_eq!(c_node.phase, PhaseIndex::new(1));
        Ok(())
    }

    #[test]
    fn cycle_detected() {
        let result = DependencyDag::build(vec![desc("a", DEPS_B), desc("b", DEPS_A)]);
        assert!(matches!(result, Err(DagError::CycleDetected { .. })));
    }

    #[test]
    fn unknown_dependency() {
        let result = DependencyDag::build(vec![desc("a", DEPS_MISSING)]);
        assert!(matches!(result, Err(DagError::UnknownDependency { .. })));
    }

    #[test]
    fn duplicate_name() {
        let result = DependencyDag::build(vec![desc("a", DEPS_NONE), desc("a", DEPS_NONE)]);
        assert!(matches!(result, Err(DagError::DuplicateName(_))));
    }

    #[test]
    fn edges_have_pipelined_firing_by_default() -> Result<(), DagError> {
        let dag = DependencyDag::build(vec![desc("a", DEPS_NONE), desc("b", DEPS_A)])?;
        let rule = dag.firing_rule(A, B).expect("edge exists");
        assert_eq!(rule, FiringRule::Pipelined);
        Ok(())
    }
}
