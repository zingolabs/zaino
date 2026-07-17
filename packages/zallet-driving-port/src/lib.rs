//! Zallet driving port — the contract through which consumers drive Zaino.
//!
//! This crate defines the driving port whose language is settled in
//! `docs/driving-port/CONTEXT.md`. The port spans snapshot-pinned chain
//! reads, an explicit chain tip-change subscription, a mempool stream that
//! stands apart from chain state
//! (`docs/driving-port/0001-decouple-mempool-from-chain-state.md`), and
//! transaction broadcast. Payloads cross the boundary as consensus-serialized
//! bytes; only identifiers and locators are typed, drawn from
//! `zaino-primitives`
//! (`docs/driving-port/0002-raw-bytes-at-the-driving-port.md`).
//!
//! Zallet is the port's first driver, and zainod-for-lightclients is the
//! expected second; the v1 method set mirrors what Zallet's `Chain` and
//! `ChainView` traits demand. The port's first implementation is a
//! scriptable in-memory mock, shipped behind the `testing` feature together
//! with a conformance test-kit that every real implementation must pass.

#![forbid(unsafe_code)]

mod block_id;
mod block_locator;
mod broadcast_transaction;
mod driving_port;
mod error;
mod find_fork_point;
mod get_address_transaction_ids;
mod get_address_unspent_outpoints;
mod get_health;
mod get_mined_transaction;
mod get_outpoint_spend_status;
mod get_raw_block;
mod get_raw_block_header;
mod get_reported_upgrades;
mod get_transaction_status;
mod get_treestate;
mod hash_for_height;
mod height_for_hash;
mod mempool_transaction;
mod mined_transaction;
mod outpoint;
mod pinned_tip;
mod raw;
mod reported_upgrade;
mod shut_down;
mod snapshot;
mod spend_status;
mod stream_raw_blocks;
mod subscribe_to_mempool;
mod subscribe_to_tip_changes;
mod take_snapshot;
mod transaction_status;
mod treestate_at;

#[cfg(any(test, feature = "testing"))]
pub mod conformance;
#[cfg(any(test, feature = "testing"))]
pub mod mock;

pub use block_id::BlockId;
pub use block_locator::{BlockLocator, BlockLocatorError};
pub use broadcast_transaction::{BroadcastTransaction, BroadcastTransactionError};
pub use driving_port::DrivingPort;
pub use error::{BackendError, FailureClass, PortError};
pub use find_fork_point::{FindForkPoint, FindForkPointError};
pub use get_address_transaction_ids::{GetAddressTransactionIds, GetAddressTransactionIdsError};
pub use get_address_unspent_outpoints::{
    GetAddressUnspentOutpoints, GetAddressUnspentOutpointsError,
};
pub use get_health::{GetHealth, GetHealthError, Health};
pub use get_mined_transaction::{GetMinedTransaction, GetMinedTransactionError};
pub use get_outpoint_spend_status::{GetOutpointSpendStatus, GetOutpointSpendStatusError};
pub use get_raw_block::{GetRawBlock, GetRawBlockError};
pub use get_raw_block_header::{GetRawBlockHeader, GetRawBlockHeaderError};
pub use get_reported_upgrades::{GetReportedUpgrades, GetReportedUpgradesError};
pub use get_transaction_status::{GetTransactionStatus, GetTransactionStatusError};
pub use get_treestate::{GetTreestate, GetTreestateError};
pub use hash_for_height::{GetHashForHeight, GetHashForHeightError};
pub use height_for_hash::{GetHeightForHash, GetHeightForHashError};
pub use mempool_transaction::MempoolTransaction;
pub use mined_transaction::MinedTransaction;
pub use outpoint::Outpoint;
pub use pinned_tip::GetPinnedTip;
pub use raw::{RawBlock, RawBlockHeader, RawTransaction, RawTreeFrontier};
pub use reported_upgrade::{ReportedUpgrade, UpgradeStatus};
pub use shut_down::ShutDown;
pub use snapshot::ChainSnapshot;
pub use spend_status::SpendStatus;
pub use stream_raw_blocks::{StreamRawBlocks, StreamRawBlocksError};
pub use subscribe_to_mempool::SubscribeToMempool;
pub use subscribe_to_tip_changes::SubscribeToTipChanges;
pub use take_snapshot::{TakeSnapshot, TakeSnapshotError};
pub use transaction_status::TransactionStatus;
pub use treestate_at::TreestateAt;
