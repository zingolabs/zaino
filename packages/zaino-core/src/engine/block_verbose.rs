//! Verbose block reads — always passthrough.
//!
//! The chain-position overlay on a block — confirmations, difficulty, chainwork
//! and the neighbouring block hashes — is cumulative chain state the validator
//! derives, not facts in the stored block, so it can only come from the
//! validator. Like the full [`block`](super::super::engine::block) read, its
//! placement never varies: there is no placement-trait dispatch, just one impl
//! bounded on the verbose source ports.
//!
//! A [`BlockSelector`] routes to the by-height or by-hash verbose port; the
//! header read is hash-addressed, matching both the source port and
//! `getblockheader`. A domain miss is `Ok(None)`; an unreachable validator is a
//! transient error, never a silent miss.

use crate::chain_view::ChainTier;
use crate::routing::Routing;
use zaino_primitives::types::rpc::BlockHeaderVerbose;
use zaino_primitives::types::{BlockHash, BlockSelector, BlockVerbose};
use zaino_service::BlockVerboseRead;
use zaino_service::error::BlockReadError;
use zaino_source::{
    GetBlockHeader, GetBlockVerbose, GetBlockVerboseByHash, GetRawBlock, GetRawBlockByHash,
};

use super::EngineSnapshot;

impl<F, N, Src, R> BlockVerboseRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetBlockHeader
        + GetBlockVerbose
        + GetBlockVerboseByHash
        + GetRawBlock
        + GetRawBlockByHash
        + Send
        + Sync
        + 'static,
    R: Routing,
{
    async fn block_header_verbose(
        &self,
        hash: BlockHash,
    ) -> Result<Option<BlockHeaderVerbose>, BlockReadError> {
        self.passthrough().block_header_verbose(hash).await
    }

    async fn block_verbose(
        &self,
        at: BlockSelector,
    ) -> Result<Option<BlockVerbose>, BlockReadError> {
        match at {
            BlockSelector::Height(height) => self.passthrough().block_verbose(height).await,
            BlockSelector::Hash(hash) => self.passthrough().block_verbose_by_hash(hash).await,
        }
    }

    async fn raw_block(&self, at: BlockSelector) -> Result<Option<Vec<u8>>, BlockReadError> {
        match at {
            BlockSelector::Height(height) => self.passthrough().raw_block(height).await,
            BlockSelector::Hash(hash) => self.passthrough().raw_block_by_hash(hash).await,
        }
    }
}
