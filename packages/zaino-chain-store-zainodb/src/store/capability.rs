//! Capability model, versioned metadata, and DB trait surface
//!
//! This file defines the **capability- and version-aware interface** that all `FinalisedState` database
//! implementations must conform to.
//!
//! The core idea is:
//! - Each concrete DB major version (e.g. `DbV1`) implements a common set of traits.
//! - A `Capability` bitmap declares which parts of that trait surface are actually supported.
//! - The router (`Router`) and reader (`DbReader`) use *single-feature* requests
//!   (`CapabilityRequest`) to route a call to a backend that is guaranteed to support it.
//!
//! This design enables:
//! - serving reads from the ephemeral passthrough while the database builds,
//! - and gating API features cleanly when a backend does not support an extension.
//!
//! # What’s in this file
//!
//! ## Capability / routing types
//! - [`Capability`]: bitflags describing what an *open* database instance can serve.
//! - [`CapabilityRequest`]: a single-feature request (non-composite) used for routing.
//!
//! ## Metadata
//! - [`DbMetadata`]: persisted singleton stored under the fixed key `"metadata"` in the LMDB
//!   metadata database. It holds the 32-byte schema hash that the store computes from its
//!   canonical encodings, tables, and enabled features.
//!
//! ## Trait surface
//! This file defines:
//!
//! - **Core traits** implemented by every DB version:
//!   - [`DbRead`], [`DbWrite`], and [`DbCore`]
//!
//! - **Extension traits** implemented by *some* versions:
//!   - [`BlockCoreExt`], [`BlockTransparentExt`], [`BlockShieldedExt`]
//!   - [`CompactBlockExt`]
//!   - [`IndexedBlockExt`]
//!   - [`TransparentHistExt`]
//!
//! Extension traits must be capability-gated: if a DB does not advertise the corresponding capability
//! bit, routing must not hand that backend out for that request.
//!
//! # Development: adding or changing features safely
//!
//! When adding a new feature/query that requires new persistent data:
//!
//! 1. Add a new capability bit to [`Capability`].
//! 2. Add a corresponding variant to [`CapabilityRequest`] and map it in:
//!    - `as_capability()`
//!    - `name()`
//! 3. Add a new extension trait (or extend an existing one) that expresses the required operations.
//! 4. Implement the extension trait for the latest DB version(s).
//! 5. Add the new capability to [`Capability::LATEST`].
//! 6. Route it through `DbReader` by requesting the new `CapabilityRequest`.
//!
//! Changing a persisted format changes the computed schema hash, and every existing database
//! rebuilds.

use core::fmt;

use crate::codec::{read_fixed_le, write_fixed_le, DbCodec, FixedEncodedLen};
use crate::error::StoreError;
use crate::stream::CompactBlockStream;
use crate::support::SendFut;
use crate::types::{
    db::metadata::FinalisedTxOutSetInfoAccumulator, AbsoluteChainWork, BlockHash, BlockHeaderData,
    CommitmentTreeData, Height, IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint,
    SaplingCompactTx, SaplingTxList, TransactionHash, TransparentCompactTx, TransparentTxList,
    TxLocation, TxOutCompact, TxidList,
};
use zaino_status::StatusType;

#[cfg(feature = "transparent_address_history_experimental")]
use crate::types::{AddrEventBytes, AddrScript};

use bitflags::bitflags;
use corez::io::{self, Read, Write};
use zaino_proto::proto::utils::PoolTypeFilter;

// ***** Capability definition structs *****

