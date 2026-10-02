//! Resolved-transaction reads — always passthrough.
//!
//! A transaction view is the explorer shape: the decoded transaction and its
//! detail, with every transparent input resolved to the output it spends. The
//! decoded transaction and the decoded block both come live from the validator,
//! whose chain library does the per-transaction decoding this crate must not
//! depend on; like [`BlockVerboseRead`](super::super::engine::block_verbose),
//! the placement never varies, so there is one impl bounded on the decoded source
//! ports rather than a placement-trait dispatch.
//!
//! Resolution looks first among the transactions already in hand — a block spends
//! its own earlier outputs with no extra fetch — and otherwise fetches the spent
//! transaction through the decoded-transaction port, deduplicated and with
//! bounded concurrency (see [`crate::prevout`]). A miss on the requested
//! transaction or block is `Ok(None)`; a miss on a prevout is a typed source
//! inconsistency, never a blank value.

use crate::chain_view::ChainTier;
use crate::prevout::resolve_prevouts;
use crate::routing::Routing;
use zaino_primitives::types::{BlockSelector, DecodedBlock, Transaction, TransactionId};
use zaino_service::error::TransactionViewError;
use zaino_service::{
    BlockTransactionViews, LocatedTransactionView, TransactionView, TransactionViewRead,
};
use zaino_source::{GetBlockDecoded, GetBlockDecodedByHash, GetTransactionVerbose};

use super::EngineSnapshot;

impl<F, N, Src, R> TransactionViewRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetBlockDecoded + GetBlockDecodedByHash + GetTransactionVerbose + Send + Sync + 'static,
    R: Routing,
{
    async fn transaction_view(
        &self,
        id: TransactionId,
    ) -> Result<Option<LocatedTransactionView>, TransactionViewError> {
        let Some(decoded) = self.passthrough().transaction_decoded(id).await? else {
            return Ok(None);
        };
        // A single fetched transaction has no same-block context, so every prevout
        // is fetched; passing it as a one-element set reuses the block path's
        // resolver and dedup.
        let resolved = resolve_prevouts(std::slice::from_ref(&decoded.transaction), |txid| {
            self.passthrough().prevout_outputs(txid)
        })
        .await?;
        let inputs = resolved.into_iter().next().unwrap_or_default();
        Ok(Some(LocatedTransactionView {
            view: TransactionView {
                transaction: decoded.transaction,
                detail: decoded.detail,
                inputs,
            },
            location: decoded.location,
        }))
    }

    async fn block_transaction_views(
        &self,
        at: BlockSelector,
    ) -> Result<Option<BlockTransactionViews>, TransactionViewError> {
        let decoded = match at {
            BlockSelector::Height(height) => self.passthrough().block_decoded(height).await?,
            BlockSelector::Hash(hash) => self.passthrough().block_decoded_by_hash(hash).await?,
        };
        let Some(DecodedBlock { size, transactions }) = decoded else {
            return Ok(None);
        };
        // Resolve against the block's own transactions so an intra-block spend
        // needs no fetch; the resolver returns one input list per transaction, in
        // block order.
        let shapes: Vec<Transaction> = transactions
            .iter()
            .map(|detailed| detailed.transaction.clone())
            .collect();
        let resolved =
            resolve_prevouts(&shapes, |txid| self.passthrough().prevout_outputs(txid)).await?;
        let views = transactions
            .into_iter()
            .zip(resolved)
            .map(|(detailed, inputs)| TransactionView {
                transaction: detailed.transaction,
                detail: detailed.detail,
                inputs,
            })
            .collect();
        Ok(Some(BlockTransactionViews {
            size,
            transactions: views,
        }))
    }
}
