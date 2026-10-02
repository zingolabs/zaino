//! `zaino-core` — the engine every use case is served by.
//!
//! Composes the finalised store, the non-finalised chain head and the
//! validator into one `IndexerService`, **under a routing**. The
//! two chain tiers are named only through the shared `zaino-service` ports
//! and captured together on each pin by [`crate::chain_view::ChainView`], so the
//! seam watermark and the volatile window are coherent; the validator is
//! reached live through [`PassthroughProvider`] over the canonical (resilient)
//! `zaino-source` ports.
//!
//! Which provider answers each capability is a per-deployment choice, not a
//! property of the use case served: a [`Routing`](routing::Routing) type, which
//! [`Engine`] carries as a type parameter: each read trait on
//! [`EngineSnapshot`] is implemented once per placement, bounded on that
//! placement and on the provider ports it needs. A capability the providers
//! cannot back under the chosen routing is an impl that does not exist, so a
//! use case's demand bound fails where the engine is wired — the proofs are
//! in [`Engine`]'s docs.
//!
//! Always local: compact blocks (and their nullifier projection), chain info.
//! Always passthrough: raw transactions, broadcast, mempool, the upgrade schedule.
//! Decided per use case: address history, treestate, spend status, transaction
//! location.
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

pub mod chain_view;
mod engine;
mod nullifiers;
mod passthrough;
mod prevout;
pub mod routing;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use engine::{Engine, EngineSnapshot};
pub use passthrough::PassthroughProvider;

#[cfg(test)]
mod tests;
