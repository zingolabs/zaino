//! Per-operation errors for the finalised-state component.
//!
//! Distinct types because the operations fail in different ways — height-keyed
//! reads can be above the watermark, address reads can be absent in a
//! deployment that doesn't run the index, and the ingest paths fail on the
//! source or the commit. None of those failures is shared across all methods,
//! so a single error would over-state what each call can return.

use zaino_core::Height;

/// Errors from height-keyed reads (`compact_block`, `treestate`).
#[derive(Debug)]
pub enum HeightReadError {
    /// Backend I/O failure.
    Backend(String),
    /// The requested height is above the finalised watermark.
    AboveWatermark(Height),
}

/// Errors from key→value lookups (`height_of`, `tx_location`, `spend_status`).
/// A miss is `Ok(None)` / a domain answer, never an error here.
#[derive(Debug)]
pub enum LookupError {
    /// Backend I/O failure.
    Backend(String),
}

/// Errors from address-history reads (`address_balance`, `address_unspent`).
#[derive(Debug)]
pub enum AddressReadError {
    /// Backend I/O failure.
    Backend(String),
    /// This deployment does not run the address-history index.
    NotEnabled,
}

/// Errors from the boot-time bulk build (`bulk_build_to`).
#[derive(Debug)]
pub enum BuildError {
    /// The validator source failed.
    Source(String),
    /// A backend commit failed.
    Commit(String),
}

/// Errors from freezing one block (`freeze`).
#[derive(Debug)]
pub enum FreezeError {
    /// A backend commit failed.
    Commit(String),
    /// The block failed continuity/validation at the freeze boundary.
    Invalid(String),
}