bitflags! {
    /// Capability bitmap describing what an **open** database instance can serve.
    ///
    /// A capability is an *implementation promise*: if a backend advertises a capability bit, then
    /// the corresponding trait surface must be fully and correctly implemented for that backend’s
    /// on-disk schema.
    ///
    /// ## How capabilities are used
    /// - [`crate::store::router::Router`] holds a primary and optional ephemeral
    ///   backend and uses masks to decide which backend may serve a given feature.
    /// - [`crate::store::reader::DbReader`] requests capabilities via
    ///   [`CapabilityRequest`] (single-feature requests) and therefore obtains a backend that is
    ///   guaranteed to support the requested operation.
    ///
    /// ## Extension trait mapping
    /// Each bit corresponds 1-for-1 with a trait surface:
    /// - `READ_CORE` / `WRITE_CORE` correspond to [`DbRead`] / [`DbWrite`]
    /// - all other bits correspond to extension traits (e.g. [`BlockCoreExt`], [`TransparentHistExt`])
    #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
    pub(crate) struct Capability: u32 {
        /* ------ core database functionality ------ */

        /// Backend advertises no supported capability bits.
        const NONE                  = 0;

        /// Backend implements [`DbRead`].
        ///
        /// This includes:
        /// - tip height (`db_height`)
        /// - hash↔height lookups
        /// - reading the persisted metadata singleton.
        const READ_CORE             = 0b0000_0001;

        /// Backend implements [`DbWrite`].
        ///
        /// This includes:
        /// - appending tip blocks,
        /// - deleting tip blocks,
        /// - and updating the metadata singleton.
        const WRITE_CORE            = 0b0000_0010;

        /* ---------- database extensions ---------- */

        /// Backend implements [`BlockCoreExt`] (header/txid and tx-index lookups).
        const BLOCK_CORE_EXT        = 0b0000_0100;

        /// Backend implements [`BlockTransparentExt`] (transparent per-block/per-tx data).
        const BLOCK_TRANSPARENT_EXT = 0b0000_1000;

        /// Backend implements [`BlockShieldedExt`] (sapling/orchard per-block/per-tx data).
        const BLOCK_SHIELDED_EXT    = 0b0001_0000;

        /// Backend implements [`CompactBlockExt`] (CompactBlock materialization).
        const COMPACT_BLOCK_EXT     = 0b0010_0000;

        /// Backend implements [`IndexedBlockExt`] (full `IndexedBlock` materialization).
        const CHAIN_BLOCK_EXT       = 0b0100_0000;

        /// Backend implements [`TransparentHistExt`] (transparent address history indices).
        ///
        /// Address history only. It used to also stand for the spent-output
        /// index and the txout-set accumulator, which are neither address
        /// history nor experimental — see [`Capability::SPENT_OUTPUT_INDEX`].
        const TRANSPARENT_HIST_INDEX = 0b1000_0000;

        /// Backend implements [`SpentOutputExt`] (which transaction spent an outpoint).
        ///
        /// Split out of `TRANSPARENT_HIST_EXT`, which conflated three things.
        /// The spent index is built unconditionally from schema v1.2 onward and
        /// has nothing to do with address history; routing it through a bit
        /// named after an experimental feature meant a build without that
        /// feature advertised a capability under a name that implied otherwise.
        const SPENT_OUTPUT_INDEX    = 0b0001_0000_0000;

        /// Backend implements [`TxOutSetExt`] (the UTXO-set accumulator).
        ///
        /// Separate from [`Capability::SPENT_OUTPUT_INDEX`] because it is a
        /// separate persisted row that a backend could maintain without the
        /// other, and because its correctness condition is different: the
        /// accumulator is a running fold, so a backend that has one is claiming
        /// it has been maintained across every write, not merely that a table
        /// exists.
        const TXOUT_SET_INDEX       = 0b0010_0000_0000;
    }
}

impl Capability {
    /// Every capability a fresh database at the latest schema serves, except
    /// address history.
    ///
    /// Split from [`Capability::LATEST`] so the address-history bit is added in
    /// exactly one place rather than being repeated in two `cfg` arms.
    const LATEST_WITHOUT_ADDRESS_HISTORY: Capability = Capability::READ_CORE
        .union(Capability::WRITE_CORE)
        .union(Capability::BLOCK_CORE_EXT)
        .union(Capability::BLOCK_TRANSPARENT_EXT)
        .union(Capability::BLOCK_SHIELDED_EXT)
        .union(Capability::COMPACT_BLOCK_EXT)
        .union(Capability::CHAIN_BLOCK_EXT)
        .union(Capability::SPENT_OUTPUT_INDEX)
        .union(Capability::TXOUT_SET_INDEX);

    /// Capability set supported by a **fresh** database at the latest major schema
    /// supported by this build.
    ///
    /// The expected modern baseline for new database instances. It must remain in sync
    /// with the latest on-disk schema (`DbV1` today).
    ///
    /// This arm: address history is compiled in, so a fresh database serves it.
    #[cfg(feature = "transparent_address_history_experimental")]
    pub(crate) const LATEST: Capability =
        Capability::LATEST_WITHOUT_ADDRESS_HISTORY.union(Capability::TRANSPARENT_HIST_INDEX);

