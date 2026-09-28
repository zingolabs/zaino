//! `zaino-store` — the finalised tier, as a provider.
//!
//! A thin reader over a persistence [`Backend`] and an [`IndexSet`]: it
//! composes the `zaino-service` reads **on read** from the indexes the set
//! builds, and pins each read to the writer's committed watermark so a
//! [`StoreSnapshot`] is one coherent view of the finalised chain.
//!
//! The crate defines no ports. What it *serves* are the shared serving ports
//! (`ChainSegment`, `CompactBlockRead`, `Serviceable`, `TakeSnapshot`), the
//! same ones the non-finalised head serves, so the composer in `zaino-core`
//! names both tiers through one contract. What it *consumes* is the backend
//! port from `zaino-persistence` and the index schemas from `zaino-indexes`.
//!
//! # Presence is in the type
//!
//! Every serving read is implemented **only for index sets that build the
//! indexes it composes from**: `CompactBlockRead` needs the compact-block
//! capability's indexes, declared once in `zaino_indexes::capabilities::local`.
//! A store over an index set lacking one does not have the read, so a
//! deployment that demands it fails where the store is wired, not per
//! request. The store claims nothing it cannot back: reads it does not own
//! (treestate, raw transactions, mempool, broadcast, and address history
//! until a local transparent index is served) are not stubbed here; the
//! composer routes them to the provider that has them.
//!
//! ```text
//! reads(StoreSnapshot<B, M>) = { R : indexes(R) ⊆ built(M) }
//! ```
//!
//! # Known limitation
//!
//! Reads run on the async executor without `spawn_blocking`. An LMDB read is
//! a memory-mapped lookup and short, so a per-height compact-block read is
//! fine; a scan-shaped read (address history, when it lands) must move off
//! the executor.
#![forbid(unsafe_code)]

mod component;
mod watermark_repair;

pub use component::StoreComponent;
pub use watermark_repair::{WatermarkRepair, WatermarkRepairError};

use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

use futures::stream::BoxStream;
use zaino_indexes::capabilities::local::{self, Backs};
use zaino_indexes::index_set::{Builds, IndexSet};
use zaino_indexes::indexes::chain_metadata::{self, ChainMetadataIndex};
use zaino_indexes::indexes::hash_to_height::{self, HashToHeightIndex};
use zaino_indexes::indexes::headers::{self, HeadersIndex};
use zaino_indexes::indexes::ironwood::{self, IronwoodIndex};
use zaino_indexes::indexes::orchard::{self, OrchardIndex};
use zaino_indexes::indexes::sapling::{self, SaplingIndex};
use zaino_indexes::indexes::transparent_data::{self, TransparentDataIndex};
use zaino_indexes::indexes::txids::{self, TxidsIndex};
use zaino_persistence::{Backend, BackendReader, Namespace};
use zaino_persistence_codec::{
    decode_value, encode_key, freshness, watermark, EntryCodec, Freshness,
};
use zaino_primitives::types::{BlockHash, BlockRef, BlockSelector, Height, HeightRange};
use zaino_primitives::types::{
    CompactBlock, OrchardAction, PreIndexCompactTx, SaplingOutput, TransparentInput,
    TransparentOutput,
};
use zaino_service::error::{BlockReadError, ReadError, Transient};
use zaino_service::{Answerable, ServiceabilityManifest, ServiceableRange};
use zaino_service::{ChainSegment, CompactBlockRead, Serviceable, Snapshot, TakeSnapshot};
use zaino_sync::primitives::BlockHeight;

/// A read handle over the KV backend, for the index set `M`. It consumes the
/// writer's committed watermark on each snapshot; it holds no coordinates of
/// its own.
///
/// `M` names the index set the backend was built with. It carries no data; it
/// is the static promise the serving reads bound on.
pub struct StoreReader<B, M> {
    backend: Arc<B>,
    index_set: PhantomData<M>,
}

impl<B, M> StoreReader<B, M> {
    /// A reader over `backend`, built to the index set `M`. The finalised
    /// tip is read live from the backend's watermark at snapshot time, not
    /// passed in.
    pub fn new(backend: Arc<B>) -> Self {
        Self {
            backend,
            index_set: PhantomData,
        }
    }
}

// Manual `Clone` so the bound is on `Arc<B>` (always cloneable), not `B` — a
// serving adapter clones the reader per connection, and all clones share one
// backend.
impl<B, M> Clone for StoreReader<B, M> {
    fn clone(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            index_set: PhantomData,
        }
    }
}

