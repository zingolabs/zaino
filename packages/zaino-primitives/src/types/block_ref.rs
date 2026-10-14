//! Block named by both hash and height

use crate::types::BlockHash;
use zcash_protocol::consensus::BlockHeight;

/// Which block a value was computed against (a range answered, the tip a mempool set was read at)
///
/// - Named fields, not a tuple (`tip.hash != other.hash`, never the wrong `.0`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub hash: BlockHash,
    pub height: BlockHeight,
}
