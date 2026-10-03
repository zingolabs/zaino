//! The composed `getblockdeltas` read — a block's transparent value movements.
//!
//! `getblockdeltas` is indexer-only: Zebra answers `-32601`, so the shape
//! authority is zcashd's `blockToDeltasJSON`. This read **composes** it from the
//! reads the engine already serves — it adds no source port:
//!
//! * the block header, for the block's own fields ([`BlockRead::block`]);
//! * the chain-position overlay (confirmations, difficulty, chainwork, next hash)
//!   ([`BlockVerboseRead::block_verbose`]);
//! * the resolved transactions, every transparent input paired with the output it
//!   spends ([`TransactionViewRead::block_transaction_views`]);
//! * the predecessor header times the median time is taken over, read **locally**
//!   over the composed chain view ([`HeaderRead::header`]).
//!
//! A by-height request resolves the height to a hash once (ruling R50) — from the
//! block read — and issues the chain-position and transaction reads by that hash,
//! so the three reads cannot straddle a tip reorg between them. A miss on the
//! block is the domain not-found (`Ok(None)`); a subset present is a reorg race
//! surfaced as a transient failure, never a partially composed block.
//!
//! The median time mirrors zcashd's `GetMedianTimePast`: the median of the
//! block's own time and its ten predecessors (heights `h-10 ..= h`, or fewer near
//! genesis), over local header reads. A predecessor header missing at or below the
//! tip is a chain-view hole, failed loud rather than inventing a median.

use crate::chain_view::ChainTier;
use crate::routing::Routing;
use zaino_consensus::block_time::median_time_past;
use zaino_primitives::types::{BlockSelector, BlockTime, Height, SignedZatoshis, Zatoshis};
use zaino_service::error::{BlockDeltasError, BlockReadError};
use zaino_service::{
    BlockDeltas, BlockDeltasRead, BlockRead, BlockTransactionViews, BlockVerboseRead, HeaderRead,
    InputDelta, OutputDelta, TransactionDeltas, TransactionView, TransactionViewRead,
};
use zaino_source::{
    GetBlock, GetBlockByHash, GetBlockDecoded, GetBlockDecodedByHash, GetBlockHeader,
    GetBlockVerbose, GetBlockVerboseByHash, GetRawBlock, GetRawBlockByHash, GetTransactionVerbose,
};

use super::EngineSnapshot;

/// The median-time window: the block itself plus its ten predecessors, matching
/// zcashd's `nMedianTimeSpan`.
const MEDIAN_TIME_SPAN: u32 = 11;

impl<F, N, Src, R> BlockDeltasRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: GetBlock
        + GetBlockByHash
        + GetBlockHeader
        + GetBlockVerbose
        + GetBlockVerboseByHash
        + GetRawBlock
        + GetRawBlockByHash
        + GetBlockDecoded
        + GetBlockDecodedByHash
        + GetTransactionVerbose
        + Clone
        + Send
        + Sync
        + 'static,
    R: Routing,
{
    async fn block_deltas(
        &self,
        at: BlockSelector,
    ) -> Result<Option<BlockDeltas>, BlockDeltasError> {
        // The block carries the header fields; a miss is the domain not-found.
        let block = self.block(at).await?;
        // Resolve the height to a hash once (R50): the chain-position and
        // transaction reads then name the one block the header came from, so they
        // cannot straddle a tip reorg between them. With no block, the original
        // selector still drives the subset/not-found detection below.
        let contents = match &block {
            Some(block) => BlockSelector::Hash(block.header.hash),
            None => at,
        };
        let verbose = self.block_verbose(contents).await?;
        let views = self.block_transaction_views(contents).await?;
        // All three present is a block to compose; all absent a genuine miss; a
        // subset present is a reorg race between the reads — transient, never a
        // partial render.
        let (block, verbose, views) = match (block, verbose, views) {
            (Some(block), Some(verbose), Some(views)) => (block, verbose, views),
            (None, None, None) => return Ok(None),
            _ => {
                return Err(BlockDeltasError::Block(BlockReadError::Transient(format!(
                    "block {at:?} and its contents disagree; retry"
                ))));
            }
        };

        let header = block.header;
        let median_time = self.median_time(header.height).await?;
        let deltas = transaction_deltas(&views)?;

        Ok(Some(BlockDeltas {
            hash: header.hash,
            confirmations: verbose.confirmations,
            size: views.size,
            height: header.height,
            version: header.version,
            merkle_root: header.merkle_root,
            deltas,
            time: header.time,
            median_time,
            nonce: header.nonce,
            bits: header.bits,
            difficulty: verbose.difficulty,
            chainwork: verbose.chainwork,
            // Genesis has no predecessor; the tip (and a side-chain block) has no
            // next — mirroring `getblock`.
            prev_hash: (u32::from(header.height) != 0).then_some(header.prev_hash),
            next_hash: verbose.next_block_hash,
        }))
    }
}

