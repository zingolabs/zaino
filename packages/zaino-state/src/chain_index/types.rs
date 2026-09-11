//! The chain index's type vocabulary, during the move to `zaino-chain-store`.
//!
//! These types are no longer defined here. The persisted shapes and the
//! business-layer primitives they are built from now live in
//! `zaino-chain-store-zainodb`, the backend that writes them, and are
//! re-exported here so the rest of this crate does not have to move at the
//! same time.
//!
//! # This module is scaffolding
//!
//! Re-exporting a backend's internal types is the wrong dependency direction,
//! and it is deliberate and temporary. `ChainIndex` still passes these shapes
//! around its read paths; when it is reworked to read the finalised and recent
//! halves through their own vocabularies, every name here loses its consumer
//! and this module goes with them. Nothing new should be written against it.
//!
//! The wire conversions are the exception: they stay in this crate, in
//! [`super::wire_types`]. A protocol type has no business in a storage crate,
//! and keeping the two apart is what stops a stored shape being served
//! directly or a served shape being written to disk.

pub use zaino_chain_store_zainodb::types::*;
