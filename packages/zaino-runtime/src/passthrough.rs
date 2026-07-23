//! The passthrough seam.
//!
//! For data Zaino doesn't store — full `Block`s, raw transactions — the
//! validator answers directly. Reads are **by hash**: a hash is immutable, so
//! the answer is coherent regardless of reorgs (fetching by *height* against a
//! moving chain is the torn-read hazard we avoid).
//!
//! A generic on `Runtime`/`RuntimeSnapshot`, like the other component seams —
//! static dispatch, native `async`, no boxing.

use std::future::Future;

use zaino_core::{Block, BlockHash, Transaction, TransactionHash};

/// A handle to the validator for passthrough reads.
pub trait Passthrough: Send + Sync {
    fn full_block(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Block>, PassthroughError>> + Send;
    fn raw_transaction(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<Option<Transaction>, PassthroughError>> + Send;
}

/// Passthrough (validator) failures.
#[derive(Debug)]
pub enum PassthroughError {
    /// The validator query failed (likely retryable).
    Source(String),
}