    /// As above, but address history is not compiled in, so no database can
    /// serve it — the reads do not exist in this build.
    #[cfg(not(feature = "transparent_address_history_experimental"))]
    pub(crate) const LATEST: Capability = Capability::LATEST_WITHOUT_ADDRESS_HISTORY;

    /// Returns `true` if `self` includes **all** bits from `other`.
    ///
    /// This is primarily used for feature gating and routing assertions.
    #[inline]
    pub(crate) const fn has(self, other: Capability) -> bool {
        self.contains(other)
    }
}

/// A *single-feature* capability request used for routing.
///
/// `CapabilityRequest` values are intentionally non-composite: each variant maps to exactly one
/// [`Capability`] bit. This keeps routing and error reporting unambiguous.
///
/// The router uses the request to select a backend that advertises the requested capability.
/// If no backend advertises the capability, the call must fail with
/// [`StoreError::FeatureUnavailable`].
// `pub` (not `pub(crate)`) for the same reason as [`DbMetadata`]: it is carried
// by [`StoreError::FeatureUnavailable`], and `error` is a `pub` module, so the
// rustc `private_interfaces` check requires the variant's type to be at least as
// visible as the variant. The `capability` module is itself `pub(crate)`, so
// this does not widen the type beyond the crate; it only satisfies that check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityRequest {
    /// Request the [`DbRead`] core surface.
    ReadCore,

    /// Request the [`DbWrite`] core surface.
    WriteCore,

    /// Request the [`BlockCoreExt`] extension surface.
    BlockCoreExt,

    /// Request the [`BlockTransparentExt`] extension surface.
    BlockTransparentExt,

    /// Request the [`BlockShieldedExt`] extension surface.
    BlockShieldedExt,

    /// Request the [`CompactBlockExt`] extension surface.
    CompactBlockExt,

    /// Request the [`IndexedBlockExt`] extension surface.
    IndexedBlockExt,

    /// Request the [`TransparentHistExt`] extension surface.
    TransparentHistIndex,

    /// Request the [`SpentOutputExt`] extension surface.
    SpentOutputIndex,

    /// Request the [`TxOutSetExt`] extension surface.
    TxOutSetIndex,
}

impl CapabilityRequest {
    /// Maps this request to the corresponding single-bit [`Capability`].
    ///
    /// This mapping must remain 1-for-1 with:
    /// - the definitions in [`Capability`], and
    /// - the human-readable names returned by [`CapabilityRequest::name`].
    #[inline]
    pub(crate) const fn as_capability(self) -> Capability {
        match self {
            CapabilityRequest::ReadCore => Capability::READ_CORE,
            CapabilityRequest::WriteCore => Capability::WRITE_CORE,
            CapabilityRequest::BlockCoreExt => Capability::BLOCK_CORE_EXT,
            CapabilityRequest::BlockTransparentExt => Capability::BLOCK_TRANSPARENT_EXT,
            CapabilityRequest::BlockShieldedExt => Capability::BLOCK_SHIELDED_EXT,
            CapabilityRequest::CompactBlockExt => Capability::COMPACT_BLOCK_EXT,
            CapabilityRequest::IndexedBlockExt => Capability::CHAIN_BLOCK_EXT,
            CapabilityRequest::TransparentHistIndex => Capability::TRANSPARENT_HIST_INDEX,
            CapabilityRequest::SpentOutputIndex => Capability::SPENT_OUTPUT_INDEX,
            CapabilityRequest::TxOutSetIndex => Capability::TXOUT_SET_INDEX,
        }
    }

    /// Returns a stable human-friendly feature name for errors and logs.
    ///
    /// This value is used in [`StoreError::FeatureUnavailable`] and must remain stable
    /// across refactors to avoid confusing diagnostics.
    #[inline]
    pub(crate) const fn name(self) -> &'static str {
        match self {
            CapabilityRequest::ReadCore => "READ_CORE",
            CapabilityRequest::WriteCore => "WRITE_CORE",
            CapabilityRequest::BlockCoreExt => "BLOCK_CORE_EXT",
            CapabilityRequest::BlockTransparentExt => "BLOCK_TRANSPARENT_EXT",
            CapabilityRequest::BlockShieldedExt => "BLOCK_SHIELDED_EXT",
            CapabilityRequest::CompactBlockExt => "COMPACT_BLOCK_EXT",
            CapabilityRequest::IndexedBlockExt => "CHAIN_BLOCK_EXT",
            CapabilityRequest::TransparentHistIndex => "TRANSPARENT_HIST_INDEX",
            CapabilityRequest::SpentOutputIndex => "SPENT_OUTPUT_INDEX",
            CapabilityRequest::TxOutSetIndex => "TXOUT_SET_INDEX",
        }
    }
}

