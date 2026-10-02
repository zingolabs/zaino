//! Query: decode a whole block, every transaction with its detail, from one
//! raw-block fetch.

use std::future::Future;

use zaino_primitives::types::{BlockHash, DecodedBlock, Height};

use super::{QueryError, ValidatorSource};

pub use super::{GetBlockByHashError, GetBlockError};

/// Decode a best-chain block at a height into every transaction with its detail.
///
/// One fetch, one consistent snapshot: the whole block is decoded from the bytes
/// a single `getblock(height, 0)` returns, so the coinbase and the transactions
/// that reference it cannot be read from different chain states. Callers that
/// need only the raw bytes want [`GetRawBlock`](super::GetRawBlock); callers that
/// need the indexing shape want [`GetBlock`](super::GetBlock); this is the
/// explorer surface, whose per-transaction decoding needs the validator's chain
/// library and so lives in the adapter rather than the validator-agnostic core.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetBlockDecoded: ValidatorSource + Send + Sync {
    /// Decode a block by height.
    fn get_block_decoded(
        &self,
        height: Height,
    ) -> impl Future<Output = Result<DecodedBlock, QueryError<GetBlockError, Self::NonDomain>>> + Send;
}

/// Decode a block by hash into every transaction with its detail.
///
/// Separate from [`GetBlockDecoded`] because a height names a best-chain block
/// whereas a hash can name one on a side chain — different questions, which
/// adapters may answer from different places.
#[zaino_source_macros::resilient_port]
pub trait OneShotGetBlockDecodedByHash: ValidatorSource + Send + Sync {
    /// Decode a block by hash.
    fn get_block_decoded_by_hash(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<DecodedBlock, QueryError<GetBlockByHashError, Self::NonDomain>>>
           + Send;
}
