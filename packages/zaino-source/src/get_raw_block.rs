//! Query: fetch a block in its consensus-canonical serialized form.

use std::future::Future;

use zaino_primitives::types::{BlockHash, Height};

use super::QueryError;

pub use super::{GetBlockByHashError, GetBlockError};

/// Fetch the raw serialized bytes of a best-chain block at a height.
///
/// The consensus-canonical form: exactly the bytes the block hash commits to,
/// with nothing dropped, reordered or reinterpreted. Callers that need the
/// parsed shape want [`GetBlock`](super::GetBlock); callers that must not lose
/// a field — because they compute a hash, verify work, or build their own index
/// from the block — want this.
///
/// The two are separate traits rather than one method with a flag for the same
/// reason as [`GetBlockHeader`](super::GetBlockHeader) and
/// [`GetRawBlockHeader`](super::GetRawBlockHeader): the caller already knows
/// which form it wants, so the choice belongs in the request rather than in a
/// response the caller must match on.
pub trait GetRawBlock: Send + Sync {
    /// Fetch a serialized block by height.
    fn get_raw_block(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockError>>> + Send;
}

/// Fetch the raw serialized bytes of a block by hash.
///
/// Separate from [`GetRawBlock`] because a height names a best-chain block
/// whereas a hash can name one on a side chain — different questions, which
/// adapters may answer from different places.
pub trait GetRawBlockByHash: Send + Sync {
    /// Fetch a serialized block by hash.
    fn get_raw_block_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockByHashError>>> + Send;
}