/// Renders the stable feature name, so [`StoreError::FeatureUnavailable`] and
/// logs share the vocabulary routing uses.
impl fmt::Display for CapabilityRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Convenience conversion from a routing request to its single-bit capability.
impl From<CapabilityRequest> for Capability {
    #[inline]
    fn from(req: CapabilityRequest) -> Self {
        req.as_capability()
    }
}

// ***** Database metadata structs *****

/// The metadata singleton, which records the schema hash of the build that created the database.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Eq, Hash, Default)]
#[cfg_attr(test, derive(serde::Serialize, serde::Deserialize))]
// `pub` (not `pub(crate)`) so it matches the visibility of the `pub` capability
// traits in this module that expose it (e.g. `DbRead::get_metadata`). The
// `capability` module is itself `pub(crate)`, so this does not widen the type
// beyond the crate; it only resolves the rustc-1.96 E0446 private-in-public
// check on `DbRead::get_metadata`'s signature.
pub struct DbMetadata {
    /// The schema hash of the build that created the database.
    pub(crate) schema_hash: [u8; 32],
}

impl DbMetadata {
    /// Constructs a metadata record carrying `schema_hash`.
    pub(crate) fn new(schema_hash: [u8; 32]) -> Self {
        Self { schema_hash }
    }
}

/// On-disk encoding for the metadata singleton: the 32-byte schema hash.
impl DbCodec for DbMetadata {
    fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_fixed_le::<32, _>(w, &self.schema_hash)
    }

    fn decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let schema_hash = read_fixed_le::<32, _>(r)?;
        Ok(DbMetadata { schema_hash })
    }
}

impl FixedEncodedLen for DbMetadata {
    /// The record is the 32-byte schema hash.
    const ENCODED_LEN: usize = 32;
}

// ***** Core Database functionality *****

/// Core read-only operations that *every* database schema version must support.
///
/// These operations form the minimum required surface for:
/// - determining the chain tip stored on disk,
/// - mapping hashes to heights and vice versa,
/// - and reading the persisted schema metadata.
///
/// All methods must be consistent with the database’s *finalised* chain view.
pub trait DbRead: Send + Sync {
    /// Returns the highest block height stored, or `None` if the database is empty.
    ///
    /// Implementations must treat the stored height as the authoritative tip for all other core
    /// lookups.
    fn db_height(&self) -> impl SendFut<Result<Option<Height>, StoreError>>;

    /// Returns the height for `hash` if present.
    ///
    /// Returns:
    /// - `Ok(Some(height))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    fn get_block_height(&self, hash: BlockHash)
        -> impl SendFut<Result<Option<Height>, StoreError>>;

    /// Returns the hash for `height` if present.
    ///
    /// Returns:
    /// - `Ok(Some(hash))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    fn get_block_hash(&self, height: Height)
        -> impl SendFut<Result<Option<BlockHash>, StoreError>>;

    /// Returns the persisted metadata singleton.
    ///
    /// This must reflect the schema actually used by the backend instance.
    fn get_metadata(&self) -> impl SendFut<Result<DbMetadata, StoreError>>;
}

/// Core write operations that *every* database schema version must support.
///
/// The finalised database is updated using *stack semantics*:
/// - blocks are appended at the tip (`write_block`),
/// - and removed only from the tip (`delete_block_at_height` / `delete_block`).
///
/// Implementations must keep all secondary indices internally consistent with these operations.
pub trait DbWrite: Send + Sync {
    /// Appends a fully-validated block to the database.
    ///
    /// Invariant: `block` must be the next height after the current tip (no gaps, no rewrites).
    fn write_block(
        &self,
        block: IndexedBlock<AbsoluteChainWork>,
    ) -> impl SendFut<Result<(), StoreError>>;

