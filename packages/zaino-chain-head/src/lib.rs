//! ChainHead: the bounded, non-finalised head of the chain.
//!
//! ChainHead owns the recent block graph — the canonical chain within a fixed
//! window below the tip, plus the competing branches a validator still knows
//! about — and the reorg handling that keeps it correct. It publishes that as
//! an immutable [`ChainHeadSnapshot`], so a consumer can capture one view and
//! ask it several questions without the chain moving underneath the answers.
//!
//! This crate is the domain half: vocabulary and ports, no runtime and no data
//! structures. The runtime is `zaino-chain-head-service`. Splitting them means
//! a consumer can name what ChainHead answers without depending on the
//! machinery that answers it.
//!
//! # Capabilities, not representations
//!
//! [`ChainHeadSnapshot`] is a trait. Nothing here says how the block graph is
//! stored — that is the publishing runtime's decision, and replacing hash maps
//! with persistent structures must be invisible to every consumer. What is
//! concrete here is vocabulary: a [`ChainHeadBlock`], a [`ChainStateEpoch`], a
//! position, a location. What is abstract is anything that could reasonably be
//! built more than one way.
//!
//! # What ChainHead is not
//!
//! It is not a chain index. It holds a bounded window and nothing below it, it
//! never reads the finalised state, and it never scans history. Complete
//! answers — an address's whole balance, a transaction's full status — combine
//! ChainHead with the finalised state and the mempool, and that combining
//! happens in the consumer.
//!
//! Two consequences follow from the decoupling, and both are visible in the
//! types:
//!
//! - Chainwork is measured from ChainHead's own anchor, not from genesis. It
//!   orders competing branches correctly and is not the absolute value a
//!   validator reports. Hence [`ChainHeadWork`] rather than
//!   `zaino_primitives::types::ChainWork`.
//! - A retained block is a parsed projection, not the consensus bytes. Serving
//!   a raw transaction or raw block from ChainHead is not yet possible; those
//!   queries stay on their existing path.

pub mod block;
pub mod config;
pub mod error;
pub mod ports;
pub mod snapshot;
#[cfg(feature = "transparent_address_history_experimental")]
pub mod transparent;

pub use block::{ChainHeadBlock, ChainHeadWork};
pub use config::ChainHeadConfig;
pub use error::ChainHeadError;
pub use ports::{ChainHeadBlockService, ChainHeadBlockSource, ChainHeadFreezeEvents};
pub use snapshot::{
    ChainHeadBlockIter, ChainHeadSnapshot, ChainHeadTransactionLocations,
    ChainHeadTransactionService, ChainHeadTxPosition, SpenderLocation,
};

#[cfg(feature = "transparent_address_history_experimental")]
pub use snapshot::ChainHeadTransparentHistoryService;
#[cfg(feature = "transparent_address_history_experimental")]
pub use transparent::{
    ChainHeadAddressEffects, LocatedTransparentOutput, LocatedTransparentSpend,
    TransparentHistoryQuery,
};
