//! The tip the view serves from: the header chain's most-work verified block (`chainview.md` §2),
//! and the trusted validators whose chains hold it
//!
//! ```text
//!   VerifiedChain best ──┐
//!                        ├─▶ ChainTip { block, held_by } ─▶ sync follows it, mempool keyed by it
//!   getblockhash answers ┘        (held_by = ∅ → no tip: nothing proves the block is valid, §4)
//! ```

use zaino_primitives::types::BlockRef;

use crate::endpoints::EndpointSet;

/// The verified best block + every trusted validator whose chain holds it (its tip or below)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainTip {
    pub block: BlockRef,
    pub held_by: EndpointSet,
}

/// No tip to serve from (fail closed: maps to `UNAVAILABLE`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unserved {
    #[error("no verified header chain tip yet")]
    NoBestTip,
    #[error("no trusted validator holds the verified tip {height} (of {configured} configured)")]
    NotHeld { height: u32, configured: usize },
}
