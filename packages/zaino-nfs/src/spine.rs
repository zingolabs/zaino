//! The reorg-window spine — the always-present core of a pinned NFS view.
//!
//! The window essentials + reorg/side-branch queries. Every pinned view has
//! these. The re-derived transparent facts (spend, address) are facet traits in
//! [`crate::facts`] — mirroring the FS spine/addon split, but in NFS's idiom:
//! **sync + infallible** (in-memory over the pinned `Chain`), not async/fallible.

use zaino_core::{BlockHash, BlockId, CompactBlock, ForkPoint, Height, Locator};

/// A pinned view's spine: tip/range, block lookups, and reorg/side-branch
/// queries. Reads are in-memory over the pinned `Chain`, so **infallible** — a
/// miss is `None`, never an error. Coherent for the view's lifetime (ADR-0003).
pub trait NfsSpine: Clone + Send + Sync {
    /// The pinned tip.
    fn tip(&self) -> BlockId;
    /// The height range this window covers: `[finalised + 1, tip]`.
    fn range(&self) -> (Height, Height);

    fn compact_block(&self, height: Height) -> Option<CompactBlock>;
    fn height_of(&self, hash: BlockHash) -> Option<Height>;
    fn fork_point(&self, locator: Locator) -> Option<ForkPoint>;

    /// All current chain tips, including non-best branches (`getchaintips`).
    fn chain_tips(&self) -> Vec<BlockId>;
}
