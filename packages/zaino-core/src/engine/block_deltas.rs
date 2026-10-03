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
//! * the predecessor header times the median time is taken over, read over the
//!   block's own ancestry by hash — locally where the local chain covers it,
//!   through the passthrough header read otherwise.
//!
//! A by-height request resolves the height to a hash once (ruling R50) — from the
//! block read — and issues the chain-position and transaction reads by that hash,
//! so the three reads cannot straddle a tip reorg between them. A miss on the
//! block is the domain not-found (`Ok(None)`); a subset present is a reorg race
//! surfaced as a transient failure, never a partially composed block. A block the
//! passthrough reports off the main chain (negative confirmations) is declined
//! with [`BlockDeltasError::Orphan`], mirroring zcashd's `blockToDeltasJSON`,
//! which reports confirmations only for a main-chain block.
//!
//! The median time mirrors zcashd's `GetMedianTimePast`: the median of the
//! block's own time and its ten predecessors (heights `h-10 ..= h`, or fewer near
//! genesis). The window is the requested block's ancestry, walked by hash rather
//! than by height, so it cannot straddle a reorg: the ancestors at or below the
//! local pinned tip are read from the coherent local chain (after checking the
//! local hash matches the walked ancestry), while ancestors above the tip — the
//! block is served from passthrough while the local indexer catches up — fall back
//! to the passthrough header read, the same source as the block, so nothing
//! straddles the seam. A header missing from the local chain at or below the
//! pinned tip is a genuine hole, failed loud as [`BlockDeltasError::MissingHeader`]
//! rather than inventing a median.

