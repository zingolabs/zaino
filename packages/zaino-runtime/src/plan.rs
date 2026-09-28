//! How a deployment runs: the config it consumes and what "ready" means for it.

use crate::deployment::Deployment;
use crate::signals::ReadinessCriteria;

/// The runtime side of a [`Deployment`]: which assembly it boots through and
/// what its readiness gates on.
///
/// A deployment is a static shape — use case, routing, index set. A plan is
/// what the runtime needs beyond that to bring it up: the typed config
/// sections its assembly consumes (`Config` names the assembly; a deployment
/// with a local index takes
/// [`IndexedDeploymentConfig`](crate::config::IndexedDeploymentConfig) and
/// boots through [`boot_indexed`](crate::boot_indexed)) and the readiness
/// criteria its components are judged by (a deployment that serves only by
/// passthrough does not gate on a sync it never runs).
pub trait RuntimePlan: Deployment {
    /// The config sections this deployment's assembly consumes.
    type Config: Send + Sync + 'static;

    /// What "ready" requires of this deployment's components.
    const READINESS: ReadinessCriteria;
}