    /// Ingests blocks from `source`, writing every height from the current tip up to and including
    /// `height` in order.
    ///
    /// This is the bulk catch-up path. Implementations own the ingestion loop so they can choose an
    /// efficient strategy: the v1 backend defers expensive secondary-index maintenance (the
    /// txout-set accumulator) across the run and rebuilds it once at the tip, whereas legacy
    /// backends may simply loop [`DbWrite::write_block`]. A no-op is valid when the tip already
    /// meets or exceeds `height`.
    fn write_blocks_to_height<S: zaino_chain_store::ChainStoreSource>(
        &self,
        height: Height,
        source: &S,
    ) -> impl SendFut<Result<(), StoreError>>;

    /// Deletes the tip block identified by `height` from every finalised table.
    ///
    /// Invariant: `height` must be the current database tip height.
    fn delete_block_at_height(&self, height: Height) -> impl SendFut<Result<(), StoreError>>;

    /// Deletes the provided tip block from every finalised table.
    ///
    /// This is the “full-information” deletion path: it takes an [`IndexedBlock`] so the backend
    /// can deterministically remove all derived index entries even if reconstructing them from
    /// height alone is not possible.
    ///
    /// Invariant: `block` must be the current database tip block.
    fn delete_block(&self, block: &IndexedBlock) -> impl SendFut<Result<(), StoreError>>;
}

/// Core runtime surface implemented by every backend instance.
///
/// This trait binds together:
/// - the core read/write operations, and
/// - lifecycle and status reporting for background tasks.
///
/// In practice, [`crate::store::router::Router`] implements this by
/// delegating to the currently routed core backend(s).
pub trait DbCore: DbRead + DbWrite + Send + Sync {
    /// Returns the current runtime status (`Starting`, `Syncing`, `Ready`, …).
    fn status(&self) -> StatusType;

    /// Initiates a graceful shutdown of background tasks and closes database resources.
    fn shutdown(&self) -> impl SendFut<Result<(), StoreError>>;
}

// ***** Database Extension traits *****

/// Core block indexing extension.
///
/// This extension covers header and txid range fetches plus transaction indexing by [`TxLocation`].
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise [`Capability::BLOCK_CORE_EXT`].
pub trait BlockCoreExt: Send + Sync {
    /// Return block header data by height.
    fn get_block_header(&self, height: Height)
        -> impl SendFut<Result<BlockHeaderData, StoreError>>;

    /// Returns block headers for the inclusive range `[start, end]`.
    ///
    /// Callers should ensure `start <= end`.
    fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<BlockHeaderData>, StoreError>>;

    /// Return block txids by height.
    fn get_block_txids(&self, height: Height) -> impl SendFut<Result<TxidList, StoreError>>;

    /// Return block txids for the given height range.
    ///
    /// Callers should ensure `start <= end`.
    fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<TxidList>, StoreError>>;

    /// Returns the transaction hash for the given [`TxLocation`].
    ///
    /// `TxLocation` is the internal transaction index key used by the database.
    fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<TransactionHash, StoreError>>;

    /// Returns the [`TxLocation`] for `txid` if the transaction is indexed.
    ///
    /// Returns:
    /// - `Ok(Some(location))` if indexed,
    /// - `Ok(None)` if not present (not an error).
    ///
    /// NOTE: transaction data is indexed by TxLocation internally.
    fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> impl SendFut<Result<Option<TxLocation>, StoreError>>;
}

/// Transparent transaction indexing extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::BLOCK_TRANSPARENT_EXT`].
pub trait BlockTransparentExt: Send + Sync {
    /// Returns the serialized [`TransparentCompactTx`] for `tx_location`, if present.
    ///
    /// Returns:
    /// - `Ok(Some(tx))` if present,
    /// - `Ok(None)` if not present (not an error).
    fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<TransparentCompactTx>, StoreError>>;

    /// Fetch block transparent transaction data for given block height.
    fn get_block_transparent(
        &self,
        height: Height,
    ) -> impl SendFut<Result<TransparentTxList, StoreError>>;

    /// Returns transparent transaction tx data for the inclusive block height range `[start, end]`.
    fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<TransparentTxList>, StoreError>>;

    /// Returns the [`TxOutCompact`] referenced by `outpoint`, looking up the previous
    /// transaction's transparent data via the txid index and the transparent block table.
    ///
    /// Returns an error if the previous transaction is not indexed by the finalised state
    /// or the requested output index is out of range.
    fn get_previous_output(
        &self,
        outpoint: Outpoint,
    ) -> impl SendFut<Result<TxOutCompact, StoreError>>;
}

