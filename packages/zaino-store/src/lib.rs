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
//! Point reads run on the async executor without `spawn_blocking`. An LMDB read
//! is a memory-mapped lookup and short, so a per-height compact-block read is
//! fine. The address-history reads are scan-shaped and run on the blocking
//! pool. Other ranged reads (a compact-block range, subtree roots) still run
//! on the executor.
#![forbid(unsafe_code)]

mod address;
mod index_coverage;
mod spend;
mod spend_resolve;
mod transaction_location;
mod tree_state;
mod txout_set;
mod watermark_repair;

pub use index_coverage::{IndexCoverageError, UnstampedIndexes};
pub use watermark_repair::{WatermarkRepair, WatermarkRepairError};

use std::future::Future;
use std::marker::PhantomData;
use std::ops::ControlFlow;
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
    decode_key, decode_value, encode_key, freshness, watermark, EntryCodec, Freshness,
};
use zaino_primitives::types::{
    BlockHash, BlockRef, BlockSelector, Height, HeightRange, PoolActivations,
};
use zaino_primitives::types::{
    CompactBlock, OrchardAction, PreIndexCompactTx, SaplingOutput, TransparentInput,
    TransparentOutput,
};
use zaino_service::error::{BlockReadError, ReadError, Transient};
use zaino_service::{Answerable, Capability, ServiceabilityManifest, ServiceableRange};
use zaino_service::{
    ChainSegment, CompactBlockRead, HeaderRead, HeaderSummary, Serviceable, Snapshot, TakeSnapshot,
};
use zaino_sync::primitives::BlockHeight;

/// A read handle over the KV backend, for the index set `M`. It consumes the
/// writer's committed watermark on each snapshot; it holds no coordinates of
/// its own.
///
/// `M` names the index set the backend was built with. It carries no data; it
/// is the static promise the serving reads bound on.
pub struct StoreReader<B, M> {
    backend: Arc<B>,
    /// The per-pool activation schedule the treestate read renders against,
    /// learned from the validator at boot. Pinned into every snapshot.
    activations: PoolActivations,
    index_set: PhantomData<M>,
}

impl<B, M> StoreReader<B, M> {
    /// A reader over `backend`, built to the index set `M`, with no activation
    /// schedule — treestate reports every pool absent, which suits a reader that
    /// serves no treestate. The finalised tip is read live from the backend's
    /// watermark at snapshot time, not passed in.
    pub fn new(backend: Arc<B>) -> Self {
        Self::with_activations(backend, PoolActivations::unknown())
    }

    /// A reader over `backend` carrying the validator-reported activation
    /// schedule, so its treestate read tells an active-but-empty pool (serve the
    /// empty tree) from one below its activation (absent).
    pub fn with_activations(backend: Arc<B>, activations: PoolActivations) -> Self {
        Self {
            backend,
            activations,
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
            activations: self.activations,
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
        let watermark = watermark::read(&reader).ok().flatten();
        zaino_indexes::capabilities::serviceability(&reader, watermark)
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
        let activations = self.activations;
        async move {
            // Consume the writer's watermark — the finalised tip this view can
            // answer up to — and pin it. Reads through the snapshot hit live
            // backend state rather than a read transaction; that is coherent
            // because the finalised tier is append-only and every read is
            // bounded by the pinned watermark, below which nothing changes.
            let reader = backend
                .reader()
                .map_err(|e| Transient(format!("open reader: {e}")))?;
            let watermark =
                watermark::read(&reader).map_err(|e| Transient(format!("read watermark: {e}")))?;
            // Compose the tip's BlockRef on read from the headers index, so the
            // view reports a real (height, hash) rather than a bare height.
            let pinned_tip = match watermark {
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
                watermark,
                pinned_tip,
                activations,
                index_set: PhantomData,
            })
        }
    }
}

/// An immutable pinned view over a store built to `M`. Clones share the
/// backend via `Arc`.
pub struct StoreSnapshot<B, M> {
    backend: Arc<B>,
    /// The watermark this view was pinned to, read from the backend.
    watermark: Option<Height>,
    /// The block at the watermark, composed from the headers index at pin time.
    pinned_tip: Option<BlockRef>,
    /// The per-pool activation schedule, pinned from the reader.
    activations: PoolActivations,
    index_set: PhantomData<M>,
}

// Manual `Clone` so the bound is on `Arc<B>` (always cloneable), not `B`.
impl<B, M> Clone for StoreSnapshot<B, M> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            watermark: self.watermark,
            pinned_tip: self.pinned_tip,
            activations: self.activations,
            index_set: PhantomData,
        }
    }
}

impl<B: Backend + 'static, M: IndexSet> ChainSegment for StoreSnapshot<B, M> {
    fn pinned_tip(&self) -> Option<BlockRef> {
        self.pinned_tip
    }

    fn coverage(&self) -> Option<HeightRange> {
        // The finalised store serves `[genesis, watermark]`; an empty store
        // (no watermark) covers nothing.
        self.watermark.map(|end| HeightRange {
            start: Height::GENESIS,
            end,
        })
    }
}

