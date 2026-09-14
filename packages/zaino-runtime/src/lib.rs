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
//! The read-serving composition over the finalised / non-finalised / mempool
//! components (route/merge/serviceability) is re-seamed onto dev's
//! `zaino-chain-store` / `zaino-chain-head` / `zaino-mempool` ports separately —
//! it is the primitives-heavy half and lands after the primitives settle.
//!
//! [`run`]: Orchestra::run
#![forbid(unsafe_code)]

mod orchestra;
mod supervisor;

pub use orchestra::{BootError, Orchestra, OrchestraBuilder, RuntimeOutcome};
pub use supervisor::{supervise, supervise_step, RecoveryPolicy, SupervisionOutcome};
