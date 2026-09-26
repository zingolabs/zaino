//! Metadata record and DB trait surface
//!
//! This file defines the interface that the `FinalisedState` database implements. Which optional
//! indexes exist is a build-time choice: an optional index's extension trait is gated by a cargo
//! feature, so a build without the feature has no such trait to call.
//!
//! # What’s in this file
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
//! - **Extension traits**:
//!   - [`BlockCoreExt`], [`BlockTransparentExt`]
//!   - [`CompactBlockExt`]
//!   - [`IndexedBlockExt`]
//!   - [`SpentOutputExt`], [`TxOutSetExt`]
//!   - `TransparentHistExt`, behind `transparent_address_history_experimental`
//!
//! # Development: adding or changing features safely
//!
//! When adding a new feature/query that requires new persistent data:
//!
//! 1. Add a new extension trait (or extend an existing one) that expresses the required operations.
//! 2. Gate the trait by a cargo feature if the index is optional.
//! 3. Implement the extension trait for `DbV1`.
//! 4. Expose it through a `DbReader` method.
//!
//! Changing a persisted format changes the computed schema hash, and every existing database
//! rebuilds.

use crate::codec::{read_fixed_le, write_fixed_le, DbCodec, FixedEncodedLen};
use crate::error::StoreError;
use crate::stream::CompactBlockStream;
use crate::support::SendFut;
#[cfg(test)]
use crate::types::BlockHeaderData;
use crate::types::{
    db::metadata::FinalisedTxOutSetInfoAccumulator, BlockHash, Height, IndexedBlock, Outpoint,
    TransactionHash, TransparentCompactTx, TxLocation, TxOutCompact,
};
use zaino_status::StatusType;

#[cfg(feature = "transparent_address_history_experimental")]
use crate::types::AddrScript;

use corez::io::{self, Read, Write};
use zaino_proto::proto::utils::PoolTypeFilter;

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
/// - and mapping hashes to heights and vice versa.
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
}

/// Core write operations that *every* database schema version must support.
pub trait DbWrite: Send + Sync {
    /// Ingests blocks from `source`, writing every height from the current tip up to and including
    /// `height` in order.
    ///
    /// This is the bulk catch-up path. Implementations own the ingestion loop so they can choose an
    /// efficient strategy: the v1 backend defers expensive secondary-index maintenance (the
    /// txout-set accumulator) across the run and rebuilds it once at the tip. A no-op is valid
    /// when the tip already meets or exceeds `height`.
    fn write_blocks_to_height<S: zaino_chain_store::ChainStoreSource>(
        &self,
        height: Height,
        source: &S,
    ) -> impl SendFut<Result<(), StoreError>>;
}

/// Core runtime surface that binds the core read/write operations to lifecycle and status reporting.
pub trait DbCore: DbRead + DbWrite + Send + Sync {
    /// Returns the current runtime status (`Starting`, `Syncing`, `Ready`, …).
    fn status(&self) -> StatusType;

    /// Initiates a graceful shutdown of background tasks and closes database resources.
    fn shutdown(&self) -> impl SendFut<Result<(), StoreError>>;
}

// ***** Database Extension traits *****

/// Core block indexing extension.
///
/// This extension covers transaction indexing by [`TxLocation`].
pub trait BlockCoreExt: Send + Sync {
    /// Return block header data by height.
    #[cfg(test)]
    fn get_block_header(&self, height: Height)
        -> impl SendFut<Result<BlockHeaderData, StoreError>>;

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

/// CompactBlock materialization extension.
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
}

/// One unspent output found by an address-history range query: where the
/// transaction sits, which output of it, and its value.
///
/// A named alias rather than a bare tuple repeated at each signature — the
/// positions are not self-describing, and a `u16` beside a `u64` invites being
/// swapped.
#[cfg(all(test, feature = "transparent_address_history_experimental"))]
pub(crate) type AddrUtxo = (TxLocation, u16, u64);

/// Transparent address history indexing extension.
///
/// This extension provides address-scoped queries backed by persisted indices built from the
/// transparent transaction graph (outputs, spends, and derived address events).
///
/// Range semantics:
/// - Methods that accept `start_height` and `end_height` interpret the range as inclusive:
///   `[start_height, end_height]`
// Gated as a whole rather than per method: every method it has left is behind
// the feature, so without it the trait had no methods at all.
#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) trait TransparentHistExt: Send + Sync {
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
    #[cfg(test)]
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
    #[cfg(test)]
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
