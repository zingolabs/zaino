//! ZainoDB Finalised State (Schema V1)
//!
//! This module provides the **V1** implementation of Zaino’s LMDB-backed finalised-state database.
//! It stores a validated, append-only view of the best chain and exposes a set of capability traits
//! (read, write, metadata, block-range fetchers, compact-block generation, and transparent history).
//!
//! ## On-disk layout
//! The V1 on-disk layout is described by an ASCII schema file that is embedded into the binary at
//! compile time (`db_schema_v1_0.txt`). A fixed 32-byte BLAKE2b checksum of that schema description
//! is stored in / compared against the database metadata to detect accidental schema drift.
//!
//! ## Integrity model
//! Each stored record carries a keyed BLAKE2b checksum. Writes embed it, and callers that need to
//! detect on-disk corruption (metadata load and migrations) verify it explicitly. Serving-path
//! reads decode without re-verifying; detecting at-rest corruption below this layer is delegated to
//! the storage layer. Hash-keyed lookups resolve to a height via `resolve_hash_or_height`.
//!
//! ## Concurrency model
//! LMDB supports many concurrent readers and a single writer per environment. This implementation
//! uses `tokio::task::block_in_place` / `spawn_blocking` for LMDB operations to avoid blocking the
//! async runtime, and configures `max_readers` to support high read concurrency.

