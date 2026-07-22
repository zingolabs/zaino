//! Finalised state — the immutable, append-only side of the chain.
//!
//! Domain semantics over finalised blocks: serve compact blocks + aux lookups,
//! and ingest (bulk-build on boot, freeze one block in steady state). Internally
//! this drives `zaino-sync` + `zaino-indexes` over a `zaino-persistence` backend
//! — but that is hidden; consumers see finalised *state*, not indices.
//!
//! Scaffold: capability algebra only. Implementations follow.
#![forbid(unsafe_code)]

pub mod error;

use std::future::Future;

use zaino_core::{
    AddressBalance, BlockHash, CompactBlock, Height, Outpoint, PreIndexCompactBlock, SpendStatus,
    TransactionHash, TransactionLocation, TransparentAddress, Treestate, Utxo,
};

use error::{AddressReadError, BuildError, FreezeError, HeightReadError, LookupError};

/// A block handed over the freeze boundary (NFS → FS): now final, to be indexed.
/// Carries enough to store the compact block and extract the aux indexes.
pub type FrozenBlock = PreIndexCompactBlock;

/// The finalised-state component. Everything it answers is at or below
/// [`FinalisedState::watermark`] and immutable — so no reorg machinery. Each
/// method carries the error type appropriate to *its* failure modes.
pub trait FinalisedState: Send + Sync {
    /// The finalised tip. Reads are valid for heights `<= watermark`. Infallible.
    fn watermark(&self) -> Height;

    // --- height-keyed reads (can be above the watermark) ---
    fn compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<CompactBlock>, HeightReadError>> + Send;
    fn treestate(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Treestate, HeightReadError>> + Send;

    // --- lookups (miss is Ok, only the backend can fail) ---
    fn height_of(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, LookupError>> + Send;
    fn tx_location(
        &self,
        txid: TransactionHash,
    ) -> impl Future<Output = Result<Option<TransactionLocation>, LookupError>> + Send;
    fn spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, LookupError>> + Send;

    // --- address-history reads (may be not-enabled in this deployment) ---
    fn address_balance(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<AddressBalance, AddressReadError>> + Send;
    fn address_unspent(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<Vec<Utxo>, AddressReadError>> + Send;

    // --- ingest ---
    /// Bulk-build the finalised state up to `target` (boot catch-up), pulling
    /// from `source` (a `zaino-source`-shaped validator port — bounded in the
    /// impl). Where the sync engine's parallel pipeline runs.
    fn bulk_build_to<S: Send + Sync>(
        &self,
        target: Height,
        source: &S,
    ) -> impl Future<Output = Result<(), BuildError>> + Send;
    /// Extend by one finalised block (steady-state freeze).
    fn freeze(&self, block: FrozenBlock) -> impl Future<Output = Result<(), FreezeError>> + Send;
}