impl<B: Backend + 'static, M: IndexSet> Serviceable for StoreReader<B, M> {
    fn serviceability(&self) -> ServiceabilityManifest {
        // Infallible by contract: a serviceability query must not be the call
        // that fails, so a backend read failure degrades to "nothing serviceable
        // right now" rather than propagating — not-yet, because the indexes are
        // still built; the reader just could not see them. The manifest is
        // derived from the built index set (the `Capability ⇄ IndexId`
        // relation), bounded by the committed watermark.
        let Ok(reader) = self.backend.reader() else {
            return ServiceabilityManifest::uniform(Answerable::NotYet);
        };
        let finalized_tip = watermark::read(&reader).ok().flatten();
        zaino_indexes::capabilities::serviceability(&reader, finalized_tip)
    }
}

impl<B, M> TakeSnapshot for StoreReader<B, M>
where
    B: Backend + 'static,
    // The pinned tip's hash is composed from the headers index.
    M: Builds<HeadersIndex>,
{
    type Snapshot = StoreSnapshot<B, M>;

    fn snapshot(&self) -> impl Future<Output = Result<Self::Snapshot, Transient>> + Send {
        let backend = self.backend.clone();
        async move {
            // Consume the writer's watermark — the finalised tip this view can
            // answer up to — and pin it. Reads through the snapshot hit live
            // backend state rather than a read transaction; that is coherent
            // because the finalised tier is append-only and every read is
            // bounded by the pinned watermark, below which nothing changes.
            let reader = backend
                .reader()
                .map_err(|e| Transient(format!("open reader: {e}")))?;
            let finalized_tip =
                watermark::read(&reader).map_err(|e| Transient(format!("read watermark: {e}")))?;
            // Compose the tip's BlockRef on read from the headers index, so the
            // view reports a real (height, hash) rather than a bare height.
            let pinned_tip = match finalized_tip {
                Some(height) => {
                    read_index_value::<HeadersIndex, B>(&reader, headers::ID.into(), height)?.map(
                        |header| BlockRef {
                            height,
                            hash: header.hash,
                        },
                    )
                }
                None => None,
            };
            Ok(StoreSnapshot {
                backend: backend.clone(),
                finalized_tip,
                pinned_tip,
                index_set: PhantomData,
            })
        }
    }
}

/// An immutable pinned view over a store built to `M`. Clones share the
/// backend via `Arc`.
pub struct StoreSnapshot<B, M> {
    backend: Arc<B>,
    /// The finalised watermark this view was pinned to, read from the backend.
    finalized_tip: Option<Height>,
    /// The finalised tip's `BlockRef`, composed from the headers index at pin time.
    pinned_tip: Option<BlockRef>,
    index_set: PhantomData<M>,
}

// Manual `Clone` so the bound is on `Arc<B>` (always cloneable), not `B`.
impl<B, M> Clone for StoreSnapshot<B, M> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            finalized_tip: self.finalized_tip,
            pinned_tip: self.pinned_tip,
            index_set: PhantomData,
        }
    }
}

impl<B: Backend + 'static, M: IndexSet> ChainSegment for StoreSnapshot<B, M> {
    fn pinned_tip(&self) -> Option<BlockRef> {
        self.pinned_tip
    }

    fn coverage(&self) -> Option<HeightRange> {
        // The finalised store serves `[genesis, finalized_tip]`; an empty store
        // (no watermark) covers nothing.
        self.finalized_tip.map(|end| HeightRange {
            start: Height::GENESIS,
            end,
        })
    }
}

impl<B: Backend + 'static, M: IndexSet> Snapshot for StoreSnapshot<B, M> {
    fn serviceable_range(&self) -> ServiceableRange {
        // No non-finalised window is wired, so the view answers up to the
        // finalised tip only: `tip == finalized_tip`.
        let tip = self.finalized_tip.unwrap_or(Height::GENESIS);
        ServiceableRange {
            finalized_tip: tip,
            tip,
        }
    }
}