use crate::chain_index::types::Height;
use crate::{
    chain_index::{
        finalised_state::{
            capability::{
                BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbCore,
                DbMetadata, DbRead, DbVersion, DbWrite, IndexedBlockExt, MigrationStatus,
                TransparentHistExt,
            },
            entry::{StoredEntryFixed, StoredEntryVar},
        },
        types::{TransactionHash, GENESIS_HEIGHT},
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockHeaderData, CommitmentTreeData, CompactBlockStream, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactSize, CompactTxData, FixedEncodedLen as _,
    IndexedBlock, NamedAtomicStatus, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx,
    SaplingTxList, StatusType, TransparentCompactTx, TransparentTxList, TxInCompact, TxLocation,
    TxOutCompact, TxidList, ZainoVersionedSerde as _,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::chain_index::finalised_state::entry::keyed_checksum;
#[cfg(feature = "transparent_address_history_experimental")]
use crate::{chain_index::types::AddrEventBytes, AddrHistRecord, AddrScript};

use zaino_proto::proto::{compact_formats::CompactBlock, utils::PoolTypeFilter};
use zebra_chain::parameters::NetworkKind;
use zebra_state::HashOrHeight;

use super::LmdbLifecycle;
use crate::chain_index::types::db::metadata::is_unspendable_tx_out;

use async_trait::async_trait;
use corez::io::{self, Read};
use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction as _, WriteFlags,
};
use std::collections::HashMap;
use std::{collections::HashSet, fs, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub(crate) mod read_core;
pub(crate) mod write_core;

pub(crate) mod block_core;
pub(crate) mod block_shielded;
pub(crate) mod block_transparent;

pub(crate) mod compact_block;
pub(crate) mod indexed_block;

pub(crate) mod transparent_address_history;

/// The single transparent created/spent delta of a block, shared by the DB-write
/// path, the txout-set accumulator, and the in-memory UTXO cache.
mod transparent_delta;

/// In-memory transparent UTXO cache that will feed the txout-set accumulator in
/// place of per-block DB reads. Skeleton; not yet wired in.
mod utxo_cache;

// ───────────────────────── Schema v1 constants ─────────────────────────

/// Full V1 schema text file.
///
/// This is the exact ASCII description of the V1 on-disk layout embedded into the binary at
/// compile-time. The path is relative to this source file.
///
/// 1. Bring the *exact* ASCII description of the on-disk layout into the binary at compile-time.
pub(crate) const DB_SCHEMA_V1_TEXT: &str = include_str!("db_schema_v1.txt");

/*
2. Compute the checksum once, outside the code:

       $ cd packages/zaino-state/src/chain_index/finalised_state/db
       $ b2sum -l 256 db_schema_v1.txt
       => [HASH]  db_schema_v1.txt

   Optional helper if you don’t have `b2sum`:

       $ python - <<'PY'
       > import hashlib, pathlib, binascii
       > data = pathlib.Path("db_schema_v1.txt").read_bytes()
       > print(hashlib.blake2b(data, digest_size=32).hexdigest())
       > PY

3. Turn those 64 hex digits into a Rust `[u8; 32]` literal:

       $ echo [HASH] | sed 's/../0x&, /g' | fold -s -w48

*/

/// *Current* database V1 schema hash, used for version validation.
///
/// This value is compared against the schema hash stored in the metadata record to detect schema
/// drift without a corresponding version bump.
pub(crate) const DB_SCHEMA_V1_HASH: [u8; 32] = [
    0x11, 0xb2, 0x6a, 0x12, 0x08, 0x67, 0xf0, 0x42, 0xf6, 0x31, 0x45, 0xea, 0x87, 0xe7, 0x23, 0x75,
    0x40, 0x3b, 0xf2, 0x14, 0xaa, 0x2b, 0x00, 0x12, 0xec, 0xa4, 0x4d, 0x00, 0xe9, 0x0b, 0x07, 0x9b,
];

/// *Current* database V1 version.
pub(crate) const DB_VERSION_V1: DbVersion = DbVersion {
    major: 1,
    minor: 2,
    patch: 0,
};

/// LMDB table name for the finalised txout-set accumulator.
pub(crate) const TX_OUT_SET_INFO_ACCUMULATOR_DATABASE_NAME: &str =
    "tx_out_set_info_accumulator_1_2_0";

/// Singleton key for the finalised txout-set accumulator table.
pub(crate) const TX_OUT_SET_INFO_ACCUMULATOR_KEY: &[u8] = b"tx_out_set_info_accumulator";

/// [`DbCore`] capability implementation for [`DbV1`].
///
/// This trait exposes lifecycle operations and a high-level status indicator.
#[async_trait]
impl DbCore for DbV1 {
    fn status(&self) -> StatusType {
        LmdbLifecycle::status(self)
    }

    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        LmdbLifecycle::shutdown(self).await
    }
}

impl LmdbLifecycle for DbV1 {
    fn env(&self) -> &Arc<Environment> {
        &self.env
    }

    fn db_handler_slot(&self) -> &std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> {
        &self.db_handler
    }

    fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    fn status_atomic(&self) -> &NamedAtomicStatus {
        &self.status
    }
}

/// Zaino’s Finalised State database V1.
///
/// This type owns an LMDB [`Environment`] and a fixed set of named databases representing the V1
/// schema. It implements the capability traits used by the rest of the chain indexer.
///
/// Data is stored per-height in “best chain” order; the write path appends only height-contiguous
/// blocks, and each record carries a keyed checksum for on-demand integrity verification.
#[derive(Debug)]
pub(crate) struct DbV1 {
    /// Shared LMDB environment.
    env: Arc<Environment>,

    /// Block headers: `Height` -> `StoredEntryVar<BlockHeaderData>`
    ///
    /// Stored per-block, in order.
    headers: Database,

    /// Txids: `Height` -> `StoredEntryVar<TxidList>`
    ///
    /// Stored per-block, in order.
    txids: Database,

    /// Transparent: `Height` -> `StoredEntryVar<Vec<TransparentTxList>>`
    ///
    /// Stored per-block, in order.
    transparent: Database,

    /// Sapling: `Height` -> `StoredEntryVar<Vec<TxData>>`
    ///
    /// Stored per-block, in order.
    sapling: Database,

    /// Orchard: `Height` -> `StoredEntryVar<Vec<TxData>>`
    ///
    /// Stored per-block, in order.
    orchard: Database,

    /// Block commitment tree data: `Height` -> `StoredEntryFixed<Vec<CommitmentTreeData>>`
    ///
    /// Stored per-block, in order.
    commitment_tree_data: Database,

    /// Heights: `Hash` -> `StoredEntryFixed<Height>`
    ///
    /// Used for hash based fetch of the best chain (and random access).
    heights: Database,

    /// Spent outpoints: `Outpoint` -> `StoredEntryFixed<Vec<TxLocation>>`
    ///
    /// Used to check spent status of given outpoints, retuning spending tx.
    spent: Database,

    /// Reverse txid index: `TransactionHash` -> `StoredEntryFixed<TxLocation>`
    ///
    /// Maps a transaction id to its on-chain `TxLocation`, giving O(log n) previous-output
    /// resolution instead of a full scan of the height-keyed `txids` table.
    txid_location: Database,

    /// Finalised txout-set accumulator:
    /// `"tx_out_set_info_accumulator"` -> `StoredEntryFixed<FinalisedTxOutSetInfoAccumulator>`.
    ///
    /// Stores the finalised-state portion of `gettxoutsetinfo` that can be maintained cheaply
    /// without adding per-UTXO storage.
    tx_out_set_info_accumulator: Database,

    /// Transparent address history: `AddrScript` -> duplicate values of `StoredEntryFixed<AddrEventBytes>`.
    ///
    /// Stored as an LMDB `DUP_SORT | DUP_FIXED` database keyed by address script bytes. Each duplicate
    /// value is a fixed-size entry encoding one address event (mined output or spending input),
    /// including flags and checksum.
    ///
    /// Used to search all transparent address indexes (txids, utxos, balances, deltas)
    #[cfg(feature = "transparent_address_history_experimental")]
    address_history: Database,

    /// Metadata: singleton entry "metadata" -> `StoredEntryFixed<DbMetadata>`
    metadata: Database,

    /// In-memory live transparent UTXO set, maintained forward as blocks are
    /// ingested so the txout-set accumulator can resolve spent-output values and
    /// per-tx unspent counts without faulting back to the `transparent` /
    /// `txid_location` / `spent` tables. Reconstructed from committed state on
    /// open; see [`utxo_cache`].
    transparent_utxo_cache: utxo_cache::TransparentUtxoCache,

    /// Background validator / maintenance task handle.
    ///
    /// Wrapped in a `Mutex` so `shutdown(&self)` can `.take()` the handle on
    /// the trait's `&self` signature. The lock is only held to swap the
    /// `Option`; no `.await` happens while it's held.
    db_handler: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Cancels the background task so it observes shutdown without waiting for
    /// the next idle-sleep or maintenance-tick boundary. Cloning the token
    /// shares cancellation state with every clone, so all background tasks
    /// (current and future) wake on a single `cancel()` call.
    cancel_token: CancellationToken,

    /// ZainoDB status.
    status: NamedAtomicStatus,

    /// BlockCache config data.
    config: BlockCacheConfig,
}

/// Inherent implementation for [`DbV1`].
///
/// This block contains:
/// - environment / database setup (`spawn`, `open_or_create_db`, schema checks),
/// - background validation task management,
/// - write/delete operations for finalised blocks,
/// - validated read fetchers used by the capability trait implementations, and
/// - internal validation / indexing helpers.
impl DbV1 {
    /// Spawns a new [`DbV1`] and opens (or creates) the LMDB environment for the configured network.
    ///
    /// This method:
    /// - chooses a versioned path suffix (`.../<network>/v1`),
    /// - configures LMDB map size and reader slots,
    /// - opens or creates all V1 named databases,
    /// - validates or initializes the `"metadata"` record (schema hash + version), and
    /// - spawns the background validator / maintenance task.
    pub(crate) async fn spawn(config: &BlockCacheConfig) -> Result<Self, FinalisedStateError> {
        info!("Launching ZainoDB");

        // Prepare database details and path.
        let db_size_bytes = config.storage.database.size.to_byte_count();
        let db_path_dir = match config.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "mainnet",
            NetworkKind::Testnet => "testnet",
            NetworkKind::Regtest => "regtest",
        };
        let db_path = config.storage.database.path.join(db_path_dir).join("v1");
        if !db_path.exists() {
            fs::create_dir_all(&db_path)?;
        }

        // Check system rescources to set max db reeaders, clamped between 512 and 4096.
        let cpu_cnt = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Sets LMDB max_readers based on CPU count (cpu * 32), clamped between 512 and 4096.
        // Allows high async read concurrency while keeping memory use low (~192B per slot).
        // The 512 min ensures reasonable capacity even on low-core systems.
        let max_readers = u32::try_from((cpu_cnt * 32).clamp(512, 4096))
            .expect("max_readers was clamped to fit in u32");

        // Open LMDB environment and set environmental details.
        let env = Environment::new()
            .set_max_dbs(15)
            .set_map_size(db_size_bytes)
            .set_max_readers(max_readers)
            // NO_META_SYNC: fsync the data pages on each commit but defer the meta-page
            // fsync, so a commit costs one fsync instead of two. The data fsync still
            // orders data-before-meta, so a crash stays consistent and loses at most the
            // last commit — it never corrupts (unlike bare NO_SYNC, which has no such
            // ordering barrier). With batched commits this is one fsync per batch.
            .set_flags(
                EnvironmentFlags::NO_TLS
                    | EnvironmentFlags::NO_READAHEAD
                    | EnvironmentFlags::NO_META_SYNC,
            )
            .open(&db_path)?;

        // Open individual LMDB DBs.
        let headers =
            super::open_or_create_db(&env, "headers_1_0_0", DatabaseFlags::empty()).await?;
        let txids = super::open_or_create_db(&env, "txids_1_0_0", DatabaseFlags::empty()).await?;
        let transparent =
            super::open_or_create_db(&env, "transparent_1_0_0", DatabaseFlags::empty()).await?;
        let sapling =
            super::open_or_create_db(&env, "sapling_1_0_0", DatabaseFlags::empty()).await?;
        let orchard =
            super::open_or_create_db(&env, "orchard_1_0_0", DatabaseFlags::empty()).await?;
        let commitment_tree_data =
            super::open_or_create_db(&env, "commitment_tree_data_1_0_0", DatabaseFlags::empty())
                .await?;
        let hashes = super::open_or_create_db(&env, "hashes_1_0_0", DatabaseFlags::empty()).await?;

        let spent = super::open_or_create_db(&env, "spent_1_0_0", DatabaseFlags::empty()).await?;

        let txid_location =
            super::open_or_create_db(&env, "txid_location_1_0_0", DatabaseFlags::empty()).await?;

        let tx_out_set_info_accumulator = super::open_or_create_db(
            &env,
            TX_OUT_SET_INFO_ACCUMULATOR_DATABASE_NAME,
            DatabaseFlags::empty(),
        )
        .await?;

        let metadata = super::open_or_create_db(&env, "metadata", DatabaseFlags::empty()).await?;

        #[cfg(feature = "transparent_address_history_experimental")]
        let address_history = super::open_or_create_db(
            &env,
            "address_history_1_0_0",
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .await?;

        let zaino_db = Self {
            env: Arc::new(env),
            headers,
            txids,
            transparent,
            sapling,
            orchard,
            commitment_tree_data,
            heights: hashes,
            spent,
            txid_location,
            tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history,
            metadata,
            transparent_utxo_cache: utxo_cache::TransparentUtxoCache::new(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("ZainoDB", StatusType::Spawning),
            config: config.clone(),
        };

        // Validate (or initialise) the metadata entry before we touch any tables.
        zaino_db.check_schema_version().await?;

        // Temporary 0.4.0-alpha.1 compatibility: heal a cache whose alpha migration left the
        // `txid_location` index unbuilt. Runs before the background validator starts so it operates
        // on a quiescent database.
        zaino_db.reconcile_alpha_txid_location_index().await?;

        // Reconstruct the in-memory transparent UTXO cache from committed state before
        // serving. A no-op on a fresh database; a one-time scan on resume.
        zaino_db.seed_transparent_utxo_cache()?;

        // Background validation has been removed; mark the database ready to serve.
        zaino_db.status.store(StatusType::Ready);

        Ok(zaino_db)
    }

    // *** Internal Control Methods ***

    /// Provides access to the metadata DB table, enabling the migration manager
    /// to use this DB table to store temporary migration metadata.
    pub(crate) fn metadata_db(&self) -> Database {
        self.metadata
    }

    /// Provudes access to the spent DB table, required for Migration1_1_0To1_2_0.
    pub(crate) fn spent_db(&self) -> Database {
        self.spent
    }

    /// Provides access to the reverse txid-index DB table, required for Migration1_1_0To1_2_0
    /// to backfill `txid_location` before resolving previous outputs.
    pub(crate) fn txid_location_db(&self) -> Database {
        self.txid_location
    }

    /// Provides access to the txids DB table, required for Migration1_1_0To1_2_0 to build the
    /// reverse txid index directly from stored block data.
    pub(crate) fn txids_db(&self) -> Database {
        self.txids
    }

    /// **Temporary 0.4.0-alpha.1 cache compatibility.**
    ///
    /// The 0.4.0-alpha.1 build shipped a v1.1.0 → v1.2.0 migration (and write path) that did not
    /// populate the new `txid_location` reverse index. A cache that *completed* that migration is
    /// recorded at version 1.2.0 with an empty `txid_location` table, and the migration manager
    /// would not re-select any step for it — so the corrected code would fail on its first new
    /// block write. When a non-empty database is recorded at `>= 1.2.0` but its `txid_location`
    /// index is empty, we roll the recorded version back to 1.1.0 (status `Empty`) so the corrected
    /// v1.1.0 → v1.2.0 migration rebuilds the index in place rather than forcing a full rebuild.
    ///
    /// TODO: Remove this shim once 0.4.0 is released; from then on no cache can reach this state.
    async fn reconcile_alpha_txid_location_index(&self) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            // A fresh database (no metadata yet) needs no reconciliation.
            let raw = match txn.get(self.metadata, b"metadata") {
                Ok(raw) => raw,
                Err(lmdb::Error::NotFound) => return Ok(()),
                Err(error) => return Err(FinalisedStateError::LmdbError(error)),
            };
            let stored = StoredEntryFixed::<DbMetadata>::from_bytes(raw).map_err(|error| {
                FinalisedStateError::Custom(format!("corrupt metadata: {error}"))
            })?;
            if !stored.verify(b"metadata") {
                return Err(FinalisedStateError::Custom(
                    "metadata checksum mismatch".to_string(),
                ));
            }
            let mut metadata = stored.item;

            // Only caches recorded at >= 1.2.0 can be in the broken alpha state.
            if metadata.version
                < (DbVersion {
                    major: 1,
                    minor: 2,
                    patch: 0,
                })
            {
                return Ok(());
            }

            // A genuinely fresh database (no blocks) needs no reconciliation; the write path builds
            // `txid_location` as it syncs. Under the corrected code a non-empty database always has
            // a non-empty index, so an empty index on a non-empty database means an alpha cache.
            let has_blocks = {
                let mut cursor = txn.open_ro_cursor(self.headers)?;
                cursor.iter().next().is_some()
            };
            let index_empty = {
                let mut cursor = txn.open_ro_cursor(self.txid_location)?;
                cursor.iter().next().is_none()
            };
            if !has_blocks || !index_empty {
                return Ok(());
            }

            warn!(
                "detected a 0.4.0-alpha.1 cache recorded at v{} with an unbuilt txid_location \
                 index; rolling the recorded version back to 1.1.0 so the corrected migration \
                 rebuilds it in place",
                metadata.version
            );

            // Clear the `spent` index the alpha migration built: the corrected Stage B rebuilds it
            // from genesis, and its accumulator forward-check rejects re-adding already-present
            // spends, so it must start from an empty table. Drop any stale per-stage progress keys
            // so both stages restart at genesis. (`txid_location` is already empty — that is the
            // condition that brought us here.)
            txn.clear_db(self.spent)?;
            for key in [
                b"_migration_txid_location_progress_1_2_0_next_height".as_slice(),
                b"_migration_spent_progress_1_2_0_next_height".as_slice(),
            ] {
                match txn.del(self.metadata, &key, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                }
            }

            metadata.version = DbVersion {
                major: 1,
                minor: 1,
                patch: 0,
            };
            metadata.migration_status = MigrationStatus::Empty;

            let entry = StoredEntryFixed::new(b"metadata", metadata);
            txn.put(
                self.metadata,
                b"metadata",
                &entry.to_bytes()?,
                WriteFlags::empty(),
            )?;
            txn.commit()?;

            Ok(())
        })
    }

    /// Reconstructs the in-memory transparent UTXO cache from committed state with two
    /// sequential table scans — never per-output random lookups, which would re-create
    /// the very read cliff this cache exists to remove:
    ///
    /// 1. scan `transparent` (paired with the parallel `txids` scan that supplies each
    ///    tx's id) and add every *spendable* created output, then
    /// 2. scan `spent` and remove every output that has since been spent.
    ///
    /// The set difference (created − spent) is the live unspent set. A no-op for a
    /// from-genesis sync (both tables empty); a one-time pair of sequential passes on
    /// resume, before the first new block. Reconstruct-on-open keeps the cache
    /// schema-free — nothing extra is persisted (see [`utxo_cache`]).
    fn seed_transparent_utxo_cache(&self) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;

            // Pass 1: add every spendable created output. `transparent` and `txids` are
            // both keyed by height and written together, so a lockstep cursor walk pairs
            // each transparent tx with its id without any point lookups.
            {
                let mut transparent_cursor = ro.open_ro_cursor(self.transparent)?;
                let mut txids_cursor = ro.open_ro_cursor(self.txids)?;
                let mut transparent_iter = transparent_cursor.iter();
                let mut txids_iter = txids_cursor.iter();

                loop {
                    match (transparent_iter.next(), txids_iter.next()) {
                        (
                            Some((transparent_key, transparent_raw)),
                            Some((txids_key, txids_raw)),
                        ) => {
                            if transparent_key != txids_key {
                                return Err(FinalisedStateError::Custom(
                                    "transparent and txids tables diverge while seeding the UTXO \
                                     cache"
                                        .into(),
                                ));
                            }

                            let transparent_list =
                                StoredEntryVar::<TransparentTxList>::from_bytes(transparent_raw)
                                    .map_err(|e| {
                                        FinalisedStateError::Custom(format!(
                                            "transparent decode error: {e}"
                                        ))
                                    })?
                                    .inner()
                                    .clone();
                            let txid_list = StoredEntryVar::<TxidList>::from_bytes(txids_raw)
                                .map_err(|e| {
                                    FinalisedStateError::Custom(format!("txids decode error: {e}"))
                                })?
                                .inner()
                                .clone();
                            let txids = txid_list.txids();

                            for (tx_index, tx_opt) in transparent_list.tx().iter().enumerate() {
                                let Some(tx) = tx_opt else { continue };
                                let Some(txid) = txids.get(tx_index) else {
                                    return Err(FinalisedStateError::Custom(
                                        "txids shorter than transparent tx list while seeding the \
                                         UTXO cache"
                                            .into(),
                                    ));
                                };

                                for (vout, output) in tx.outputs().iter().enumerate() {
                                    // Unspendable outputs were never in the UTXO set; the
                                    // accumulator excludes them, so the cache must too.
                                    if is_unspendable_tx_out(output) {
                                        continue;
                                    }
                                    let vout = u32::try_from(vout).map_err(|_| {
                                        FinalisedStateError::Custom(
                                            "output index does not fit u32".into(),
                                        )
                                    })?;
                                    self.transparent_utxo_cache
                                        .record_created(Outpoint::new(txid.0, vout), *output);
                                }
                            }
                        }
                        (None, None) => break,
                        _ => {
                            return Err(FinalisedStateError::Custom(
                                "transparent and txids tables have different lengths while seeding \
                                 the UTXO cache"
                                    .into(),
                            ));
                        }
                    }
                }
            }

            // Pass 2: remove every spent output, leaving the live unspent set.
            {
                let mut spent_cursor = ro.open_ro_cursor(self.spent)?;
                for (outpoint_key, _spender) in spent_cursor.iter() {
                    let outpoint = Outpoint::deserialize(outpoint_key).map_err(|e| {
                        FinalisedStateError::Custom(format!("spent outpoint decode error: {e}"))
                    })?;
                    self.transparent_utxo_cache.record_spent(&outpoint);
                }
            }

            Ok(())
        })
    }

    /// Reconstructs the cache from committed state, discarding any in-memory build-time
    /// updates a failed or aborted write left behind. The delete path and the
    /// write-failure paths call this to bring the cache back in line with the durable DB.
    fn reseed_transparent_utxo_cache(&self) -> Result<(), FinalisedStateError> {
        self.transparent_utxo_cache.clear();
        self.seed_transparent_utxo_cache()
    }

    /// Provides access to the finalised txout-set accumulator DB table.
    pub(crate) fn tx_out_set_info_accumulator_db(&self) -> Database {
        self.tx_out_set_info_accumulator
    }

    /// Test-only snapshot of the reconstructed transparent UTXO cache.
    #[cfg(test)]
    pub(crate) fn transparent_utxo_cache_snapshot(
        &self,
    ) -> std::collections::HashMap<Outpoint, TxOutCompact> {
        self.transparent_utxo_cache.snapshot()
    }

    /// Resolve a `HashOrHeight` to the block height stored on disk.
    ///
    /// * Height -> returned unchanged (zero cost).
    /// * Hash   -> lookup in the `heights` db.
    pub(super) async fn resolve_hash_or_height(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<Height, FinalisedStateError> {
        match hash_or_height {
            // Height path: confirm the height is in the stored best chain.
            //
            // A height "exists" iff its header is stored. The previous validating
            // resolver did this header read implicitly; the pure resolver must keep it
            // so an absent height (DataUnavailable here) stays distinguishable from a
            // block that exists but is internally incomplete (IncompleteBlock, raised by
            // the dependent-table reads downstream). This is one hot-edge header read,
            // not the per-input faulting reads the background validator used to perform.
            HashOrHeight::Height(z_height) => {
                let height = Height::try_from(z_height.0).map_err(|_| {
                    FinalisedStateError::DataUnavailable("height out of range".into())
                })?;
                let hkey = height.to_bytes()?;

                tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    match ro.get(self.headers, &hkey) {
                        Ok(_) => Ok::<(), FinalisedStateError>(()),
                        Err(lmdb::Error::NotFound) => Err(FinalisedStateError::DataUnavailable(
                            "height not found in best chain".into(),
                        )),
                        Err(e) => Err(FinalisedStateError::LmdbError(e)),
                    }
                })?;

                Ok(height)
            }

            // Hash lookup path.
            HashOrHeight::Hash(z_hash) => {
                let hash = BlockHash::from(z_hash.0);
                let hkey = hash.to_bytes()?;

                let height: Height = tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    let bytes = ro.get(self.heights, &hkey).map_err(|e| {
                        if e == lmdb::Error::NotFound {
                            FinalisedStateError::DataUnavailable(
                                "height not found in best chain".into(),
                            )
                        } else {
                            FinalisedStateError::LmdbError(e)
                        }
                    })?;

                    let entry = *StoredEntryFixed::<Height>::deserialize(bytes)?.inner();
                    Ok::<Height, FinalisedStateError>(entry)
                })?;

                Ok(height)
            }
        }
    }

    /// Ensure the `metadata` table contains **exactly** our `DB_SCHEMA_V1`.
    ///
    /// * Brand-new DB → insert the entry.
    /// * Existing DB  → verify checksum, version, and schema hash.
    async fn check_schema_version(&self) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            match txn.get(self.metadata, b"metadata") {
                // ***** Existing DB *****
                Ok(raw_bytes) => {
                    let stored: StoredEntryFixed<DbMetadata> =
                        StoredEntryFixed::from_bytes(raw_bytes).map_err(|e| {
                            FinalisedStateError::Custom(format!("corrupt metadata CBOR: {e}"))
                        })?;
                    if !stored.verify(b"metadata") {
                        return Err(FinalisedStateError::Custom(
                            "metadata checksum mismatch – DB corruption suspected".into(),
                        ));
                    }

                    let meta = stored.item;

                    // Error if major version differs
                    if meta.version.major != DB_VERSION_V1.major {
                        return Err(FinalisedStateError::Custom(format!(
                            "unsupported schema major version {} (expected {})",
                            meta.version.major, DB_VERSION_V1.major
                        )));
                    }

                    // Warn if schema hash mismatches
                    // NOTE: There could be a schema mismatch at launch during minor migrations,
                    //       so we do not return an error here. Maybe we can improve this?
                    if meta.schema_hash != DB_SCHEMA_V1_HASH {
                        warn!(
                            "schema hash mismatch: db_schema_v1.txt has likely changed \
                         without bumping version; expected 0x{:02x?}, found 0x{:02x?}",
                            &DB_SCHEMA_V1_HASH[..4],
                            &meta.schema_hash[..4],
                        );
                    }
                }

                // ***** Fresh DB (key not found) *****
                Err(lmdb::Error::NotFound) => {
                    let entry = StoredEntryFixed::new(
                        b"metadata",
                        DbMetadata {
                            version: DB_VERSION_V1,
                            schema_hash: DB_SCHEMA_V1_HASH,
                            // Fresh database, no migration required.
                            migration_status: MigrationStatus::Empty,
                        },
                    );
                    txn.put(
                        self.metadata,
                        b"metadata",
                        &entry.to_bytes()?,
                        WriteFlags::NO_OVERWRITE,
                    )?;
                }

                // ***** Any other LMDB error *****
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }

            txn.commit()?;
            Ok(())
        })
    }
}