use crate::chain_view::ChainTier;
use crate::routing::Routing;
use zaino_consensus::block_time::median_time_past;
use zaino_primitives::types::{
    BlockHeader, BlockSelector, BlockTime, Height, SignedZatoshis, Zatoshis,
};
use zaino_service::error::{BlockDeltasError, BlockReadError};
use zaino_service::{
    BlockDeltas, BlockDeltasRead, BlockRead, BlockTransactionViews, BlockVerboseRead, ChainSegment,
    HeaderRead, InputDelta, OutputDelta, TransactionDeltas, TransactionView, TransactionViewRead,
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
        // zcashd's `blockToDeltasJSON` reports confirmations only for a block on
        // the main chain and throws "Block is an orphan" otherwise. The passthrough
        // verbose block reports `-1` confirmations for an off-chain block, so the
        // same condition is a negative count here: decline rather than compose a
        // delta view for an orphan.
        if header_is_orphan(verbose.confirmations) {
            return Err(BlockDeltasError::Orphan { hash: header.hash });
        }
        let median_time = self.median_time(&header).await?;
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
    Src: GetBlockHeader
        + GetBlockVerbose
        + GetBlockVerboseByHash
        + GetRawBlock
        + GetRawBlockByHash
        + Clone
        + Send
        + Sync
        + 'static,
    R: Routing,
{
    /// zcashd's `GetMedianTimePast` for the requested block: the median of the
    /// block's own time and its ten predecessors' times (heights `h-10 ..= h`, or
    /// fewer near genesis).
    ///
    /// The window is the block's own ancestry, walked by hash from the block's
    /// hash so it cannot straddle a reorg against the block. An ancestor at or
    /// below the local pinned tip is read from the coherent local chain, after
    /// checking the local hash matches the walked ancestry; an ancestor above the
    /// tip — the block is served from passthrough while the local indexer catches
    /// up — falls back to the passthrough header read, the same source as the
    /// block. A header missing from the local chain at or below the pinned tip is a
    /// genuine hole, failed loud as [`BlockDeltasError::MissingHeader`] rather than
    /// narrowing the window and shifting the median.
    async fn median_time(&self, header: &BlockHeader) -> Result<BlockTime, BlockDeltasError> {
        let tip = self.local().pinned_tip().map(|tip| u32::from(tip.height));
        let top = u32::from(header.height);
        let bottom = top.saturating_sub(MEDIAN_TIME_SPAN - 1);
        let mut times: Vec<BlockTime> = Vec::new();

        // Phase 1 — the ancestors above the local pinned tip (and the whole window
        // when there is no local coverage): walk by hash through the passthrough
        // header chain, which carries each ancestor's time and its predecessor's
        // hash. The walk stops at the first ancestor that is within local coverage,
        // yielding that boundary ancestor's `(height, hash)`; `None` when the window
        // bottoms out above the tip (or at genesis) before reaching coverage.
        let mut height = top;
        let mut hash = header.hash;
        let mut time = header.time;
        // Genesis has no predecessor; above genesis the block header names it.
        let mut prev = (top != 0).then_some(header.prev_hash);
        let boundary = loop {
            if tip.is_some_and(|tip| height <= tip) {
                break Some((height, hash));
            }
            times.push(time);
            if height == bottom {
                break None;
            }
            let Some(prev_hash) = prev else {
                // The window reaches genesis above the tip; it is complete.
                break None;
            };
            // Stop before fetching when the predecessor is already within local
            // coverage: its hash is `prev_hash`, which the current header carries,
            // so the boundary is crossed without a passthrough round trip.
            if tip.is_some_and(|tip| height - 1 <= tip) {
                break Some((height - 1, prev_hash));
            }
            let predecessor = self.block_header_verbose(prev_hash).await?.ok_or_else(|| {
                // The validator served the block but not one of its ancestors — a
                // reorg race between the two reads, surfaced as transient.
                BlockDeltasError::Block(BlockReadError::Transient(format!(
                    "validator did not serve ancestor header {prev_hash} while composing the median time; retry"
                )))
            })?;
            height -= 1;
            hash = prev_hash;
            time = predecessor.time;
            prev = predecessor.previous_block_hash;
        };

        // Phase 2 — the ancestors at or below the pinned tip: read from the local
        // chain. The pinned chain is internally coherent, so once the boundary
        // ancestor's local hash matches the walked ancestry, every lower ancestor is
        // this block's too and can be read by height.
        if let Some((boundary_height, boundary_hash)) = boundary {
            let Ok(boundary) = Height::try_from(boundary_height) else {
                return Err(BlockDeltasError::MissingHeader {
                    height: header.height,
                });
            };
            match self.local().header(boundary).await? {
                Some(summary) if summary.hash == boundary_hash => {
                    for raw in (bottom..=boundary_height).rev() {
                        let Ok(window_height) = Height::try_from(raw) else {
                            continue;
                        };
                        let summary = self.local().header(window_height).await?.ok_or(
                            BlockDeltasError::MissingHeader {
                                height: window_height,
                            },
                        )?;
                        times.push(summary.time);
                    }
                }
                // A hole in the local chain at or below the pinned tip is a genuine
                // chain-view gap, failed loud rather than inventing a median.
                None => return Err(BlockDeltasError::MissingHeader { height: boundary }),
                // The local chain holds a different block at the boundary height:
                // the ancestry has diverged from the pinned chain (a stale local
                // fork), so the local times would straddle. Walk the rest of the
                // window by hash through the passthrough headers instead.
                Some(_) => {
                    let mut height = boundary_height;
                    let mut hash = boundary_hash;
                    loop {
                        let summary = self.block_header_verbose(hash).await?.ok_or_else(|| {
                            BlockDeltasError::Block(BlockReadError::Transient(format!(
                                "validator did not serve ancestor header {hash} while composing the median time; retry"
                            )))
                        })?;
                        times.push(summary.time);
                        if height == bottom {
                            break;
                        }
                        let Some(prev_hash) = summary.previous_block_hash else {
                            break;
                        };
                        height -= 1;
                        hash = prev_hash;
                    }
                }
            }
        }

        // The window always includes the block's own height, so `times` is never
        // empty; the `MissingHeader` fallback is defensive, not a reachable path.
        median_time_past(&times).ok_or(BlockDeltasError::MissingHeader {
            height: header.height,
        })
    }
}

/// zcashd's `blockToDeltasJSON` orphan test: a block is on the main chain exactly
/// when its confirmations are non-negative, so a negative count (the passthrough
/// verbose block reports `-1` off-chain) is an orphan.
fn header_is_orphan(confirmations: zaino_primitives::types::Confirmations) -> bool {
    confirmations < 0
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
