//! `zaino-store-service` — the concrete inner engine.
//!
//! Composes the finalised store, the non-finalised chain head and the
//! validator into one `IndexerService`, **under a use case's routing**. The
//! two chain tiers are named only through the shared `zaino-service` ports
//! and captured together on each pin by [`zaino_chainview::ChainView`], so the
//! seam watermark and the volatile window are coherent; the validator is
//! reached live through [`RemoteChainView`] over the canonical (resilient)
//! `zaino-source` ports.
//!
//! Which provider answers each capability is not decided here. It is the
//! use case's [`Routing`](zaino_service::routing::Routing) type, and
//! [`Composed`] carries it as a type parameter: each read trait on
//! [`ComposedSnapshot`] is implemented once per placement, bounded on that
//! placement and on the provider ports it needs. A capability the providers
//! cannot back under the chosen routing is an impl that does not exist, so a
//! use case's profile bound fails where the engine is wired — the proofs are
//! in [`Composed`]'s docs.
//!
//! Always local: compact blocks (and their nullifier projection), chain info.
//! Always remote: raw transactions, broadcast, mempool, the upgrade schedule.
//! Decided per use case: address history, treestate, spend status, transaction
//! location.
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

mod composed;
mod nullifiers;
mod remote;

pub use composed::{Composed, ComposedSnapshot};
pub use remote::RemoteChainView;

#[cfg(test)]
mod tests;