impl Drop for DbV1 {
    fn drop(&mut self) {
        if let Some(handle) = self
            .db_handler
            .get_mut()
            .expect("db_handler mutex poisoned")
            .take()
        {
            handle.abort();
        }
    }
}

#[cfg(test)]
impl DbV1 {
    /// Spawns a test-only [`DbV1`] using the v1.0.0 database metadata.
    ///
    /// This method is intended for migration tests that need to create an old v1.0.0 database
    /// before opening it through the current startup / migration path.
    ///
    /// This method:
    /// - chooses the normal V1 path suffix (`.../<network>/v1`),
    /// - configures LMDB map size and reader slots,
    /// - opens or creates the v1.0.0 named databases,
    /// - writes a `"metadata"` record with database version `1.0.0`, and
    /// - spawns the background validator / maintenance task.
    ///
    /// Unlike [`DbV1::spawn`], this method intentionally does **not** call
    /// [`DbV1::check_schema_version`], because that would initialize fresh metadata using the
    /// current [`DB_VERSION_V1`] value instead of the historical v1.0.0 value required by the tests.
    pub(crate) async fn spawn_v1_0_0(
        config: &BlockCacheConfig,
    ) -> Result<Self, FinalisedStateError> {
        info!("Launching ZainoDB");

        // Prepare database details and path.
        let db_size_bytes = config.storage.database.size.to_byte_count();
        let db_path_dir = match config.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "mainnet",
            NetworkKind::Testnet => "testnet",
            NetworkKind::Regtest => "regtest",
        };
        let db_path = config.storage.database.path.join(db_path_dir).join("v1");
        if !db_path.exists() {
            fs::create_dir_all(&db_path)?;
        }

