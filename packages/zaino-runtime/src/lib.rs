//! The Zaino runtime's component supervision.
//!
//! The runtime is the conductor: it boots the long-lived components (validator,
//! finalised state, non-finalised state, mempool) in a dictated order and
//! supervises their health through the `zaino-component` ports.
//!
//! - [`supervise`] / [`supervise_step`] watch one component and act per a
//!   [`RecoveryPolicy`]; the recovery mechanism (restart via `Managed`) is wired
//!   but the current policy escalates rather than restarts.
//! - [`Orchestra`] boots components in order (each `Ready` before the next),
//!   gives each a babysitter, and funnels escalations up one channel; [`run`]
//!   turns the first escalation into a [`RuntimeOutcome`].
//!
//! The read-serving composition over the finalised / non-finalised /
//! validator providers lives in `zaino-store-service`; which provider answers
//! each capability is the use case's `zaino_service::routing::Routing` type,
//! not a policy table here.
//!
//! [`run`]: Orchestra::run
#![forbid(unsafe_code)]

mod health;
mod orchestra;
mod run;
mod run_component;
mod signals;
mod status_log;
mod supervisor;
mod validator;

pub use health::{HealthServeError, HealthServer};
pub use orchestra::{BootError, Orchestra, OrchestraBuilder, RuntimeOutcome};
pub use run_component::RunComponent;
pub use signals::{classify, ReadinessCriteria, RuntimePhase, RuntimeSignals};
pub use supervisor::{observe, supervise, supervise_step, RecoveryPolicy, SupervisionOutcome};
pub use validator::{ValidatorComponent, ValidatorUnreachable};
pub use zaino_component::{ReachabilityProbe, RunLoop};

/// Back-compat alias: an indexer is a [`RunComponent`] over a [`RunLoop`] writer
/// (its `RUNNING` phase is `Syncing`).
pub type IndexerComponent<D> = RunComponent<D>;
/// Back-compat alias: a server is a [`RunComponent`] over a [`RunLoop`] server
/// (its `RUNNING` phase is `Spawning`).
pub type ServeComponent<A> = RunComponent<A>;
