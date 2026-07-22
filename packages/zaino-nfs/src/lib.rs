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
    TransparentAddress, Utxo,
};

/// A block that has crossed the freeze horizon (the NFS → FS handoff).
pub type FrozenOut = zaino_core::PreIndexCompactBlock;

/// The result of pinning the reorg window. Encodes readiness in the type so a
/// consumer *cannot* read a not-yet-synced window (locality of correctness) —
/// there is no `S` to call reads on until the window is established.
pub enum NfsView<S> {
    /// The window is live; `S` is a coherent pinned snapshot.
    Ready(S),
    /// Not established yet (boot catch-up). Recent reads are unavailable; the
    /// finalised state is caught up to `finalised`.
    Syncing {
        /// The finalised height the FS is caught up to.
        finalised: Height,
    },
}

/// The non-finalised-state component (the reorg window).
pub trait NonFinalisedState: Send + Sync {
    /// A pinned view (Q1).
    type Snapshot: NfsSnapshot;

    /// Pin the current reorg-window view. Returns [`NfsView::Syncing`] until the
    /// window is established (boot catch-up) — the not-ready state is in the
    /// type, so recent reads can't be issued against an empty window.
    fn snapshot(&self) -> NfsView<Self::Snapshot>;

    /// Explicit tip-change subscription (drives mempool re-validation etc.).
    fn subscribe_tip(&self) -> BoxStream<'_, TipEvent>;

    /// Blocks graduating past the freeze horizon — the runtime forwards each to
    /// the FS component's `freeze`.
    fn frozen(&self) -> BoxStream<'_, FrozenOut>;

    /// Run the tip-follow loop against `source` (a `zaino-source`-shaped
    /// validator port — bounded in the impl). Where `find_trim_index` resolves
    /// reorgs, one block at a time.
    fn follow<S: Send + Sync>(
        &self,
        source: &S,
    ) -> impl Future<Output = Result<(), FollowError>> + Send;
}

/// A pinned view over the reorg window. Reads are **in-memory over the pinned
/// `Chain`, so they are infallible** — a miss is `None` / a domain answer, not
/// an error. (Contrast the FS component, whose reads hit a backend.) Coherent
/// for the view's lifetime, across reorgs (ADR-0003).
pub trait NfsSnapshot: Clone + Send + Sync {
    /// The pinned tip.
    fn tip(&self) -> BlockId;
    /// The height range this window covers: `[finalised + 1, tip]`.
    fn range(&self) -> (Height, Height);

    fn compact_block(&self, height: Height) -> Option<CompactBlock>;
    fn height_of(&self, hash: BlockHash) -> Option<Height>;
    /// Re-derived from the window's blocks (no persistent NFS index).
    fn spend_status(&self, outpoint: Outpoint) -> SpendStatus;
    fn fork_point(&self, locator: Locator) -> Option<ForkPoint>;

    /// Unspent outpoints for `addr` created within (and still unspent within)
    /// this window — re-derived, infallible. Merged with the FS index by the
    /// runtime for a snapshot-coherent unspent set (US-1.3).
    fn address_unspent(&self, addr: &TransparentAddress) -> Vec<Utxo>;

    // --- side-branch (Q2) ---
    /// All current chain tips, including non-best branches (`getchaintips`).
    fn chain_tips(&self) -> Vec<BlockId>;
}

/// Errors from the tip-follow loop (`follow`).
#[derive(Debug)]
pub enum FollowError {
    /// The validator source failed (retryable).
    Source(String),
    /// A reorg deeper than the window (`find_trim_index` fuel exhausted) —
    /// unresolvable by the loop; needs a resync.
    ReorgTooDeep,
    /// Unrecoverable.
    Fatal(String),
}
