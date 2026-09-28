//! The Zaino runtime: deployments, and the supervision they run under.
//!
//! Two halves. The **deployments** ([`deployment`]) say how each use case is
//! served — the routing the engine is composed under, the index set the store
//! builds, the config its assembly consumes and what its readiness gates on —
//! and [`boot_indexed`] brings one up. The **supervision** is the conductor:
//! it boots the long-lived components (validator, finalised store, chain head,
//! server) in a dictated order and supervises their health through the
//! `zaino-component` ports.
//!
//! - [`supervise`] / [`supervise_step`] watch one component and act per a
//!   [`RecoveryPolicy`]; the recovery mechanism (restart via `Managed`) is wired
//!   but the current policy escalates rather than restarts.
//! - [`Orchestra`] boots components in order (each `Ready` before the next),
//!   gives each a babysitter, and funnels escalations up one channel; [`run`]
//!   turns the first escalation into a [`RuntimeOutcome`].
//!
//! The read-serving composition itself is `zaino-core`'s [`Engine`](zaino_core::Engine);
//! which provider answers each capability is the deployment's
//! `zaino_core::routing::Routing` type, not a policy table here.
//!
//! [`run`]: Orchestra::run
#![forbid(unsafe_code)]

mod boot;
pub mod config;
pub mod deployment;
mod health;
mod orchestra;
mod plan;
mod run;
mod run_component;
mod signals;
mod status_log;
mod supervisor;
mod validator;

pub use boot::{boot_indexed, DeployError, IndexedEngine};
pub use deployment::{compose, Deployment, DeploymentEngine, IndexedSource};
pub use health::{HealthServeError, HealthServer};
pub use orchestra::{BootError, Orchestra, OrchestraBuilder, RuntimeOutcome};
pub use plan::RuntimePlan;
pub use run_component::RunComponent;
pub use signals::{classify, ReadinessCriteria, RuntimePhase, RuntimeSignals};
pub use supervisor::{observe, supervise, supervise_step, RecoveryPolicy, SupervisionOutcome};
pub use validator::{ValidatorComponent, ValidatorUnreachable};
pub use zaino_component::{ReachabilityProbe, RunLoop};
