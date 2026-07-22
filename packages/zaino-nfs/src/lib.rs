//! Non-finalised state — the reorg-prone recent window.
//!
//! Adopts Hahn's `zaino-store` reorg machinery, narrowly: an in-memory `Chain`
//! (persistent vector) + `find_trim_index` (Lean-verified reorg) + a pinned
//! snapshot + a side-branch set (Q2), driven by a light tip-follow loop.
//! Internally `im`; no LMDB. Consumers see non-finalised *state*.
//!
//! Scaffold: capability algebra only. Implementations follow.
#![forbid(unsafe_code)]

use std::future::Future;

use futures::stream::BoxStream;

use zaino_core::{
    BlockHash, BlockId, CompactBlock, ForkPoint, Height, Locator, Outpoint, SpendStatus, TipEvent,
};

/// A block that has crossed the freeze horizon (the NFS → FS handoff).
pub type FrozenOut = zaino_core::PreIndexCompactBlock;

/// The non-finalised-state component (the reorg window).
pub trait NonFinalisedState: Send + Sync {
    /// A pinned view (Q1).
    type Snapshot: NfsSnapshot;

    /// Pin the current reorg-window view.
    fn snapshot(&self) -> Self::Snapshot;

    /// Explicit tip-change subscription (drives mempool re-validation etc.).
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent>;

    /// Blocks graduating past the freeze horizon — the runtime forwards each to
    /// the FS component's `freeze`.
    fn frozen(&self) -> BoxStream<'_, FrozenOut>;

    /// Run the tip-follow loop against `source` (a `zaino-source`-shaped
    /// validator port — bounded in the impl). This is where `find_trim_index`
    /// resolves reorgs, one block at a time.
    fn follow<S: Send + Sync>(
        &self,
        source: &S,
    ) -> impl Future<Output = Result<(), NfsError>> + Send;
}

/// A pinned view over the reorg window. Reads are coherent for its lifetime,
/// across reorgs (ADR-0003).
pub trait NfsSnapshot: Clone + Send + Sync {
    /// The pinned tip.
    fn tip(&self) -> BlockId;
    /// The height range this window covers: `[finalised + 1, tip]`.
    fn range(&self) -> (Height, Height);

    fn compact_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Option<CompactBlock>, NfsError>> + Send;
    fn height_of(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, NfsError>> + Send;
    /// Re-derived from the window's blocks (no persistent NFS index).
    fn spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, NfsError>> + Send;
    fn fork_point(
        &self,
        locator: Locator,
    ) -> impl Future<Output = Result<Option<ForkPoint>, NfsError>> + Send;

    // --- side-branch (Q2) ---
    /// All current chain tips, including non-best branches (`getchaintips`).
    fn chain_tips(&self) -> Vec<BlockId>;
}

/// Non-finalised-state errors (placeholder).
#[derive(Debug)]
pub enum NfsError {
    /// Likely to resolve on retry (e.g. a reorg mid-fetch).
    Transient(String),
    /// Unrecoverable.
    Fatal(String),
}