        // Check system rescources to set max db reeaders, clamped between 512 and 4096.
        let cpu_cnt = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        // Sets LMDB max_readers based on CPU count (cpu * 32), clamped between 512 and 4096.
        // Allows high async read concurrency while keeping memory use low (~192B per slot).
        // The 512 min ensures reasonable capacity even on low-core systems.
        let max_readers = u32::try_from((cpu_cnt * 32).clamp(512, 4096))
            .expect("max_readers was clamped to fit in u32");

        // Open LMDB environment and set environmental details.
        let env = Environment::new()
            .set_max_dbs(15)
            .set_map_size(db_size_bytes)
            .set_max_readers(max_readers)
            // NO_META_SYNC: fsync the data pages on each commit but defer the meta-page
            // fsync, so a commit costs one fsync instead of two. The data fsync still
            // orders data-before-meta, so a crash stays consistent and loses at most the
            // last commit — it never corrupts (unlike bare NO_SYNC, which has no such
            // ordering barrier). With batched commits this is one fsync per batch.
            .set_flags(
                EnvironmentFlags::NO_TLS
                    | EnvironmentFlags::NO_READAHEAD
                    | EnvironmentFlags::NO_META_SYNC,
            )
            .open(&db_path)?;

        // Open individual LMDB DBs.
        let headers =
            super::open_or_create_db(&env, "headers_1_0_0", DatabaseFlags::empty()).await?;
        let txids = super::open_or_create_db(&env, "txids_1_0_0", DatabaseFlags::empty()).await?;
        let transparent =
            super::open_or_create_db(&env, "transparent_1_0_0", DatabaseFlags::empty()).await?;
        let sapling =
            super::open_or_create_db(&env, "sapling_1_0_0", DatabaseFlags::empty()).await?;
        let orchard =
            super::open_or_create_db(&env, "orchard_1_0_0", DatabaseFlags::empty()).await?;
        let commitment_tree_data =
            super::open_or_create_db(&env, "commitment_tree_data_1_0_0", DatabaseFlags::empty())
                .await?;
        let hashes = super::open_or_create_db(&env, "hashes_1_0_0", DatabaseFlags::empty()).await?;

