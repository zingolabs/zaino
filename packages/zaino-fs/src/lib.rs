//! Finalised state — the immutable, append-only side of the chain.
//!
//! Domain semantics over finalised blocks: serve compact blocks + aux lookups,
//! and ingest (bulk-build on boot, freeze one block in steady state). Internally
//! this drives `zaino-sync` + `zaino-indexes` over a `zaino-persistence` backend
//! — but that is hidden; consumers see finalised *state*, not indices.
//!
//! The surface is decomposed by *provenance* so deployment variants compose by
//! subset:
//! - [`FinalisedSpine`] — always present: the block store + intrinsic
//!   derivations + ingest.
//! - [`indexes`] — addon reverse indexes ([`TxLocationIndex`], [`SpendIndex`],
//!   [`AddressIndex`]), each built only if the deployment serves it.
//! - [`FinalisedState`] — the convenience bundle of all of them, for a full
//!   deployment. Per-capability consumers should bound on exactly the
//!   spine/addon traits they use, so a variant that omits an index is a
//!   compile-time subset, not a runtime miss.
//!
//! Scaffold: capability algebra only. Implementations follow.
#![forbid(unsafe_code)]

pub mod error;
pub mod indexes;
mod spine;

pub use indexes::{AddressIndex, SpendIndex, TxLocationIndex};
pub use spine::{FinalisedSpine, FrozenBlock};

/// The full finalised-state surface: spine + every addon index. A convenience
/// bundle for deployments that build the complete index set. The blanket impl
/// makes any type providing the parts a `FinalisedState` — but prefer bounding
/// on the specific spine/addon traits a consumer actually uses, so omitting an
/// index yields a compile-time-smaller capability set rather than a runtime miss.
pub trait FinalisedState: FinalisedSpine + TxLocationIndex + SpendIndex + AddressIndex {}

impl<T: FinalisedSpine + TxLocationIndex + SpendIndex + AddressIndex> FinalisedState for T {}