/// Shielded transaction indexing extension (Sapling + Orchard + commitment tree data).
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::BLOCK_SHIELDED_EXT`].
pub trait BlockShieldedExt: Send + Sync {
    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<SaplingCompactTx>, StoreError>>;

    /// Fetch block sapling transaction data by height.
    fn get_block_sapling(&self, height: Height) -> impl SendFut<Result<SaplingTxList, StoreError>>;

    /// Fetches block sapling tx data for the given (inclusive) height range.
    fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<SaplingTxList>, StoreError>>;

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<OrchardCompactTx>, StoreError>>;

    /// Fetch block orchard transaction data by height.
    fn get_block_orchard(&self, height: Height) -> impl SendFut<Result<OrchardTxList, StoreError>>;

    /// Fetches block orchard tx data for the given (inclusive) height range.
    fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<OrchardTxList>, StoreError>>;

    /// Fetch the serialized Ironwood (NU6.3) compact tx for the given TxLocation, if present.
    ///
    /// Ironwood actions are modelled with the Orchard compact types. Returns `None` when the block
    /// has no ironwood row (any block below NU6.3 activation, or written before schema v1.3.0).
    fn get_ironwood(
        &self,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<OrchardCompactTx>, StoreError>>;

    /// Fetch block ironwood transaction data by height.
    ///
    /// Returns an empty [`OrchardTxList`] when the block has no ironwood row.
    fn get_block_ironwood(&self, height: Height)
        -> impl SendFut<Result<OrchardTxList, StoreError>>;

    /// Fetches block ironwood tx data for the given (inclusive) height range.
    ///
    /// Heights with no ironwood row yield an empty [`OrchardTxList`].
    fn get_block_range_ironwood(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<OrchardTxList>, StoreError>>;

    /// Fetch block commitment tree data by height.
    fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> impl SendFut<Result<CommitmentTreeData, StoreError>>;

    /// Fetches block commitment tree data for the given (inclusive) height range.
    fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<CommitmentTreeData>, StoreError>>;
}

/// CompactBlock materialization extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::COMPACT_BLOCK_EXT`].
pub trait CompactBlockExt: Send + Sync {
    /// Returns the compact block at `height`.
    ///
    /// A domain block, not a wire message. It is *dense*: one entry per
    /// transaction in the block, including transactions with nothing in any
    /// requested pool, so a transaction's position in the result is its
    /// position in the block. The wire form omits the empty ones, and the
    /// conversion to it does that — see
    /// [`compact_block_to_wire`](crate::conversion::compact_block_to_wire).
    ///
    /// The filter is still pushed down: a pool it excludes is not read from
    /// disk at all, which is where the saving is.
    fn get_compact_block(
        &self,
        height: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> impl SendFut<Result<zaino_primitives::types::CompactBlock, StoreError>>;

    /// Returns every compact block in `start..=end`, ascending.
    ///
    /// The range primitive: a backend answers under one read transaction, so
    /// the blocks are coherent with each other and the per-block transaction
    /// cost is paid once. A missing height is an error, not a skip.
    fn get_compact_block_range(
        &self,
        start: Height,
        end: Height,
        pool_types: zaino_chain_store::PoolFilter,
    ) -> impl SendFut<Result<Vec<zaino_primitives::types::CompactBlock>, StoreError>>;

    fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> impl SendFut<Result<CompactBlockStream, StoreError>>;
}

/// `IndexedBlock` materialization extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::CHAIN_BLOCK_EXT`].
pub trait IndexedBlockExt: Send + Sync {
    /// Returns the [`IndexedBlock`] for `height`, if present.
    ///
    /// Returns:
    /// - `Ok(Some(block))` if present,
    /// - `Ok(None)` if not present (not an error).
    ///
    fn get_chain_block(
        &self,
        height: Height,
    ) -> impl SendFut<Result<Option<IndexedBlock>, StoreError>>;

