//! Runtime configuration consulted by the read-composition policy.

use zaino_core::Capability;

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