impl<B: Backend + 'static, M: IndexSet> Snapshot for StoreSnapshot<B, M> {
    fn serviceable_range(&self) -> Option<ServiceableRange> {
        // The store alone has no window: its served tip is its watermark.
        self.watermark.map(|watermark| ServiceableRange {
            watermark: Some(watermark),
            tip: watermark,
        })
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
                    // By-hash resolution reads the `hash_to_height` index, which
                    // is `Scattered` and so may still be deferred during the
                    // initial catch-up. While it is incomplete the mapping is
                    // partial, so a miss would be a false "no such block": refuse
                    // instead. Height-addressed reads below do not touch it and
                    // serve throughout.
                    if let Some(capability) = serviceability_gate::<B>(
                        &reader,
                        &[hash_to_height::ID.into()],
                        Capability::Blocks,
                    )
                    .map_err(|e| BlockReadError::Transient(format!("by-hash readiness: {e}")))?
                    {
                        return Err(BlockReadError::NotServiceable(capability));
                    }
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
            Ok(reader) => match read_compact_range::<B>(&reader, range) {
                Ok(blocks) => blocks
                    .into_iter()
                    .map(|block| block.map_err(Into::into))
                    .collect(),
                Err(e) => vec![Err(e.into())],
            },
            Err(e) => vec![Err(ReadError::Fatal(format!("open reader: {e}")))],
        };
        Box::pin(futures::stream::iter(blocks))
    }
}

