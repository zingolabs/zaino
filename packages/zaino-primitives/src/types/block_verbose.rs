//! Verbose block metadata (cumulative chain state).

use super::{ChainWork, Confirmations, Difficulty};

/// Verbose block metadata not present in the raw block bytes.
///
/// Fields that require cumulative chain state (chainwork,
/// confirmations) live here; fields derivable from the raw block
/// (header, transactions) do not.
#[derive(Debug, Clone)]
pub struct BlockVerbose {
    /// Cumulative chainwork at this block.
    pub chainwork: ChainWork,
    /// Current difficulty target.
    pub difficulty: Difficulty,
    /// Number of confirmations (depth from tip).
    pub confirmations: Confirmations,
}
