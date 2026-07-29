//! Pure domain vocabulary for Zaino's inner driving surface.
//!
//! Types only — no async, no runtime. Domain data and identifiers come from
//! `zaino-primitives` (re-exported below); this crate adds the driving-surface
//! vocabulary primitives doesn't carry, and will host the pure logic
//! (fork-point, tx-status, serviceability derivation) later. It never awaits,
//! spawns, or sets status.
#![forbid(unsafe_code)]

// Domain data + identifiers — the real vocabulary, from the zero-dependency crate.
pub use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, CompactBlock, Height, OutputIndex,
    PreIndexCompactBlock, Script, ShieldedPool, SubtreeRoot, Transaction, TransactionHash,
    TransactionLocation, TransparentAddress, Treestate, Utxo, Zatoshis,
};

mod events;
mod locator;
mod refs;
mod serviceability;
mod status;
mod upgrades;

pub use events::{MempoolTx, TipEvent};
pub use locator::{ForkPoint, Locator};
pub use refs::{BlockId, BlockRef, HeightRange, Outpoint};
pub use serviceability::{Capability, ServiceabilityManifest, ServiceableRange};
pub use status::{SpendStatus, TxStatus};
pub use upgrades::{ReportedUpgrade, UpgradeStatus};