/// The header projection is backed by the same `Blocks` capability as the
/// compact block — the headers index is one of its indexes — so a store that can
/// serve a compact block can serve its header, and a [`StoreSnapshot`] is a chain
/// tier for both reads under one bound.
impl<B, M> HeaderRead for StoreSnapshot<B, M>
where
    B: Backend + 'static,
    M: Backs<local::Blocks>,
{
    fn header(
        &self,
        h: Height,
    ) -> impl Future<Output = Result<Option<HeaderSummary>, BlockReadError>> + Send {
        let backend = self.backend.clone();
        async move {
            let reader = backend
                .reader()
                .map_err(|e| BlockReadError::Fatal(format!("open reader: {e}")))?;
            // A read or decode failure propagates as an error; an absent entry is
            // a height the store does not cover (above its watermark).
            match read_present::<HeadersIndex, B>(&reader, headers::ID.into(), h)? {
                Some(header) => Ok(Some(HeaderSummary {
                    hash: header.hash,
                    time: header.time,
                })),
                None => Ok(None),
            }
        }
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
    read_keyed::<C, B>(reader, namespace, &BlockHeight::new(u64::from(height)))
}

/// Read one index's value at `key`, composing on read.
///
/// The general form of [`read_index_value`], for the indexes keyed by
/// something other than a height — an outpoint, a transaction id. Applies the
/// same codec version guard, so a format skew reads as absent rather than
/// decoding stale bytes.
fn read_keyed<C, B>(
    reader: &B::Reader,
    namespace: Namespace,
    key: &C::Key,
) -> Result<Option<C::Value>, Transient>
where
    C: EntryCodec,
    B: Backend,
{
    if freshness::<C>(reader, namespace)
        .map_err(|e| Transient(format!("freshness {}: {e}", namespace.as_str())))?
        == Freshness::Stale
    {
        return Ok(None);
    }
    let key = encode_key::<C>(key);
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

/// The store-tier serviceability gate: whether every backing namespace is
/// complete on the pinned reader, for a read whose answer is composed from
/// scattered namespaces the backend may still be building during the initial
/// bulk catch-up.
///
/// Returns `Some(capability)` when any `namespaces` entry is still incomplete
/// ([`BackendReader::is_complete`] is `false`), so the caller answers
/// `NotServiceable(capability)` rather than serving partial — or
/// empty-as-complete — data; `None` when all are complete and the read may
/// proceed. One manifest probe per namespace, checked once per read rather than
/// per entry: completeness is a property of the namespace, not of any one key.
pub(crate) fn serviceability_gate<B: Backend>(
    reader: &B::Reader,
    namespaces: &[Namespace],
    capability: Capability,
) -> Result<Option<Capability>, zaino_persistence::ReadError> {
    for namespace in namespaces {
        if !reader.is_complete(*namespace)? {
            return Ok(Some(capability));
        }
    }
    Ok(None)
}

/// The seven index values a compact block composes from, all at one height.
struct BlockParts {
    header: <HeadersIndex as EntryCodec>::Value,
    chain_metadata: <ChainMetadataIndex as EntryCodec>::Value,
    txids: <TxidsIndex as EntryCodec>::Value,
    transparent: <TransparentDataIndex as EntryCodec>::Value,
    sapling: <SaplingIndex as EntryCodec>::Value,
    orchard: <OrchardIndex as EntryCodec>::Value,
    ironwood: <IronwoodIndex as EntryCodec>::Value,
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
    let parts = BlockParts {
        header,
        chain_metadata: require::<ChainMetadataIndex, B>(
            reader,
            chain_metadata::ID.into(),
            height,
            "chain_metadata",
        )?,
        txids: require::<TxidsIndex, B>(reader, txids::ID.into(), height, "txids")?,
        transparent: require::<TransparentDataIndex, B>(
            reader,
            transparent_data::ID.into(),
            height,
            "transparent",
        )?,
        sapling: require::<SaplingIndex, B>(reader, sapling::ID.into(), height, "sapling")?,
        orchard: require::<OrchardIndex, B>(reader, orchard::ID.into(), height, "orchard")?,
        ironwood: require::<IronwoodIndex, B>(reader, ironwood::ID.into(), height, "ironwood")?,
    };
    assemble(height, parts).map(Some)
}

/// Compose every compact block in `range` on read, one range scan per index
/// rather than seven point lookups per height.
///
/// Each index is checked for freshness once and walked once over the range;
/// the columns are then paired by height and each height composed as
/// [`read_compact_block`] composes it, under the same rules: a height with no
/// header is not in the result, a header without a companion is `Fatal`, and a
/// height whose entries fail to decode is an error item in its place. A failed
/// scan fails the whole range.
fn read_compact_range<B: Backend>(
    reader: &B::Reader,
    range: HeightRange,
) -> Result<Vec<Result<CompactBlock, BlockReadError>>, BlockReadError> {
    let mut headers = scan_column::<HeadersIndex, B>(reader, headers::ID.into(), range)?;
    let mut chain_metadata =
        scan_column::<ChainMetadataIndex, B>(reader, chain_metadata::ID.into(), range)?;
    let mut txids = scan_column::<TxidsIndex, B>(reader, txids::ID.into(), range)?;
    let mut transparent =
        scan_column::<TransparentDataIndex, B>(reader, transparent_data::ID.into(), range)?;
    let mut sapling = scan_column::<SaplingIndex, B>(reader, sapling::ID.into(), range)?;
    let mut orchard = scan_column::<OrchardIndex, B>(reader, orchard::ID.into(), range)?;
    let mut ironwood = scan_column::<IronwoodIndex, B>(reader, ironwood::ID.into(), range)?;

    let mut blocks = Vec::new();
    for (slot, header) in headers.iter_mut().enumerate() {
        let Some(header) = header.take() else {
            continue;
        };
        let height = range_height(range, slot)?;
        let block = header.and_then(|header| {
            let parts = BlockParts {
                header,
                chain_metadata: companion(&mut chain_metadata, slot, height, "chain_metadata")?,
                txids: companion(&mut txids, slot, height, "txids")?,
                transparent: companion(&mut transparent, slot, height, "transparent")?,
                sapling: companion(&mut sapling, slot, height, "sapling")?,
                orchard: companion(&mut orchard, slot, height, "orchard")?,
                ironwood: companion(&mut ironwood, slot, height, "ironwood")?,
            };
            assemble(height, parts)
        });
        blocks.push(block);
    }
    Ok(blocks)
}

/// One index's entries over a height range, by offset from the range's start:
/// `None` where the index holds nothing, the decoded value or its decode error
/// where it does.
type Column<V> = Vec<Option<Result<V, BlockReadError>>>;

/// Walk one height-keyed index over `range` with a single range scan, decoding
/// each entry in place from the backend's borrowed bytes.
///
/// Applies the codec version guard once for the whole range: a stale index
/// reads as holding nothing, as [`read_index_value`] reads it per height.
fn scan_column<C, B>(
    reader: &B::Reader,
    namespace: Namespace,
    range: HeightRange,
) -> Result<Column<C::Value>, BlockReadError>
where
    C: EntryCodec<Key = BlockHeight>,
    B: Backend,
{
    let first = u32::from(range.start);
    let Some(width) = u32::from(range.end)
        .checked_sub(first)
        .and_then(|span| span.checked_add(1))
    else {
        return Ok(Vec::new());
    };
    let width = usize::try_from(width)
        .map_err(|_| BlockReadError::Fatal("range wider than the address space".to_owned()))?;
    let mut column: Column<C::Value> = std::iter::repeat_with(|| None).take(width).collect();
    if freshness::<C>(reader, namespace)
        .map_err(|e| BlockReadError::Transient(format!("freshness {}: {e}", namespace.as_str())))?
        == Freshness::Stale
    {
        return Ok(column);
    }

    let start = encode_key::<C>(&BlockHeight::new(u64::from(first)));
    let end = encode_key::<C>(&BlockHeight::new(u64::from(u32::from(range.end)) + 1));
    let mut stray_key = None;
    reader
        .scan_range(namespace, &start, &end, &mut |key, value| {
            let slot = decode_key::<C>(key)
                .ok()
                .and_then(|height| u64::from(height).checked_sub(u64::from(first)))
                .and_then(|offset| usize::try_from(offset).ok())
                .filter(|slot| *slot < width);
            let Some(slot) = slot else {
                stray_key = Some(key.to_vec());
                return ControlFlow::Break(());
            };
            column[slot] = Some(decode_value::<C>(value).map_err(|e| {
                BlockReadError::Transient(format!("decode {}: {e}", namespace.as_str()))
            }));
            ControlFlow::Continue(())
        })
        .map_err(|e| BlockReadError::Transient(format!("read {}: {e}", namespace.as_str())))?;
    match stray_key {
        Some(key) => Err(BlockReadError::Transient(format!(
            "read {}: key {key:02x?} is not a height in the scanned range",
            namespace.as_str()
        ))),
        None => Ok(column),
    }
}

/// The height at `slot` of `range`.
fn range_height(range: HeightRange, slot: usize) -> Result<Height, BlockReadError> {
    u32::try_from(slot)
        .ok()
        .and_then(|offset| u32::from(range.start).checked_add(offset))
        .and_then(|height| Height::try_from(height).ok())
        .ok_or_else(|| BlockReadError::Fatal(format!("slot {slot} is outside the range")))
}

/// Take the companion entry a present header grants at `slot`. Absent →
/// `Fatal`: the block exists but this index does not, which is corruption.
fn companion<V>(
    column: &mut Column<V>,
    slot: usize,
    height: Height,
    name: &str,
) -> Result<V, BlockReadError> {
    column
        .get_mut(slot)
        .and_then(Option::take)
        .unwrap_or_else(|| Err(missing_companion(height, name)))
}

fn missing_companion(height: Height, name: &str) -> BlockReadError {
    BlockReadError::Fatal(format!(
        "inconsistent index at height {}: header present but no {name}",
        u32::from(height),
    ))
}

/// Compose the compact block at `height` from its index values, checking the
/// per-transaction alignment the atomic commit grants.
fn assemble(height: Height, parts: BlockParts) -> Result<CompactBlock, BlockReadError> {
    let BlockParts {
        header,
        chain_metadata,
        txids,
        transparent,
        sapling,
        orchard,
        ironwood,
    } = parts;
    let (txids, transparent, sapling, orchard, ironwood) =
        (txids.0, transparent.0, sapling.0, orchard.0, ironwood.0);

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

    Ok(CompactBlock {
        hash: header.hash,
        prev_hash: header.prev_hash,
        height: u32::from(height),
        time: header.time,
        bits: header.bits,
        transactions,
        chain_metadata,
    })
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
    read_present::<C, B>(reader, namespace, height)?.ok_or_else(|| missing_companion(height, name))
}

#[cfg(test)]
mod readiness_tests {
    //! Store readiness during the initial bulk catch-up.
    //!
    //! A store whose scattered namespaces are deferred must answer
    //! `NotServiceable` for the reads they back — address history, spend status,
    //! and by-hash block resolution — never partial or empty-as-complete data,
    //! while height-addressed compact reads keep serving. After `finish_bulk`
    //! every read serves and matches a store built directly from the same
    //! commits.

    use std::sync::Arc;

    use zaino_address::transparent_address_key;
    use zaino_backend_lmdb::{LmdbBackend, LmdbConfig};
    use zaino_indexes::index_set::IndexSet;
    use zaino_indexes::indexes::address_history::{self, AddrId, AddrKey, AddressHistoryIndex};
    use zaino_indexes::indexes::chain_metadata::{self, ChainMetadataIndex};
    use zaino_indexes::indexes::hash_to_height::{self, HashToHeightIndex};
    use zaino_indexes::indexes::headers::{self, HeaderValue, HeadersIndex};
    use zaino_indexes::indexes::ironwood::{self, IronwoodBlockValue, IronwoodIndex};
    use zaino_indexes::indexes::orchard::{self, OrchardBlockValue, OrchardIndex};
    use zaino_indexes::indexes::sapling::{self, SaplingBlockValue, SaplingIndex};
    use zaino_indexes::indexes::transparent_data::{
        self, TransparentBlockValue, TransparentDataIndex,
    };
    use zaino_indexes::indexes::transparent_spends::{self, OutpointKey, TransparentSpendsIndex};
    use zaino_indexes::indexes::txid_location::{self, TxidLocationIndex};
    use zaino_indexes::indexes::txids::{self, TxidsIndex, TxidsValue};
    use zaino_indexes::sets::transparent_history::TransparentHistory;
    use zaino_persistence::{Backend, BackendWriter, BulkPolicy, NamespaceSpec, WriteOp};
    use zaino_persistence_codec::{put, reserved_namespaces, version_stamp, watermark};
    use zaino_primitives::types::{
        BlockHash, BlockSelector, ChainMetadata, CompactDifficulty, Height, HeightRange, Outpoint,
        TransactionId, TransparentAddress, Zatoshis,
    };
    use zaino_service::error::{AddressReadError, BlockReadError, SpendReadError};
    use zaino_service::SpendStatus::Spent;
    use zaino_service::{
        AddressRead, Capability, CompactBlockRead, ReadBudget, SpendRead, TakeSnapshot,
    };
    use zaino_sync::primitives::BlockHeight;

    use super::{StoreReader, StoreSnapshot};

    /// A mainnet/testnet P2PKH address that `transparent_address_key` resolves,
    /// so a receive written under its derived id is found by the read side.
    const TESTNET_P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    /// The one block height the fixture indexes.
    const H: u32 = 5;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn block_hash() -> BlockHash {
        BlockHash::from([7u8; 32])
    }

    /// The outpoint the fixture records a spend of (distinct from the address's
    /// receive, so the address balance nets no spend).
    fn spent_outpoint() -> Outpoint {
        Outpoint {
            txid: TransactionId::from([0x22u8; 32]),
            index: 0,
        }
    }

    fn spender() -> TransactionId {
        TransactionId::from([0x33u8; 32])
    }

    fn receive_value() -> Zatoshis {
        Zatoshis::new(100).expect("valid amount")
    }

    fn query_range() -> HeightRange {
        HeightRange {
            start: height(0),
            end: height(10),
        }
    }

    fn address() -> TransparentAddress {
        TransparentAddress::new(TESTNET_P2PKH.to_owned())
    }

    /// The namespace specs the runtime's `open_store` declares for this set.
    fn specs() -> Vec<NamespaceSpec> {
        TransparentHistory::pipelines()
            .namespace_specs()
            .into_iter()
            .chain(reserved_namespaces().map(NamespaceSpec::meta))
            .collect()
    }

    fn open_at(path: &std::path::Path) -> LmdbBackend {
        LmdbBackend::open(LmdbConfig {
            path: path.to_path_buf(),
            map_size_bytes: 16 << 20,
            namespaces: specs(),
        })
        .expect("open lmdb store")
    }

    /// Every op the fixture commits: version stamps, an empty-transaction compact
    /// block at height `H`, its hash→height mapping, one address receive, one
    /// recorded spend, and the watermark.
    fn store_ops() -> Vec<WriteOp> {
        let bh = BlockHeight::new(u64::from(H));
        let (script_type, hash160) =
            transparent_address_key(&address()).expect("a transparent address");

        let mut ops = vec![
            version_stamp::<HeadersIndex>(headers::ID.into()),
            version_stamp::<TxidsIndex>(txids::ID.into()),
            version_stamp::<HashToHeightIndex>(hash_to_height::ID.into()),
            version_stamp::<TransparentDataIndex>(transparent_data::ID.into()),
            version_stamp::<SaplingIndex>(sapling::ID.into()),
            version_stamp::<OrchardIndex>(orchard::ID.into()),
            version_stamp::<IronwoodIndex>(ironwood::ID.into()),
            version_stamp::<ChainMetadataIndex>(chain_metadata::ID.into()),
            version_stamp::<AddressHistoryIndex>(address_history::ID.into()),
            version_stamp::<TransparentSpendsIndex>(transparent_spends::ID.into()),
            version_stamp::<TxidLocationIndex>(txid_location::ID.into()),
        ];

        // An empty-transaction compact block: every pool present at `H` with a
        // zero-length tx list, so `read_compact_block` composes a valid block.
        ops.push(put::<HeadersIndex>(
            headers::ID.into(),
            &bh,
            &HeaderValue {
                hash: block_hash(),
                prev_hash: BlockHash::from([6u8; 32]),
                time: 3,
                bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
            },
        ));
        ops.push(put::<TxidsIndex>(
            txids::ID.into(),
            &bh,
            &TxidsValue(Vec::new()),
        ));
        ops.push(put::<TransparentDataIndex>(
            transparent_data::ID.into(),
            &bh,
            &TransparentBlockValue(Vec::new()),
        ));
        ops.push(put::<SaplingIndex>(
            sapling::ID.into(),
            &bh,
            &SaplingBlockValue(Vec::new()),
        ));
        ops.push(put::<OrchardIndex>(
            orchard::ID.into(),
            &bh,
            &OrchardBlockValue(Vec::new()),
        ));
        ops.push(put::<IronwoodIndex>(
            ironwood::ID.into(),
            &bh,
            &IronwoodBlockValue(Vec::new()),
        ));
        ops.push(put::<ChainMetadataIndex>(
            chain_metadata::ID.into(),
            &bh,
            &ChainMetadata::ZERO,
        ));

        // The by-hash mapping (scattered).
        ops.push(put::<HashToHeightIndex>(
            hash_to_height::ID.into(),
            &block_hash(),
            &bh,
        ));

        // One receive to the queried address (scattered), unspent.
        ops.push(put::<AddressHistoryIndex>(
            address_history::ID.into(),
            &AddrKey {
                addr: AddrId {
                    script_type,
                    hash: hash160,
                },
                height: bh,
                txid: TransactionId::from([0x11u8; 32]),
                output_index: 0,
            },
            &receive_value(),
        ));

        // One recorded spend of a different outpoint (scattered).
        ops.push(put::<TransparentSpendsIndex>(
            transparent_spends::ID.into(),
            &OutpointKey {
                prev_txid: spent_outpoint().txid,
                prev_index: spent_outpoint().index,
            },
            &spender(),
        ));

        ops.push(watermark::stamp(height(H)));
        ops
    }

    fn commit(backend: &LmdbBackend, ops: Vec<WriteOp>) {
        let mut writer = backend.writer().expect("writer");
        writer.commit(ops).expect("commit");
    }

    async fn snapshot(backend: Arc<LmdbBackend>) -> StoreSnapshot<LmdbBackend, TransparentHistory> {
        StoreReader::<_, TransparentHistory>::new(backend)
            .snapshot()
            .await
            .expect("snapshot")
    }

    /// In bulk mode with deferred scattered commits, the reads backed by a
    /// deferred namespace answer `NotServiceable` with the capability the spec's
    /// readiness table names; after `finish_bulk` they serve and match a store
    /// built directly from the same commits. Height-addressed compact reads serve
    /// throughout.
    #[tokio::test]
    async fn deferred_scattered_reads_are_not_serviceable_until_finish_bulk() {
        // A reference store built with plain, non-deferred commits.
        let reference_dir = tempfile::tempdir().expect("tempdir");
        let reference = Arc::new(open_at(reference_dir.path()));
        commit(&reference, store_ops());
        let reference = snapshot(reference).await;

        // The store under test, built through bulk mode.
        let deferred_dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(open_at(deferred_dir.path()));
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");
        commit(&backend, store_ops());

        // --- During bulk: deferred namespaces are not serviceable. ---
        {
            let snap = snapshot(backend.clone()).await;
            let mut budget = ReadBudget::for_request();

            // Height-addressed compact read serves (its indexes are walk-ordered).
            let by_height = snap
                .compact_block(BlockSelector::Height(height(H)))
                .await
                .expect("height-addressed compact read serves in bulk mode")
                .expect("the block is present");
            assert_eq!(by_height.height, H);

            // By-hash resolution reads the deferred `hash_to_height` index.
            assert!(
                matches!(
                    snap.compact_block(BlockSelector::Hash(block_hash())).await,
                    Err(BlockReadError::NotServiceable(Capability::Blocks))
                ),
                "by-hash block read is NotServiceable(Blocks) while hash_to_height is deferred"
            );

            // Address history is backed by three deferred namespaces.
            assert!(
                matches!(
                    snap.balance(&address(), query_range(), &mut budget).await,
                    Err(AddressReadError::NotServiceable(Capability::AddressHistory))
                ),
                "address balance is NotServiceable(AddressHistory) while its namespaces are deferred"
            );

            // Spend status is backed by two deferred namespaces.
            assert!(
                matches!(
                    snap.spend_status(spent_outpoint()).await,
                    Err(SpendReadError::NotServiceable(Capability::SpendStatus))
                ),
                "spend status is NotServiceable(SpendStatus) while its namespaces are deferred"
            );
        }

        // --- After finish_bulk: every read serves and matches the direct build. ---
        backend.finish_bulk().expect("finish_bulk");
        let snap = snapshot(backend.clone()).await;
        let mut budget = ReadBudget::for_request();
        let mut reference_budget = ReadBudget::for_request();

        let balance = snap
            .balance(&address(), query_range(), &mut budget)
            .await
            .expect("address balance serves after finish_bulk");
        assert_eq!(balance.balance, receive_value());
        assert_eq!(
            balance,
            reference
                .balance(&address(), query_range(), &mut reference_budget)
                .await
                .expect("reference balance"),
            "the deferred build's balance matches the direct build"
        );

        let spend = snap
            .spend_status(spent_outpoint())
            .await
            .expect("spend status serves after finish_bulk");
        assert_eq!(spend, Spent { by: spender() });
        assert_eq!(
            spend,
            reference
                .spend_status(spent_outpoint())
                .await
                .expect("reference spend status"),
            "the deferred build's spend status matches the direct build"
        );

        let by_hash = snap
            .compact_block(BlockSelector::Hash(block_hash()))
            .await
            .expect("by-hash compact read serves after finish_bulk")
            .expect("the block resolves by hash");
        assert_eq!(by_hash.height, H);
        assert_eq!(by_hash.hash, block_hash());
    }

    /// `spend_info` goes through the same readiness gate as `spend_status`:
    /// `NotServiceable(SpendStatus)` while its scattered namespaces are deferred,
    /// and past the gate once `finish_bulk` completes them. (The deeper spend
    /// resolution `spend_info` then performs is exercised elsewhere; here the
    /// point is only that the gate flips.)
    /// The store locates a transaction it holds on its best chain at the indexed
    /// height, answers `None` for one it does not hold, and — while the
    /// scattered `txid_location` index is still deferred — refuses the read as
    /// not serviceable, where answering `None` would be a false miss.
    #[tokio::test]
    async fn transaction_location_is_gated_and_then_located() {
        use zaino_primitives::types::TransactionLocation;
        use zaino_service::error::TxReadError;
        use zaino_service::TransactionLocationRead;

        // The shared fixture records a spend by `spender()` but no location for
        // it, so this test adds the entry it reads.
        let mut ops = store_ops();
        ops.push(put::<TxidLocationIndex>(
            txid_location::ID.into(),
            &spender(),
            &txid_location::TxLocation {
                height: BlockHeight::new(u64::from(H)),
                tx_index: 0,
            },
        ));

        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(open_at(dir.path()));
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");
        commit(&backend, ops);

        let during = snapshot(backend.clone())
            .await
            .transaction_location(spender())
            .await;
        assert!(
            matches!(
                during,
                Err(TxReadError::NotServiceable(Capability::TransactionLocation))
            ),
            "a deferred index is not serviceable, never a miss: {during:?}"
        );

        backend.finish_bulk().expect("finish_bulk");
        let snap = snapshot(backend).await;
        assert_eq!(
            snap.transaction_location(spender())
                .await
                .expect("the read serves"),
            Some(TransactionLocation::BestChain(height(H)))
        );
        assert_eq!(
            snap.transaction_location(TransactionId::from([0x44u8; 32]))
                .await
                .expect("the read serves"),
            None
        );
    }

    #[tokio::test]
    async fn spend_info_is_gated_and_then_ungated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Arc::new(open_at(dir.path()));
        backend
            .begin_bulk(BulkPolicy { enabled: true })
            .expect("begin_bulk");
        commit(&backend, store_ops());

        {
            let snap = snapshot(backend.clone()).await;
            assert!(
                matches!(
                    snap.spend_info(spent_outpoint()).await,
                    Err(SpendReadError::NotServiceable(Capability::SpendStatus))
                ),
                "spend_info is NotServiceable while transparent_spends is deferred"
            );
        }

        backend.finish_bulk().expect("finish_bulk");
        let snap = snapshot(backend).await;
        assert!(
            !matches!(
                snap.spend_info(spent_outpoint()).await,
                Err(SpendReadError::NotServiceable(_))
            ),
            "the readiness gate is lifted after finish_bulk"
        );
    }
}

#[cfg(test)]
mod compact_range_tests {
    //! The range read composes each height exactly as the point read does:
    //! the same blocks, the same gaps, the same corruption verdicts.

    use zaino_indexes::indexes::chain_metadata::{self, ChainMetadataIndex};
    use zaino_indexes::indexes::headers::{self, HeaderValue, HeadersIndex};
    use zaino_indexes::indexes::ironwood::{self, IronwoodBlockValue, IronwoodIndex};
    use zaino_indexes::indexes::orchard::{self, OrchardBlockValue, OrchardIndex};
    use zaino_indexes::indexes::sapling::{self, SaplingBlockValue, SaplingIndex};
    use zaino_indexes::indexes::transparent_data::{
        self, TransparentBlockValue, TransparentDataIndex,
    };
    use zaino_indexes::indexes::txids::{self, TxidsIndex, TxidsValue};
    use zaino_persistence::in_memory::InMemoryBackend;
    use zaino_persistence::{Backend, BackendWriter, Namespace, WriteOp};
    use zaino_persistence_codec::{put, version_stamp, EntryCodec};
    use zaino_primitives::types::{
        BlockHash, ChainMetadata, CompactDifficulty, Height, HeightRange, TransactionId,
    };
    use zaino_sync::primitives::BlockHeight;

    use super::{read_compact_block, read_compact_range};

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("valid test height")
    }

    fn range(start: u32, end: u32) -> HeightRange {
        HeightRange {
            start: height(start),
            end: height(end),
        }
    }

    /// Each index's namespace and its format-version stamp.
    fn stamps() -> [(Namespace, WriteOp); 7] {
        fn stamp<C: EntryCodec>(namespace: Namespace) -> (Namespace, WriteOp) {
            (namespace, version_stamp::<C>(namespace))
        }
        [
            stamp::<HeadersIndex>(headers::ID.into()),
            stamp::<ChainMetadataIndex>(chain_metadata::ID.into()),
            stamp::<TxidsIndex>(txids::ID.into()),
            stamp::<TransparentDataIndex>(transparent_data::ID.into()),
            stamp::<SaplingIndex>(sapling::ID.into()),
            stamp::<OrchardIndex>(orchard::ID.into()),
            stamp::<IronwoodIndex>(ironwood::ID.into()),
        ]
    }

    /// Every index entry of an empty-transaction block at `h`, except `skip`.
    fn block_ops(h: u32, skip: Option<&str>) -> Vec<WriteOp> {
        let bh = BlockHeight::new(u64::from(h));
        let seed = u8::try_from(h).expect("small test height");
        let entries = [
            (
                "headers",
                put::<HeadersIndex>(
                    headers::ID.into(),
                    &bh,
                    &HeaderValue {
                        hash: BlockHash::from([seed; 32]),
                        prev_hash: BlockHash::from([seed.wrapping_sub(1); 32]),
                        time: h,
                        bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                    },
                ),
            ),
            (
                "chain_metadata",
                put::<ChainMetadataIndex>(chain_metadata::ID.into(), &bh, &ChainMetadata::ZERO),
            ),
            (
                "txids",
                put::<TxidsIndex>(txids::ID.into(), &bh, &TxidsValue(Vec::new())),
            ),
            (
                "transparent",
                put::<TransparentDataIndex>(
                    transparent_data::ID.into(),
                    &bh,
                    &TransparentBlockValue(Vec::new()),
                ),
            ),
            (
                "sapling",
                put::<SaplingIndex>(sapling::ID.into(), &bh, &SaplingBlockValue(Vec::new())),
            ),
            (
                "orchard",
                put::<OrchardIndex>(orchard::ID.into(), &bh, &OrchardBlockValue(Vec::new())),
            ),
            (
                "ironwood",
                put::<IronwoodIndex>(ironwood::ID.into(), &bh, &IronwoodBlockValue(Vec::new())),
            ),
        ];
        entries
            .into_iter()
            .filter(|(name, _)| Some(*name) != skip)
            .map(|(_, op)| op)
            .collect()
    }

    /// Heights 1, 3 and 6 hold whole blocks; 2 holds nothing; 4 has a header
    /// but no sapling entry; 5 lists a txid its pools do not. Every index is
    /// stamped with its format version except those named in `unstamped`.
    fn backend(unstamped: &[Namespace]) -> InMemoryBackend {
        let backend = InMemoryBackend::new();
        let mut ops: Vec<WriteOp> = stamps()
            .into_iter()
            .filter(|(namespace, _)| !unstamped.contains(namespace))
            .map(|(_, stamp)| stamp)
            .collect();
        for h in [1, 3, 6] {
            ops.extend(block_ops(h, None));
        }
        ops.extend(block_ops(4, Some("sapling")));
        ops.extend(block_ops(5, Some("txids")));
        ops.push(put::<TxidsIndex>(
            txids::ID.into(),
            &BlockHeight::new(5),
            &TxidsValue(vec![TransactionId::from([5u8; 32])]),
        ));
        backend
            .writer()
            .expect("a writer")
            .commit(ops)
            .expect("commit the fixture");
        backend
    }

    /// The point read over each height of `range`, absent heights dropped:
    /// what the range read must equal.
    fn point_reads(reader: &<InMemoryBackend as Backend>::Reader, range: HeightRange) -> String {
        let blocks: Vec<_> = (u32::from(range.start)..=u32::from(range.end))
            .filter_map(|h| read_compact_block::<InMemoryBackend>(reader, height(h)).transpose())
            .collect();
        format!("{blocks:?}")
    }

    #[test]
    fn a_range_read_matches_the_point_reads() {
        let backend = backend(&[]);
        let reader = backend.reader().expect("a reader");
        for (start, end) in [(0, 7), (1, 1), (2, 2), (3, 6), (4, 5), (7, 9)] {
            let ranged = read_compact_range::<InMemoryBackend>(&reader, range(start, end))
                .expect("the scans succeed");
            assert_eq!(
                format!("{ranged:?}"),
                point_reads(&reader, range(start, end)),
                "range {start}..={end}"
            );
        }
        let whole =
            read_compact_range::<InMemoryBackend>(&reader, range(0, 7)).expect("the scans succeed");
        let verdicts: Vec<_> = whole.iter().map(Result::is_ok).collect();
        assert_eq!(verdicts, [true, true, false, false, true]);
    }

    #[test]
    fn a_stale_header_index_serves_no_blocks() {
        let backend = backend(&[headers::ID.into()]);
        let reader = backend.reader().expect("a reader");
        let ranged =
            read_compact_range::<InMemoryBackend>(&reader, range(0, 7)).expect("the scans succeed");
        assert!(ranged.is_empty());
        assert_eq!(point_reads(&reader, range(0, 7)), "[]");
    }
}
