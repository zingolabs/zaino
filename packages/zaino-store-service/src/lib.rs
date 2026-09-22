//! `zaino-store-service` — the concrete inner engine.
//!
//! Composes the finalised store and the non-finalised chain head — both named
//! only through the shared `zaino-service` ports — into one `IndexerService`,
//! the real backend behind the profiles the serve adapters consume.
//!
//! The composition itself lives in [`zaino_chainview::ChainView`]: it pairs a
//! finalised [`TakeSnapshot`](zaino_service::TakeSnapshot) source (`Fs`) with a
//! non-finalised one (`Nfs`), both of whose snapshots are a
//! [`ChainSegment`](zaino_service::ChainSegment) (the coherence coordinate) and a
//! [`CompactBlockRead`](zaino_service::CompactBlockRead) (compact-block serving),
//! and captures both in one shot so the seam watermark and the volatile window
//! are coherent. [`Engine`] wraps that composer and dresses it as the full inner
//! service: it forwards the pin and the compact-block reads to the composed view
//! ([`EngineSnapshot`]), and stands in for the reads and controls the
//! compact-serving slice does not yet source.
//!
//! **Serviceable slice.** Compact-block serving and the coherence surface (pin,
//! coverage, serviceable range, chain info) are wired against the real composed
//! view. Passthrough capabilities are answered by [`RemoteChainView`] over the
//! resilient source ports: `Broadcast` relays a wallet's transaction, and the
//! `Treestate` and transparent-address reads answer live from the validator
//! (zaino does not index them). The remaining reads (full `Block` / transaction /
//! spend / nullifier / subtree roots) return `NotServiceable`; `Passthrough`
//! refuses and the streaming controls (`TipSubscribe`, `MempoolSubscribe`) are
//! empty streams — wiring those through the same provider is the next increment.
//!
//! The classification is *which provider carries a capability*: local ones on the
//! composed [`ChainView`](zaino_chainview::ChainView), passthrough ones on
//! [`RemoteChainView`]. Consumers bind the **canonical** (resilient) source
//! traits, never the raw `OneShot*` ports — those belong to the adapters and the
//! `ValidatorClient` decorator the root injects.
//!
//! The milestone this crate proves is the static assertion in `tests`: the
//! composed engine type-checks as **every** profile (`WalletLibService`,
//! `LightServeService`, `NodeRpcService`) over any two composer inputs.
#![forbid(unsafe_code)]
#![deny(clippy::wildcard_enum_match_arm)]

mod engine;
mod nullifiers;
mod remote;
mod snapshot;

pub use engine::Engine;
pub use remote::RemoteChainView;
pub use snapshot::EngineSnapshot;

#[cfg(test)]
mod tests;