/// The read exists only where every index a compact block composes from is
/// built — the one list [`local::Blocks`] declares, which the manifest checks
/// stamps for at snapshot time.
impl<B, M> CompactBlockRead for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::Blocks>,
{
    fn compact_block(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send {
        let backend = self.backend.clone();
        async move {
            let reader = backend
                .reader()
                .map_err(|e| BlockReadError::Fatal(format!("open reader: {e}")))?;
            let height = match at {
                BlockSelector::Height(height) => Some(height),
                BlockSelector::Hash(hash) => {
                    resolve_hash::<B>(&reader, hash).map_err(|t| BlockReadError::Transient(t.0))?
                }
            };
            match height {
                Some(height) => read_compact_block::<B>(&reader, height),
                None => Ok(None),
            }
        }
    }

    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>> {
        // Eager, not lazy: the whole range is read up front. Streaming in
        // chunks with backpressure is the known follow-up for wide ranges.
        let blocks: Vec<Result<CompactBlock, ReadError>> = match self.backend.reader() {
            Ok(reader) => (u32::from(range.start)..=u32::from(range.end))
                .filter_map(|height| Height::try_from(height).ok())
                .filter_map(|height| {
                    read_compact_block::<B>(&reader, height)
                        .map_err(block_read_to_read_error)
                        .transpose()
                })
                .collect(),
            Err(e) => vec![Err(ReadError::Fatal(format!("open reader: {e}")))],
        };
        Box::pin(futures::stream::iter(blocks))
    }
}

/// Map a [`BlockReadError`] onto the generic [`ReadError`] used by streamed
/// reads, preserving the transient/fatal distinction.
fn block_read_to_read_error(error: BlockReadError) -> ReadError {
    match error {
        BlockReadError::NotServiceable(capability) => ReadError::NotServiceable(capability),
        BlockReadError::Transient(message) => ReadError::Transient(message),
        BlockReadError::Fatal(message) => ReadError::Fatal(message),
    }
}

/// Read one height-keyed index's value at `height`, composing on read.
///
/// Applies the codec version guard — a format skew reads as absent rather than
/// decoding stale bytes. The headers index is keyed by the engine's
/// `BlockHeight`, so the domain `Height` is converted here.
fn read_index_value<C, B>(
    reader: &B::Reader,
    namespace: Namespace,
    height: Height,
) -> Result<Option<C::Value>, Transient>
where
    C: EntryCodec<Key = BlockHeight>,
    B: Backend,
{
    if freshness::<C>(reader, namespace)
        .map_err(|e| Transient(format!("freshness {}: {e}", namespace.as_str())))?
        == Freshness::Stale
    {
        return Ok(None);
    }
    let key = encode_key::<C>(&BlockHeight::new(u64::from(height)));
    match reader
        .get(namespace, &key)
        .map_err(|e| Transient(format!("read {}: {e}", namespace.as_str())))?
    {
        Some(bytes) => decode_value::<C>(&bytes)
            .map(Some)
            .map_err(|e| Transient(format!("decode {}: {e}", namespace.as_str()))),
        None => Ok(None),
    }
}

/// Resolve a canonical block hash to its height via the `hash_to_height` index.
fn resolve_hash<B: Backend>(
    reader: &B::Reader,
    hash: BlockHash,
) -> Result<Option<Height>, Transient> {
    let namespace: Namespace = hash_to_height::ID.into();
    if freshness::<HashToHeightIndex>(reader, namespace)
        .map_err(|e| Transient(format!("freshness {}: {e}", namespace.as_str())))?
        == Freshness::Stale
    {
        return Ok(None);
    }
    let key = encode_key::<HashToHeightIndex>(&hash);
    match reader
        .get(namespace, &key)
        .map_err(|e| Transient(format!("read {}: {e}", namespace.as_str())))?
    {
        Some(bytes) => {
            let block_height = decode_value::<HashToHeightIndex>(&bytes)
                .map_err(|e| Transient(format!("decode {}: {e}", namespace.as_str())))?;
            Height::try_from(
                u32::try_from(block_height.value()).map_err(|_| {
                    Transient("indexed height exceeds the protocol limit".to_owned())
                })?,
            )
            .map(Some)
            .map_err(|_| Transient("indexed height exceeds the protocol limit".to_owned()))
        }
        None => Ok(None),
    }
}

/// Compose a true `CompactBlock` at `height` on read from the granular indexes
/// (the `Blocks` capability's backing set): headers + txids + per-pool compact
/// data, plus the `ChainMetadata` tree sizes. Returns `None` when no block is
/// indexed at `height`.
///
/// The engine commits every index for a block in one atomic batch, and the
/// per-pool indexes derive their per-tx entries from the same transaction list.
/// So a header at `height` **grants** that the other indexes are present there
/// and that their per-tx Vecs all have the block's tx count. This function
/// therefore treats a missing companion index or a length mismatch as
/// corruption (`Fatal`), not as an empty default — the alternative silently
/// serves a wrong compact block (zero tree sizes, dropped transactions).
///
/// Synchronous on the async path: a handful of memory-mapped lookups per
/// height, see the crate doc.
fn read_compact_block<B: Backend>(
    reader: &B::Reader,
    height: Height,
) -> Result<Option<CompactBlock>, BlockReadError> {
    let Some(header) = read_present::<HeadersIndex, B>(reader, headers::ID.into(), height)? else {
        return Ok(None);
    };

    // Companions are granted co-present with the header (atomic commit).
    let chain_metadata = require::<ChainMetadataIndex, B>(
        reader,
        chain_metadata::ID.into(),
        height,
        "chain_metadata",
    )?;
    let txids = require::<TxidsIndex, B>(reader, txids::ID.into(), height, "txids")?.0;
    let transparent = require::<TransparentDataIndex, B>(
        reader,
        transparent_data::ID.into(),
        height,
        "transparent",
    )?
    .0;
    let sapling = require::<SaplingIndex, B>(reader, sapling::ID.into(), height, "sapling")?.0;
    let orchard = require::<OrchardIndex, B>(reader, orchard::ID.into(), height, "orchard")?.0;
    let ironwood = require::<IronwoodIndex, B>(reader, ironwood::ID.into(), height, "ironwood")?.0;

    // Per-tx alignment is granted (same tx list): all pools have `txids.len()`.
    let count = txids.len();
    for (name, len) in [
        ("transparent", transparent.len()),
        ("sapling", sapling.len()),
        ("orchard", orchard.len()),
        ("ironwood", ironwood.len()),
    ] {
        if len != count {
            return Err(BlockReadError::Fatal(format!(
                "inconsistent index at height {}: {count} txids but {len} {name} entries",
                u32::from(height),
            )));
        }
    }

    // Aligned, so zip directly — no per-tx defaulting.
    let transactions = txids
        .into_iter()
        .zip(transparent)
        .zip(sapling)
        .zip(orchard)
        .zip(ironwood)
        .map(
            |((((txid, transparent), sapling), orchard), ironwood)| PreIndexCompactTx {
                txid,
                transparent_inputs: transparent
                    .inputs
                    .iter()
                    .map(|(prev_txid, prev_index)| TransparentInput {
                        prev_txid: *prev_txid,
                        prev_index: *prev_index,
                    })
                    .collect(),
                transparent_outputs: transparent
                    .outputs
                    .iter()
                    .map(|(value, script)| TransparentOutput {
                        value: *value,
                        script: script.clone(),
                    })
                    .collect(),
                sapling_nullifiers: sapling.nullifiers,
                sapling_outputs: sapling
                    .outputs
                    .iter()
                    .map(|(cmu, ephemeral_key, enc_ciphertext)| SaplingOutput {
                        cmu: *cmu,
                        ephemeral_key: *ephemeral_key,
                        enc_ciphertext: *enc_ciphertext,
                    })
                    .collect(),
                orchard_actions: orchard
                    .actions
                    .iter()
                    .map(
                        |(nullifier, cmx, ephemeral_key, enc_ciphertext)| OrchardAction {
                            nullifier: *nullifier,
                            cmx: *cmx,
                            ephemeral_key: *ephemeral_key,
                            enc_ciphertext: *enc_ciphertext,
                        },
                    )
                    .collect(),
                ironwood_actions: ironwood
                    .actions
                    .iter()
                    .map(
                        |(nullifier, cmx, ephemeral_key, enc_ciphertext)| OrchardAction {
                            nullifier: *nullifier,
                            cmx: *cmx,
                            ephemeral_key: *ephemeral_key,
                            enc_ciphertext: *enc_ciphertext,
                        },
                    )
                    .collect(),
            },
        )
        .collect();

    Ok(Some(CompactBlock {
        hash: header.hash,
        prev_hash: header.prev_hash,
        height: u32::from(height),
        time: header.time,
        bits: header.bits,
        transactions,
        chain_metadata,
    }))
}

/// Read a height-keyed index value, mapping errors to [`BlockReadError`]
/// (backend read → transient, decode → fatal corruption).
fn read_present<C, B>(
    reader: &B::Reader,
    namespace: Namespace,
    height: Height,
) -> Result<Option<C::Value>, BlockReadError>
where
    C: EntryCodec<Key = BlockHeight>,
    B: Backend,
{
    read_index_value::<C, B>(reader, namespace, height).map_err(|t| BlockReadError::Transient(t.0))
}

/// Read a companion index that a present header *grants* is present. Absent →
/// `Fatal`: the block exists but this index does not, which is corruption.
fn require<C, B>(
    reader: &B::Reader,
    namespace: Namespace,
    height: Height,
    name: &str,
) -> Result<C::Value, BlockReadError>
where
    C: EntryCodec<Key = BlockHeight>,
    B: Backend,
{
    read_present::<C, B>(reader, namespace, height)?.ok_or_else(|| {
        BlockReadError::Fatal(format!(
            "inconsistent index at height {}: header present but no {name}",
            u32::from(height),
        ))
    })
}
