//! Query: fetch a block header in its serialised form.

use std::future::Future;

use zaino_primitives::types::BlockHash;

use super::QueryError;

pub use super::GetBlockHeaderError;

/// Fetch the raw serialised bytes of a block header.
///
/// The consensus-canonical form, carrying no validator-derived state. Callers
/// wanting confirmations, difficulty or neighbouring hashes want
/// [`GetBlockHeader`](super::GetBlockHeader) instead.
///
/// Maps to `getblockheader(hash, verbose = false)` over JSON-RPC.
pub trait GetRawBlockHeader: Send + Sync {
    /// Fetch a serialised block header.
    fn get_raw_block_header(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Vec<u8>, QueryError<GetBlockHeaderError>>> + Send;
}
