//! Finalised state — the immutable, append-only side of the chain.
//!
//! Domain semantics over finalised blocks: serve compact blocks + aux lookups,
//! and ingest (bulk-build on boot, freeze one block in steady state). Internally
//! this drives `zaino-sync` + `zaino-indexes` over a `zaino-persistence` backend
//! — but that is hidden; consumers see finalised *state*, not indices.
//!
//! Scaffold: capability algebra only. Implementations follow.
#![forbid(unsafe_code)]

use std::future::Future;

use zaino_core::{
    AddressBalance, BlockHash, CompactBlock, Height, Outpoint, PreIndexCompactBlock, SpendStatus,
    TransactionHash, TransactionLocation, TransparentAddress, Treestate, Utxo,
};

/// A block handed over the freeze boundary (NFS → FS): now final, to be indexed.
/// Carries enough to store the compact block and extract the aux indexes.
pub type FrozenBlock = PreIndexCompactBlock;

/// The finalised-state component. Everything it answers is at or below
/// [`FinalisedState::watermark`] and immutable — so no reorg machinery.
pub trait FinalisedState: Send + Sync {
    /// The finalised tip. Reads are valid for heights `<= watermark`.
    fn watermark(&self) -> Height;

    // --- reads (all <= watermark) ---
    fn compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<CompactBlock>, FsError>> + Send;
    fn height_of(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, FsError>> + Send;
    fn tx_location(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<Option<TransactionLocation>, FsError>> + Send;
    fn spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, FsError>> + Send;
    fn address_balance(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<AddressBalance, FsError>> + Send;
    fn address_unspent(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<Vec<Utxo>, FsError>> + Send;
    fn treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Treestate, FsError>> + Send;

    // --- ingest ---
    /// Bulk-build the finalised state up to `target` (boot catch-up), pulling
    /// from `source` (a `zaino-source`-shaped validator port — bounded in the
    /// impl). This is where the sync engine's parallel pipeline runs.
    fn bulk_build_to<S: Send + Sync>(
        &self,
        target: Height,
        source: &S,
    ) -> impl Future<Output = Result<(), FsError>> + Send;
    /// Extend by one finalised block (steady-state freeze).
    fn freeze(&self, block: FrozenBlock) -> impl Future<Output = Result<(), FsError>> + Send;
}

/// Finalised-state errors (placeholder).
#[derive(Debug)]
pub enum FsError {
    /// Backend I/O or engine failure.
    Backend(String),
    /// The requested height is above the finalised watermark.
    AboveWatermark(Height),
}
