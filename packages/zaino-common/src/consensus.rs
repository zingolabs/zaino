//! Consensus-derived constants with a single source of truth.
//!
//! Each value derives from zebra's authoritative upstream constant; nothing else in
//! the workspace should hard-code these — reference this module instead.

/// Number of confirmations before a coinbase output becomes spendable.
///
/// Single source of truth, from zebra's transparent-coinbase maturity rule.
pub const COINBASE_MATURITY: u32 = zebra_chain::transparent::MIN_TRANSPARENT_COINBASE_MATURITY;

/// Distance below the best-chain tip of the finalised / non-finalised seam: a block
/// buried deeper than this is finalised (reorg-stable).
///
/// Derived from zebra's protocol reorg limit (`MAX_BLOCK_REORG_HEIGHT`). The `+ 1`
/// accounts for the tip block itself, preserving the historical seam semantics.
pub const MAX_NONFINALISED_DEPTH: u32 =
    zebra_chain::parameters::constants::MAX_BLOCK_REORG_HEIGHT + 1;

/// A tractable one-tenth of [`MAX_NONFINALISED_DEPTH`], for fast tests that need a
/// finalised seam without building a full ~[`MAX_NONFINALISED_DEPTH`]-block chain.
///
/// Integer division, so this is `100` when the real depth is `1001`. Test-only: it
/// lets in-crate tests select a shallow seam that still derives from the single
/// source of truth rather than a hard-coded literal.
pub const FAST_TEST_MAX_NONFINALISED_DEPTH: u32 = MAX_NONFINALISED_DEPTH / 10;
