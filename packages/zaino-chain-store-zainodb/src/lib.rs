//! ZainoDB: Zaino's LMDB-backed finalised chain store.
//!
//! One implementation of the `zaino-chain-store` ports, and the on-disk
//! vocabulary it is built from. A consumer should name the ports, not this
//! crate — the point of the split is that a different store can be substituted
//! by satisfying the same traits.
//!
//! # Everything here is a persisted shape
//!
//! The types in [`types`] are what this backend writes to disk. Their layouts
//! are a compatibility contract with every database already written, so they
//! change only by adding a body-format version and leaving the old decoder in
//! place. Each type sits with its own encoding and the golden test that pins
//! its bytes, so a change to a shape and a change to what that shape serialises
//! to cannot land apart.
//!
//! They are also **this backend's** shapes, not the domain's. The ports speak
//! `zaino-chain-store` and `zaino-primitives` types; these are converted at
//! that boundary. Nothing outside this crate should depend on them, and what is
//! currently re-exported for `zaino-state` is a migration measure with an end
//! date, not an interface.
//!
//! # Why the on-disk types are not the domain types
//!
//! They are lossy on purpose. A stored transparent output holds a 20-byte
//! address key and a value, not the locking script; a stored block holds the
//! fields an index reads, not the bytes the block hash commits to. That is what
//! makes the database a fraction of the chain's size, and it is why raw blocks
//! and raw transactions are served from the validator instead.

#[cfg(test)]
mod golden;

#[cfg(any(test, feature = "testing"))]
pub mod tests;

pub mod config;

/// The ZainoDB-specific half of a store's configuration.
///
/// Re-exported flat because a consumer wiring a store names it beside
/// [`zaino_chain_store::ChainStoreConfig`], and the two reading at different
/// depths would suggest one is the more important half. Neither is.
pub use config::ZainoDbConfig;
pub mod adapter;
pub mod conversion;
pub mod entry;
pub mod error;
#[cfg(feature = "prometheus")]
pub mod metric_names;
pub mod pool;
pub mod store;
pub mod stream;
pub(crate) mod support;
pub mod types;