    /// Returns every [`IndexedBlock`] in `start..=end`, ascending.
    ///
    /// The range primitive, and the reason there is no batching helper built on
    /// [`Self::get_chain_block`]: a backend answers a range under one read
    /// transaction, so the blocks are coherent with each other, and the
    /// per-block transaction and validation costs are paid once.
    ///
    /// A missing height in the middle of the range is an error. The finalised
    /// state is contiguous, so a hole means corruption rather than a branch,
    /// and returning a short range would look to a caller like the chain ends
    /// there.
    fn get_chain_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> impl SendFut<Result<Vec<IndexedBlock>, StoreError>>;
}

/// One unspent output found by an address-history range query: where the
/// transaction sits, which output of it, and its value.
///
/// A named alias rather than a bare tuple repeated at five signatures — the
/// positions are not self-describing, and a `u16` beside a `u64` invites being
/// swapped.
#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) type AddrUtxo = (TxLocation, u16, u64);

/// Transparent address history indexing extension.
///
/// This extension provides address-scoped queries backed by persisted indices built from the
/// transparent transaction graph (outputs, spends, and derived address events).
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::TRANSPARENT_HIST_INDEX`].
///
/// Range semantics:
/// - Methods that accept `start_height` and `end_height` interpret the range as inclusive:
///   `[start_height, end_height]`
// `pub(crate)`, unlike its eight sibling traits, because two of its methods
// return `AddrEventBytes`, which is `pub(crate)`. Narrowing the trait is the
// direction that keeps the packed 17-byte record private; widening the record
// to satisfy a `pub` the module never exports would leak an on-disk detail for
// nothing. The module itself is `pub(crate)` and none of these traits are
// re-exported, so this costs no reachability.
//
// Gated as a whole rather than per method: every method it has left is behind
// the feature, so without it the trait had no methods at all.
#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) trait TransparentHistExt: Send + Sync {
    /// Fetch all address history records for a given transparent address.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more valid records exist,
    /// - `Ok(None)` if no records exist (not an error),
    /// - `Err(...)` if any decoding or DB error occurs.
    fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> impl SendFut<Result<Option<Vec<AddrEventBytes>>, StoreError>>;

    /// Fetch all address history records for a given address and TxLocation.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more matching records are found at that index,
    /// - `Ok(None)` if no matching records exist (not an error),
    /// - `Err(...)` on decode or DB failure.
    fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> impl SendFut<Result<Option<Vec<AddrEventBytes>>, StoreError>>;

    /// Fetch all distinct `TxLocation` values for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more matching records are found,
    /// - `Ok(None)` if no matches found (not an error),
    /// - `Err(...)` on decode or DB failure.
    fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> impl SendFut<Result<Option<Vec<TxLocation>>, StoreError>>;

    /// Fetch all UTXOs (unspent mined outputs) for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Each entry is `(TxLocation, vout, value)`.
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more UTXOs are found,
    /// - `Ok(None)` if none found (not an error),
    /// - `Err(...)` on decode or DB failure.
    fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> impl SendFut<Result<Option<Vec<AddrUtxo>>, StoreError>>;

    /// Computes the transparent balance change for `addr_script` over the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Includes:
    /// - `+value` for mined outputs
    /// - `−value` for spent inputs
    ///
    /// Returns the signed net value as `i64`, or error on failure.
    fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> impl SendFut<Result<i64, StoreError>>;

    // TODO: Add addr_deltas_by_range method!
}

/// Spent-output indexing extension.
///
/// Answers which transaction spent a given outpoint. Built unconditionally from
/// schema v1.2 onward.
///
/// Its own trait, not part of [`TransparentHistExt`]. The two were one surface,
/// which meant a build with address history compiled out still had to advertise
/// an address-history capability in order to answer a spend lookup — a name that
/// described neither what was being asked nor what was built.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::SPENT_OUTPUT_INDEX`].
pub trait SpentOutputExt: Send + Sync {
    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> impl SendFut<Result<Option<TxLocation>, StoreError>>;

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> impl SendFut<Result<Vec<Option<TxLocation>>, StoreError>>;
}

/// UTXO-set accumulator extension.
///
/// Capability gating:
/// - Backends must only be routed for this surface if they advertise
///   [`Capability::TXOUT_SET_INDEX`].
pub trait TxOutSetExt: Send + Sync {
    /// Returns the finalised-state txout-set accumulator.
    ///
    /// This is the finalised database portion of `gettxoutsetinfo`. It only contains values that
    /// are maintained by the finalised state:
    /// - number of transactions with at least one currently unspent transparent output;
    /// - number of currently unspent transparent outputs.
    ///
    /// Full RPC assembly, including non-finalised state and RPC-only fields, belongs above the
    /// finalised database layer.
    fn get_tx_out_set_info_accumulator(
        &self,
    ) -> impl SendFut<Result<FinalisedTxOutSetInfoAccumulator, StoreError>>;
}