impl<F, N, Src, R> EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Clone + Send + Sync + 'static,
    R: Routing,
{
    /// zcashd's `GetMedianTimePast` for the block at `height`: the median of the
    /// block's own time and its ten predecessors' times (heights `h-10 ..= h`, or
    /// fewer near genesis), read locally over the composed chain view.
    ///
    /// Every height in the window is at or below the block's own, and so at or
    /// below the pinned tip, where a block must exist: a missing header is a
    /// chain-view hole, failed loud rather than narrowing the window and shifting
    /// the median.
    async fn median_time(&self, height: Height) -> Result<BlockTime, BlockDeltasError> {
        let view = self.local();
        let top = u32::from(height);
        let bottom = top.saturating_sub(MEDIAN_TIME_SPAN - 1);
        let mut times: Vec<BlockTime> = Vec::new();
        for raw in bottom..=top {
            let Ok(window_height) = Height::try_from(raw) else {
                continue;
            };
            let summary =
                view.header(window_height)
                    .await?
                    .ok_or(BlockDeltasError::MissingHeader {
                        height: window_height,
                    })?;
            times.push(summary.time);
        }
        // The window always includes the block's own height, so `times` is never
        // empty; the `MissingHeader` fallback is defensive, not a reachable path.
        median_time_past(&times).ok_or(BlockDeltasError::MissingHeader { height })
    }
}

/// The per-transaction transparent deltas of a resolved block, in block order,
/// mirroring zcashd's `blockToDeltasJSON` loop.
fn transaction_deltas(
    views: &BlockTransactionViews,
) -> Result<Vec<TransactionDeltas>, BlockDeltasError> {
    views
        .transactions
        .iter()
        .zip(0u32..)
        .map(|(view, index)| {
            Ok(TransactionDeltas {
                txid: view.transaction.txid,
                index,
                inputs: input_deltas(view)?,
                outputs: output_deltas(view),
            })
        })
        .collect()
}

/// One transaction's input deltas. A coinbase has none — zcashd skips the inputs
/// of a coinbase — matching the serve adapter's `vin` rendering, which also keys
/// coinbase-ness off the detail rather than block position. Every other input is
/// the negation of the value of the output it spends, carrying that output's
/// script and the spent outpoint.
fn input_deltas(view: &TransactionView) -> Result<Vec<InputDelta>, BlockDeltasError> {
    if view.detail.coinbase.is_some() {
        return Ok(Vec::new());
    }
    view.inputs
        .iter()
        .zip(0u32..)
        .map(|(input, index)| {
            Ok(InputDelta {
                script: input.spent.script.clone(),
                satoshis: negated(input.spent.value)?,
                index,
                prev_txid: input.outpoint.prev_txid,
                prevout: input.outpoint.prev_index,
            })
        })
        .collect()
}

/// One transaction's output deltas: each output's (positive) value, carrying its
/// script and `vout` index.
fn output_deltas(view: &TransactionView) -> Vec<OutputDelta> {
    view.transaction
        .transparent
        .outputs
        .iter()
        .zip(0u32..)
        .map(|(output, index)| OutputDelta {
            script: output.script.clone(),
            satoshis: output.value,
            index,
        })
        .collect()
}

/// `value` as a negative value movement — a spend removes value from an address.
///
/// A resolved output value is already bounded by the money supply, so the
/// negation is representable; the error is defensive against a corrupt amount,
/// failing loud rather than truncating (the no-magnitude-overflow rule).
fn negated(value: Zatoshis) -> Result<SignedZatoshis, BlockDeltasError> {
    let magnitude =
        i64::try_from(value.as_u64()).map_err(|_| BlockDeltasError::InputValueOutOfRange {
            value: value.as_u64(),
        })?;
    let signed = magnitude
        .checked_neg()
        .ok_or(BlockDeltasError::InputValueOutOfRange {
            value: value.as_u64(),
        })?;
    SignedZatoshis::try_new(signed).map_err(|_| BlockDeltasError::InputValueOutOfRange {
        value: value.as_u64(),
    })
}
