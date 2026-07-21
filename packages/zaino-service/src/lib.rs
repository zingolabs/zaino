//! Zaino's **inner driving surface** — the capability trait algebra.
//!
//! One trait per capability-cohesion unit (≈ one backing index), mirroring the
//! per-method segregation of the driven ports (`zaino-source`) but for the
//! opposite reason: here a *single* implementor (the runtime) provides all of
//! them, and the split serves the *consumer* (each outer client depends only on
//! the subset it needs), plus mocking and per-capability error mapping.
//!
//! Serviceability is a *runtime* property, not type-level: the concrete
//! snapshot implements every read trait, but a read returns
//! [`error::NotServiceable`](error) until its backing index is built.
//!
//! Async style follows the consumer stack (zallet): RPITIT (`impl Future +
//! Send`) and `BoxStream`, driven through generics — no `async-trait`, no `dyn`
//! at the fine-grained traits. The [`Snapshot`] / [`IndexerService`] bundles are
//! the single aggregate handles.
#![forbid(unsafe_code)]

mod bundle;
mod controls;
pub mod error;
mod reads;

#[cfg(feature = "testing")]
pub mod testing;

pub use bundle::{IndexerService, Snapshot};
pub use controls::{
    Broadcast, MempoolSubscribe, ReportedUpgrades, Serviceable, TakeSnapshot, TipSubscribe,
};
pub use reads::{AddressRead, BlockRead, ForkReconcile, SpendRead, TransactionRead, TreestateRead};
