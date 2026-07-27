//! Zcash index definitions and pre-composed index sets.
//!
//! # Architecture
//!
//! The implementation flow follows the sync engine's trait hierarchy:
//!
//! ## 1. Individual Indexes ([`indexes`])
//!
//! Each index module defines:
//! - A **narrow context type** (`HeaderCtx`) — the minimal per-block
//!   data this index needs to produce its delta.
//! - An **`IndexDef`** impl — pins the scope (BlockLocal, SelfCumulative)
//!   and composition (Append, Monoidal, Fold).
//! - An **extraction function** (`ExtractLocal::extract`) — pure,
//!   produces a delta from the narrow context.
//! - A **`Schema`** impl — maps deltas to key/value entries and
//!   defines encoding/decoding for persistence.
//!
//! Indexes know nothing about the set-wide context or other indexes.
//! They are reusable across different index sets.
//!
//! ## 2. Index Sets ([`sets`])
//!
//! Each set module defines:
//! - A **set-wide context type** (e.g. `HeadersOnlyContext`) — the
//!   union of all data any index in the set might need. The
//!   provisioner produces one of these per block.
//! - **`ProvideContext` impls** — one per index in the set, projecting
//!   the set-wide context into each index's narrow context type.
//! - A **builder function** (`index_set()`) — registers all indexes
//!   and returns a configured `IndexSet`.
//!
//! Different sets can compose the same indexes with different set-wide
//! contexts. A "headers-only" set has a minimal context. A "full" set
//! would carry transaction data too — but the HeadersIndex definition
//! is the same in both; only the `ProvideContext` projection differs.

pub mod indexes;
pub mod sets;
