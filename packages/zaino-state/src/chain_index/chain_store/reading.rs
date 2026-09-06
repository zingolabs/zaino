//! Reading the store through its ports.
//!
//! Every finalised read ChainIndex makes goes through one of the helpers here,
//! and every helper is generic over a `zaino-chain-store` port rather than over
//! the backend's reader. That bound is the point: it is what makes "ChainIndex
//! reads the store through its ports" a fact the compiler checks, instead of a
//! claim about which method happened to be called. A helper that reached for an
//! inherent method would stop compiling.
//!
//! They convert in both directions, because ChainIndex's own vocabulary has not
//! moved yet: it names heights, hashes and blocks in the backend's shapes, and
//! answers its callers in wire ones. Keeping that translation here rather than
//! at the call sites means the RPC methods read as they did before, and the
//! whole of it is deleted in one piece when ChainIndex is.

use futures::{Stream, StreamExt as _};
use zaino_chain_store::{
    ChainStoreError, ChainStoreReader, CompactBlockRead, SpentOutputIndex, StoredBlockRead,
    StoredTxOut, TransactionIndex, TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_primitives::types::BlockTxPosition;
use zaino_proto::proto::{compact_formats::CompactBlock, utils::PoolTypeFilter};

use crate::chain_index::types::{IndexedBlock, Outpoint, TransactionHash};
use crate::error::ChainIndexError;

/// A read the finalised store has no answer for, as `None`.
///
/// Three conditions mean the same thing to ChainIndex: the store holds nothing
/// at that height, has not opened yet, or is not built up to it. In all three
/// the answer is above or outside the finalised half, and ChainIndex's job is
/// to look in the recent window or at the validator instead — which is exactly
/// what it already does with `None`.
///
/// Collapsing them here rather than at each call site keeps the behaviour the
/// inherent reads had: those returned `Ok(None)` for an unbuilt height, because
/// they did not consult a watermark at all. The ports do, and report it
/// precisely; this is where that precision is deliberately spent, so that
/// switching to them cannot turn a routine fall-through into an error.
fn absent<T>(result: Result<Option<T>, ChainStoreError>) -> Result<Option<T>, ChainIndexError> {
    match result {
        Ok(value) => Ok(value),
        Err(ChainStoreError::AboveWatermark { .. }) | Err(ChainStoreError::NotReady) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// This crate's height, as the domain names it.
///
/// Shares [`crate::chain_index::chain_head::domain_height`] rather than restating it: both
/// halves must agree about which heights are expressible, and a second copy is
/// a second thing to keep in step.
pub(super) fn domain_height(height: crate::Height) -> Option<zaino_primitives::types::Height> {
    crate::chain_index::chain_head::domain_height(height)
}

/// The domain's height, as this crate names it.
fn local_height(height: zaino_primitives::types::Height) -> crate::Height {
    crate::Height(u32::from(height))
}

/// The main-chain block hash at `height`, or `None` if the store has none.
pub(crate) async fn block_hash<R: ChainStoreReader>(
    reader: &R,
    height: crate::Height,
) -> Result<Option<crate::BlockHash>, ChainIndexError> {
    // A height the domain cannot express names no block, which is the same
    // answer as a height the store does not hold.
    let Some(domain) = domain_height(height) else {
        return Ok(None);
    };
    Ok(absent(reader.block_hash(domain).await)?.map(|hash| crate::BlockHash(hash.into())))
}

/// The height of `hash`, or `None` if the store does not hold that block.
pub(crate) async fn block_height<R: ChainStoreReader>(
    reader: &R,
    hash: crate::BlockHash,
) -> Result<Option<crate::Height>, ChainIndexError> {
    Ok(absent(
        reader
            .block_height(crate::chain_index::chain_head::domain_hash(hash))
            .await,
    )?
    .map(local_height))
}

/// The indexed block at `height`, or `None` if the store does not hold it.
///
/// `blocks_chunk(h, h)` rather than a point read, because the port has no point
/// read: a chunk *is* the primitive, and a one-block chunk is one read
/// transaction — the same cost the single-block method had.
pub(crate) async fn block_at<R: StoredBlockRead>(
    reader: &R,
    height: crate::Height,
) -> Result<Option<IndexedBlock>, ChainIndexError> {
    let Some(domain) = domain_height(height) else {
        return Ok(None);
    };
    let Some(mut blocks) = absent(reader.blocks_chunk(domain, domain).await.map(Some))? else {
        return Ok(None);
    };
    match blocks.pop() {
        Some(block) => Ok(Some(
            zaino_chain_store_zainodb::adapter::indexed_block_from_stored(&block)?,
        )),
        None => Ok(None),
    }
}

/// Where `txid` was mined, or `None` if the store has not indexed it.
pub(crate) async fn tx_position<R: TransactionIndex>(
    reader: &R,
    txid: &TransactionHash,
) -> Result<Option<BlockTxPosition>, ChainIndexError> {
    absent(
        reader
            .tx_position(&zaino_primitives::types::TransactionId::from(txid.0))
            .await,
    )
}

/// Which transaction spent each outpoint, in the order asked.
///
/// Returns the spender's txid rather than its position: the port resolves the
/// two together, where the inherent read returned a position and left every
/// caller to look the txid up afterwards. That second lookup is gone, which is
/// one keyed read per distinct spending transaction saved on a path that runs
/// per queried outpoint.
pub(crate) async fn outpoint_spenders<R: SpentOutputIndex>(
    reader: &R,
    outpoints: &[Outpoint],
) -> Result<Vec<Option<TransactionHash>>, ChainIndexError> {
    let domain: Vec<zaino_primitives::types::Outpoint> = outpoints
        .iter()
        .map(zaino_chain_store_zainodb::adapter::domain_outpoint)
        .collect();

    Ok(reader
        .outpoint_spenders(&domain)
        .await?
        .into_iter()
        .map(|spender| spender.map(|spender| TransactionHash(spender.txid.into())))
        .collect())
}

/// Every transparent output of the transaction at `position`.
pub(crate) async fn transparent_outputs<R: SpentOutputIndex>(
    reader: &R,
    position: BlockTxPosition,
) -> Result<Option<Vec<StoredTxOut>>, ChainIndexError> {
    absent(reader.transparent_outputs(position).await)
}

/// What `outpoint` held, or `None` if the store does not hold the output.
pub(crate) async fn previous_output<R: SpentOutputIndex>(
    reader: &R,
    outpoint: &Outpoint,
) -> Result<Option<StoredTxOut>, ChainIndexError> {
    absent(
        reader
            .previous_outputs(&[zaino_chain_store_zainodb::adapter::domain_outpoint(
                outpoint,
            )])
            .await
            .map(|mut outputs| outputs.pop()),
    )
    .map(Option::flatten)
}

/// The finalised half of the UTXO-set commitment.
pub(crate) async fn txout_set<R: TxOutSetIndex>(
    reader: &R,
) -> Result<TxOutSetAccumulator, ChainIndexError> {
    Ok(reader.txout_set().await?)
}

/// The compact block at `height`, in the wire shape, or `None` if absent.
pub(crate) async fn compact_block<R: CompactBlockRead>(
    reader: &R,
    height: crate::Height,
    pools: &PoolTypeFilter,
) -> Result<Option<CompactBlock>, ChainIndexError> {
    let Some(domain) = domain_height(height) else {
        return Ok(None);
    };
    let filter = zaino_chain_store_zainodb::conversion::pool_filter_from_wire(pools);
    let blocks = absent(reader.compact_chunk(domain, domain, filter).await.map(Some))?;
    blocks
        .and_then(|mut blocks| blocks.pop())
        .as_ref()
        .map(zaino_chain_store_zainodb::conversion::compact_block_to_wire)
        .transpose()
        .map_err(ChainIndexError::internal_from)
}

/// Compact blocks over `start..=end`, ascending, in the wire shape.
///
/// The chunks the port yields are flattened back into single blocks here, at
/// the edge where the wire message is built. The chunking is not undone by
/// that: the point of it is that the store opens one read transaction per
/// thousand blocks instead of one per block, which has already happened by the
/// time a chunk arrives.
pub(crate) async fn compact_blocks_ascending<R: CompactBlockRead>(
    reader: &R,
    start: crate::Height,
    end: crate::Height,
    pools: &PoolTypeFilter,
) -> Result<WireCompactBlocks, ChainIndexError> {
    let (Some(start), Some(end)) = (domain_height(start), domain_height(end)) else {
        return Err(ChainIndexError::invalid_argument(
            "compact block range is beyond the protocol maximum height",
        ));
    };
    let filter = zaino_chain_store_zainodb::conversion::pool_filter_from_wire(pools);

    let chunks = reader.compact_stream(start, end, filter).await?;
    Ok(Box::pin(chunks.flat_map(|chunk| {
        futures::stream::iter(match chunk {
            Ok(blocks) => blocks
                .iter()
                .map(|block| {
                    zaino_chain_store_zainodb::conversion::compact_block_to_wire(block)
                        .map_err(|error| tonic::Status::internal(error.to_string()))
                })
                .collect::<Vec<_>>(),
            Err(error) => vec![Err(wire_status(error))],
        })
    })))
}

/// Compact blocks over `end..=start`, descending, in the wire shape.
///
/// Built from the ascending port rather than from a descending read, because
/// the port is ascending-only by design — a store that walked backwards would
/// double every range path to serve one caller. Reversing here costs one chunk
/// of memory, not the range: chunks are requested from the top down and each is
/// reversed as it arrives, so a descending scan holds no more than an ascending
/// one.
pub(crate) async fn compact_blocks_descending<R: CompactBlockRead + Clone + 'static>(
    reader: &R,
    start: crate::Height,
    end: crate::Height,
    pools: &PoolTypeFilter,
) -> Result<WireCompactBlocks, ChainIndexError> {
    let (Some(start), Some(end)) = (domain_height(start), domain_height(end)) else {
        return Err(ChainIndexError::invalid_argument(
            "compact block range is beyond the protocol maximum height",
        ));
    };
    if start < end {
        return Err(ChainIndexError::invalid_argument(
            "descending compact block range must start at or above its end",
        ));
    }
    let filter = zaino_chain_store_zainodb::conversion::pool_filter_from_wire(pools);
    let reader = reader.clone();

    // `end` is the lowest height and `start` the highest, because the range is
    // descending. The cursor walks down from `start`, and stops once it would
    // step below `end`.
    let stream = futures::stream::try_unfold(Some(start), move |cursor| {
        let reader = reader.clone();
        async move {
            let Some(top) = cursor else {
                return Ok(None);
            };
            let bottom = descending_chunk_floor(top, end);
            let mut blocks = reader.compact_chunk(bottom, top, filter).await?;
            blocks.reverse();

            let next = (bottom > end).then(|| {
                zaino_primitives::types::Height::try_from(u32::from(bottom).saturating_sub(1))
                    .unwrap_or(end)
            });
            Ok::<_, ChainStoreError>(Some((blocks, next)))
        }
    });

    Ok(Box::pin(stream.flat_map(|chunk| {
        futures::stream::iter(match chunk {
            Ok(blocks) => blocks
                .iter()
                .map(|block| {
                    zaino_chain_store_zainodb::conversion::compact_block_to_wire(block)
                        .map_err(|error| tonic::Status::internal(error.to_string()))
                })
                .collect::<Vec<_>>(),
            Err(error) => vec![Err(wire_status(error))],
        })
    })))
}

/// The lowest height a descending chunk starting at `top` should cover.
///
/// Mirrors the ascending walk's chunk size, so a descending scan opens the same
/// number of read transactions over the same range as an ascending one.
fn descending_chunk_floor(
    top: zaino_primitives::types::Height,
    floor: zaino_primitives::types::Height,
) -> zaino_primitives::types::Height {
    let candidate = u32::from(top).saturating_sub(DESCENDING_CHUNK - 1);
    zaino_primitives::types::Height::try_from(candidate)
        .ok()
        .filter(|bottom| *bottom > floor)
        .unwrap_or(floor)
}

/// How many blocks one descending chunk covers.
///
/// The same figure the store's own ascending walk uses. It is restated rather
/// than imported because it is not part of the port's contract — chunk
/// boundaries carry no meaning, and a store is free to choose differently.
const DESCENDING_CHUNK: u32 = 1024;

/// A stream of wire compact blocks.
///
/// Boxed rather than named, because it is consumed immediately by the task that
/// merges the finalised and recent segments and never crosses a public
/// boundary.
pub(crate) type WireCompactBlocks =
    std::pin::Pin<Box<dyn Stream<Item = Result<CompactBlock, tonic::Status>> + Send>>;

/// A store failure, as the gRPC status the block stream carries.
///
/// The mapping happens here, at the serving edge, rather than inside the store:
/// an LMDB cursor desync is not a gRPC concern, and the port carries a
/// [`ChainStoreError`] precisely so that the crate that answers gRPC is the one
/// that decides what a client is told.
///
/// # Exhaustive on purpose
///
/// No catch-all arm. A catch-all answers for variants that do not exist yet,
/// and its answer is always `internal` — so a variant added later is reported
/// as a server fault whatever it means, silently and without anything failing
/// to compile. Naming every variant makes adding one a decision taken here
/// rather than one inherited by default.
///
/// The status carries only the error's own `Display`. Causes are for the
/// operator's log, not for a client: the chain below a `Backend` names the
/// storage engine and its errno, which tells a caller nothing it can act on.
fn wire_status(error: ChainStoreError) -> tonic::Status {
    // Rendered once, before the match consumes the error: the message is the
    // same in every arm and only the code differs.
    let message = error.to_string();

    match error {
        // The caller asked outside what this half of the chain covers. Both are
        // about the range, not about the store's health.
        ChainStoreError::AboveWatermark { .. } | ChainStoreError::InvalidRange { .. } => {
            tonic::Status::out_of_range(message)
        }

        // Transient, and it resolves on its own once opening completes, so the
        // caller is told to come back.
        ChainStoreError::NotReady => tonic::Status::unavailable(message),

        // The store is healthy and this row is genuinely not here.
        ChainStoreError::MissingRow(_) => tonic::Status::not_found(message),

        // The store is healthy and the request is well-formed; this deployment
        // simply does not build that index. `internal` would tell the client
        // the server faulted, which is untrue and invites a retry that cannot
        // succeed. `unimplemented` says what is actually the case: not offered
        // here, do not come back for it.
        ChainStoreError::Unavailable(_) => tonic::Status::unimplemented(message),

        // Both are the server's fault and neither is the caller's to fix: one
        // is a broken read, the other a row that decoded into something
        // unusable.
        ChainStoreError::CorruptRow { .. } | ChainStoreError::Backend { .. } => {
            tonic::Status::internal(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_primitives::types::Height as DomainHeight;

    fn h(height: u32) -> DomainHeight {
        DomainHeight::try_from(height).expect("valid height")
    }

    /// The descending walk covers its range exactly, in chunks, without
    /// stepping below the floor or repeating a height.
    ///
    /// Worth pinning directly rather than only through a stream test: this is
    /// the one piece of range arithmetic ChainIndex owns rather than borrows,
    /// and it exists because the port is ascending-only. Off by one at either
    /// end is either a block served twice or a block never served, and both
    /// look like a wallet syncing slightly wrong rather than like a failure.
    #[test]
    fn the_descending_walk_covers_its_range_exactly_once() {
        // A range longer than one chunk, so the boundary arithmetic is
        // exercised rather than short-circuited.
        let floor = h(5);
        let mut top = h(5 + DESCENDING_CHUNK * 2 + 7);

        let mut covered: Vec<u32> = Vec::new();
        loop {
            let bottom = descending_chunk_floor(top, floor);
            assert!(bottom >= floor, "a chunk stepped below the floor");
            assert!(bottom <= top, "a chunk inverted");
            covered.extend((u32::from(bottom)..=u32::from(top)).rev());

            if bottom <= floor {
                break;
            }
            top = h(u32::from(bottom) - 1);
        }

        let expected: Vec<u32> = (u32::from(floor)..=(5 + DESCENDING_CHUNK * 2 + 7))
            .rev()
            .collect();
        assert_eq!(covered, expected);
    }

    /// A range shorter than one chunk is a single chunk that stops at the floor.
    #[test]
    fn a_short_descending_range_is_one_chunk() {
        assert_eq!(descending_chunk_floor(h(10), h(4)), h(4));
    }

    /// A single-height descending range is a single-height chunk.
    #[test]
    fn a_single_height_descending_range_does_not_step_below_itself() {
        assert_eq!(descending_chunk_floor(h(7), h(7)), h(7));
    }

    /// A chunk never covers more than the chunk size, however far the floor is.
    #[test]
    fn a_descending_chunk_is_bounded_by_the_chunk_size() {
        let top = h(DESCENDING_CHUNK * 4);
        let bottom = descending_chunk_floor(top, h(0));
        assert_eq!(
            u32::from(top) - u32::from(bottom) + 1,
            DESCENDING_CHUNK,
            "a descending chunk should hold exactly one chunk's worth"
        );
    }

    /// The three conditions that mean "the finalised store has no answer" all
    /// become `None`, and nothing else does.
    ///
    /// The mapping is what keeps ChainIndex routing to the recent window as it
    /// did before the ports were consulted, so a variant leaking through as an
    /// error is a read that used to fall through and now fails.
    #[test]
    fn only_the_absent_conditions_become_none() {
        assert!(matches!(
            absent::<u8>(Err(ChainStoreError::NotReady)),
            Ok(None)
        ));
        assert!(matches!(
            absent::<u8>(Err(ChainStoreError::AboveWatermark {
                requested: h(2),
                watermark: h(1),
            })),
            Ok(None)
        ));

        // A backend failure is a failure. Reporting it as "no such block" would
        // make a broken database look like an empty chain.
        assert!(absent::<u8>(Err(ChainStoreError::backend("lmdb"))).is_err());
        assert!(absent::<u8>(Err(ChainStoreError::MissingRow("txid".into()))).is_err());
        assert!(absent::<u8>(Err(ChainStoreError::InvalidRange {
            start: h(2),
            end: h(1)
        }))
        .is_err());
    }

    /// A store failure carries a status a client can act on.
    #[test]
    fn store_failures_map_onto_distinguishable_statuses() {
        use tonic::Code;

        assert_eq!(
            wire_status(ChainStoreError::NotReady).code(),
            Code::Unavailable
        );
        assert_eq!(
            wire_status(ChainStoreError::AboveWatermark {
                requested: h(2),
                watermark: h(1)
            })
            .code(),
            Code::OutOfRange
        );
        assert_eq!(
            wire_status(ChainStoreError::InvalidRange {
                start: h(2),
                end: h(1)
            })
            .code(),
            Code::OutOfRange
        );
        assert_eq!(
            wire_status(ChainStoreError::MissingRow("txid".into())).code(),
            Code::NotFound
        );
        assert_eq!(
            wire_status(ChainStoreError::corrupt_row("in-range height")).code(),
            Code::Internal
        );
        assert_eq!(
            wire_status(ChainStoreError::backend("lmdb")).code(),
            Code::Internal
        );
    }

    /// An index this deployment does not build is not a server fault.
    ///
    /// The store is healthy and the request well-formed, so `internal` would
    /// both misinform the client and invite a retry that cannot succeed. Its
    /// own arm rather than a line in the test above, because this is the one
    /// the catch-all used to swallow.
    #[test]
    fn an_unbuilt_index_reports_as_unimplemented_not_as_a_fault() {
        use zaino_chain_store::StoreCapability;

        let status = wire_status(ChainStoreError::Unavailable(StoreCapability::TxOutSet));

        assert_eq!(status.code(), tonic::Code::Unimplemented);
    }

    /// The wire status does not carry the cause.
    ///
    /// A `Backend` chain names the storage engine and its errno, which tells a
    /// client nothing it can act on. That belongs in the operator's log.
    #[test]
    fn a_wire_status_reports_the_failure_without_its_cause() {
        #[derive(Debug, thiserror::Error)]
        #[error("errno 22")]
        struct BackendFailure;

        let status = wire_status(ChainStoreError::backend_because(
            "reading block 42",
            BackendFailure,
        ));

        assert!(status.message().contains("reading block 42"));
        assert!(
            !status.message().contains("errno 22"),
            "the cause should stay in the log: {:?}",
            status.message()
        );
    }
}
