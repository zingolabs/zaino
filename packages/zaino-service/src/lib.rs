//! Zaino's **inner driving surface** — the capability trait algebra.
//!
//! One trait per capability-cohesion unit (≈ one backing index), mirroring the
//! per-method segregation of the driven ports (`zaino-source`) but for the
//! opposite reason: here a *single* implementor (the runtime) provides all of
//! them, and the split serves the *consumer* (each outer client depends only on
//! the subset it needs), plus mocking and per-capability error mapping.
//!
//! Presence is type-level, reach is runtime. A composed snapshot implements a
//! read trait only where its providers can back it under the use case's
//! [`routing`]; a read that exists returns [`error::NotServiceable`](error)
//! only while its backing index is still catching up.
//!
//! Async style follows the consumer stack (zallet): RPITIT (`impl Future +
//! Send`) and `BoxStream`, driven through generics — no `async-trait`, no `dyn`
//! at the fine-grained traits. The [`Snapshot`] / [`IndexerService`] bundles are
//! the single aggregate handles.
#![forbid(unsafe_code)]

mod bundle;
mod controls;
pub mod error;
mod profiles;
mod reads;
pub mod routing;

#[cfg(feature = "testing")]
pub mod conformance;
#[cfg(feature = "testing")]
pub mod testing;

pub use bundle::{ChainSegment, IndexerService, Snapshot};
pub use controls::{
    Broadcast, MempoolContent, MempoolSubscribe, Passthrough, ReportedUpgrades, Serviceable,
    TakeSnapshot, TipSubscribe,
};
pub use profiles::{
    FullWalletReads, LightServeService, LightWalletReads, NodeRpcReads, NodeRpcService,
    WalletLibService, WalletReadCore,
};
pub use reads::{
    AddressRead, BlockRead, ChainInfoRead, CompactBlockRead, CompactNullifierRead, ForkReconcile,
    RawTransactionRead, SpendRead, TransactionRead, TreestateRead,
};
