//! Non-finalised state — the reorg-prone recent window.
//!
//! Adopts Hahn's `zaino-store` reorg machinery, narrowly: an in-memory `Chain`
//! (persistent vector) + `find_trim_index` (Lean-verified reorg) + a pinned
//! snapshot + a side-branch set (Q2), driven by a light tip-follow loop.
//! Internally `im`; no LMDB. Consumers see non-finalised *state*.
//!
//! The pinned-view surface is decomposed like the FS side, for modelling
//! clarity: a [`NfsSpine`] (window essentials + reorg queries) plus re-derived
//! [`facts`] facets ([`NfsSpendFacts`], [`NfsAddressFacts`]). [`NfsSnapshot`] is
//! the convenience bundle. Unlike FS, the facets are re-derived on demand (the
//! window is tiny and reorg-churny), not persistent indexes — see [`facts`].
//!
//! Scaffold: capability algebra only. Implementations follow.
#![forbid(unsafe_code)]

pub mod facts;
mod spine;

use std::future::Future;

use futures::stream::BoxStream;

use zaino_core::{Height, TipEvent};

pub use facts::{NfsAddressFacts, NfsSpendFacts};
pub use spine::NfsSpine;

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

/// A pinned view over the reorg window: the full surface = spine + every recent
/// facet. Reads are **in-memory over the pinned `Chain`, so infallible** — a
/// miss is `None`, not an error (contrast the FS component, whose reads hit a
/// backend). Coherent for the view's lifetime, across reorgs (ADR-0003).
///
/// A convenience bundle for a full deployment; per-capability consumers should
/// bound on exactly the spine/facet traits they use (mirroring
/// `zaino_fs::FinalisedState`), so a variant that omits a facet is a
/// compile-time subset.
pub trait NfsSnapshot: NfsSpine + NfsSpendFacts + NfsAddressFacts {}

impl<T: NfsSpine + NfsSpendFacts + NfsAddressFacts> NfsSnapshot for T {}

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
