//! Runtime configuration consulted by the read-composition policy.

use std::collections::HashSet;

use zaino_core::Capability;

/// The set of optional, index-backed capabilities a deployment serves.
///
/// A thin newtype over a std set: the representation stays ours, and the
/// mutation surface stays narrow. It is populated **only** through the
/// assembler's type-gated `serving_*` methods (`insert` is crate-internal), so
/// it can never name a capability the components can't back. Both the manifest
/// and the reads consult it, so *advertised* and *answerable* can't drift.
#[derive(Clone, Default)]
pub struct CapabilitySet(HashSet<Capability>);

impl CapabilitySet {
    pub(crate) fn insert(&mut self, cap: Capability) {
        self.0.insert(cap);
    }

    pub(crate) fn contains(&self, cap: Capability) -> bool {
        self.0.contains(&cap)
    }
}

/// Deployment configuration that shapes read composition — whether validator
/// passthrough is permitted, and (later) which capabilities this deployment
/// serves at all.
pub struct RuntimeConfig {
    /// Whether reads may passthrough to the validator for data not stored
    /// locally (full blocks, raw transactions).
    pub passthrough_enabled: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            passthrough_enabled: true,
        }
    }
}

impl RuntimeConfig {
    /// Whether this deployment serves `cap` at all (capability-based storage —
    /// some deployments run only a subset of indexes). All enabled for now.
    #[allow(dead_code)] // consulted by resolve::passthrough_allowed (passthrough policy)
    pub(crate) fn capability_enabled(&self, _cap: Capability) -> bool {
        true
    }
}
