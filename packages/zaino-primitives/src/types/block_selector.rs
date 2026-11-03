//! How a caller names the block it wants.

use crate::types::{BlockHash, Height};

/// A block, named for a read: by its height on the best chain or by its hash.
///
/// The two are not interchangeable to a server. A height is routable — a
/// serving tier knows whether it holds that height before it looks — while a
/// hash has to be resolved to find out which tier, if any, holds the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockSelector {
    /// The block at this height on the best chain.
    Height(Height),
    /// The block with this hash, on any chain the server retains.
    Hash(BlockHash),
}