        let spent = super::open_or_create_db(&env, "spent_1_0_0", DatabaseFlags::empty()).await?;

        let txid_location =
            super::open_or_create_db(&env, "txid_location_1_0_0", DatabaseFlags::empty()).await?;

        let tx_out_set_info_accumulator = super::open_or_create_db(
            &env,
            TX_OUT_SET_INFO_ACCUMULATOR_DATABASE_NAME,
            DatabaseFlags::empty(),
        )
        .await?;

        let metadata = super::open_or_create_db(&env, "metadata", DatabaseFlags::empty()).await?;

        #[cfg(feature = "transparent_address_history_experimental")]
        let address_history = super::open_or_create_db(
            &env,
            "address_history_1_0_0",
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .await?;

        let zaino_db = Self {
            env: Arc::new(env),
            headers,
            txids,
            transparent,
            sapling,
            orchard,
            commitment_tree_data,
            heights: hashes,
            spent,
            txid_location,
            tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history,
            metadata,
            transparent_utxo_cache: utxo_cache::TransparentUtxoCache::new(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("ZainoDB", StatusType::Spawning),
            config: config.clone(),
        };

        // Initialise the metadata entry before we touch any tables.
        tokio::task::block_in_place(|| {
            let mut txn = zaino_db.env.begin_rw_txn()?;

            let entry = StoredEntryFixed::new(
                b"metadata",
                DbMetadata {
                    version: DbVersion {
                        major: 1,
                        minor: 0,
                        patch: 0,
                    },
                    schema_hash: [0u8; 32],
                    migration_status: MigrationStatus::Empty,
                },
            );
            txn.put(
                zaino_db.metadata,
                b"metadata",
                &entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.commit()?;

            Ok::<(), FinalisedStateError>(())
        })?;

        // Background validation has been removed; mark the database ready to serve.
        zaino_db.status.store(StatusType::Ready);

        Ok(zaino_db)
    }
}
