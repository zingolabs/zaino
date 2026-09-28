//! Zaino's **inner driving surface** — the capability trait algebra.
//!
//! One trait per capability-cohesion unit (≈ one backing index), mirroring the
//! per-method segregation of the driven ports (`zaino-source`) but for the
//! opposite reason: here a *single* implementor (the runtime) provides all of
//! them, and the split serves the *consumer* (each outer client depends only on
//! the subset it needs), plus mocking and per-capability error mapping.
//!
//! Presence is type-level, reach is runtime. An engine's snapshot implements a
//! read trait only where its providers can back it under the routing it is
//! composed under (a supply-side choice, made in `zaino-core`); a read that
//! exists returns [`error::NotServiceable`](error) only while its backing
//! index is still catching up.
//!
//! Async style follows the consumer stack (zallet): RPITIT (`impl Future +
//! Send`) and `BoxStream`, driven through generics — no `async-trait`, no `dyn`
//! at the fine-grained traits. The [`Snapshot`] / [`IndexerService`] bundles are
//! the single aggregate handles.
#![forbid(unsafe_code)]

mod bundle;
mod capability;
mod chain_info;
mod controls;
pub mod error;
mod events;
mod locator;
mod node_query;
mod read_sets;
mod reads;
mod status;
mod upgrades;
pub mod use_cases;

#[cfg(feature = "testing")]
pub mod conformance;
#[cfg(feature = "testing")]
pub mod testing;

pub use bundle::{ChainSegment, IndexerService, ServiceableRange, Snapshot};
pub use capability::{Answerable, Capability, ServiceabilityManifest};
pub use chain_info::ChainInfo;
pub use controls::{
    Broadcast, MempoolContent, MempoolSubscribe, NodeQueryRelay, ReportedUpgrades, Serviceable,
    TakeSnapshot, TipSubscribe,
};
pub use events::{MempoolTx, TipEvent};
pub use locator::{ForkPoint, Locator};
pub use node_query::{NodeQuery, NodeQueryAnswer};
pub use read_sets::{FullWalletReads, LightWalletReads, NodeRpcReads, WalletReadCore};
pub use reads::{
    AddressRead, BlockRead, ChainInfoRead, CompactBlockRead, CompactNullifierRead, ForkReconcile,
    RawTransactionRead, SpendRead, TransactionRead, TreestateRead,
};
pub use status::{SpendStatus, TxStatus};
pub use upgrades::{ReportedUpgrade, UpgradeStatus};
pub use use_cases::{FullWalletService, LightWalletService, NodeRpcService, Serves, UseCase};
