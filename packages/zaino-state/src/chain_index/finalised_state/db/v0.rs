//! ZainoDB V0 Implementation
//!
//! WARNING: This is a legacy development database and should not be used in production environments.
//!
//! This module implements the original “v0” finalised-state database backend. It exists primarily
//! for backward compatibility and for development/testing scenarios where the historical v0
//! on-disk layout must be opened.
//!
//! ## Important constraints
//!
//! - **Not schema-versioned in the modern sense:** this database version predates Zaino’s
//!   `ZainoVersionedSerde` wire format, therefore it does not store version-tagged records and does
//!   not participate in fine-grained schema evolution.
//! - **Legacy encoding strategy:**
//!   - keys and values are stored as JSON via `serde_json` for most types,
//!   - `CompactBlock` values are encoded as raw Prost bytes via a custom `Serialize`/`Deserialize`
//!     wrapper (`DbCompactBlock`) so they can still flow through `serde_json`.
//! - **Limited feature surface:** v0 only supports the core height/hash mapping and compact block
//!   retrieval. It does not provide the richer indices introduced in v1 (header data, transaction
//!   locations, transparent history indexing, etc.).
//!
//! ## On-disk layout
//!
//! The v0 database uses the legacy network directory names:
//! - mainnet: `live/`
//! - testnet: `test/`
//! - regtest: `local/`
//!
//! Each network directory contains an LMDB environment with (at minimum) these tables:
//! - `heights_to_hashes`: `<block_height_be, block_hash_json>`
//! - `hashes_to_blocks`: `<block_hash_json, compact_block_json>` (where the compact block is stored
//!   as raw Prost bytes wrapped by JSON)
//!
//! ## Runtime model
//!
//! `DbV0` spawns a lightweight background maintenance task that:
//! - publishes `StatusType::Ready` once spawned,
//! - periodically calls `clean_trailing()` to reclaim stale LMDB reader slots.
//!
//! This backend uses `tokio::task::block_in_place` / `tokio::task::spawn_blocking` around LMDB
//! operations to avoid blocking the async runtime.

use crate::{
    chain_index::{
        finalised_state::capability::{
            CompactBlockExt, DbCore, DbMetadata, DbRead, DbVersion, DbWrite,
        },
        types::GENESIS_HEIGHT,
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    status::{NamedAtomicStatus, StatusType},
    CompactBlockStream, Height, IndexedBlock,
};

use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::PoolType,
    utils::{compact_block_with_pool_types, PoolTypeFilter},
};

use zebra_chain::{
    block::{Hash as ZebraHash, Height as ZebraHeight},
    parameters::NetworkKind,
};

use async_trait::async_trait;
use lmdb::{Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{fs, sync::Arc, time::Duration};
use tokio::{
    sync::Notify,
    time::{interval, MissedTickBehavior},
};
use tracing::{info, warn};

// ───────────────────────── ZainoDb v0 Capabilities ─────────────────────────

/// `DbRead` implementation for the legacy v0 backend.
///
/// Note: v0 exposes only a minimal read surface. Missing data is mapped to `Ok(None)` where the
/// core trait expects optional results.
#[async_trait]
impl DbRead for DbV0 {
    /// Returns the database tip height (`None` if empty).
    async fn db_height(&self) -> Result<Option<crate::Height>, FinalisedStateError> {
        self.tip_height().await
    }

    /// Returns the block height for a given block hash, if known.
    ///
    /// For v0, absence is represented as either `DataUnavailable` or `FeatureUnavailable` from the
    /// legacy helper; both are mapped to `Ok(None)` here.
    async fn get_block_height(
        &self,
        hash: crate::BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        match self.get_block_height_by_hash(hash).await {
            Ok(height) => Ok(Some(height)),
            Err(
                FinalisedStateError::DataUnavailable(_)
                | FinalisedStateError::FeatureUnavailable(_),
            ) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// Returns the block hash for a given block height, if known.
    ///
    /// For v0, absence is represented as either `DataUnavailable` or `FeatureUnavailable` from the
    /// legacy helper; both are mapped to `Ok(None)` here.
    async fn get_block_hash(
        &self,
        height: crate::Height,
    ) -> Result<Option<crate::BlockHash>, FinalisedStateError> {
        match self.get_block_hash_by_height(height).await {
            Ok(hash) => Ok(Some(hash)),
            Err(
                FinalisedStateError::DataUnavailable(_)
                | FinalisedStateError::FeatureUnavailable(_),
            ) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// Returns synthetic metadata for v0.
    ///
    /// v0 does not persist `DbMetadata` on disk; this returns a constructed value describing
    /// version `0.0.0` and a default schema hash.
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.get_metadata().await
    }
}

/// `DbWrite` implementation for the legacy v0 backend.
///
/// v0 supports append-only writes and pop-only deletes at the tip, enforced by explicit checks in
/// the legacy methods.
#[async_trait]
impl DbWrite for DbV0 {
    /// Writes a fully-validated finalised block, enforcing strict height monotonicity.
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.write_block(block).await
    }

    /// Deletes a block at the given height, enforcing that it is the current tip.
    async fn delete_block_at_height(
        &self,
        height: crate::Height,
    ) -> Result<(), FinalisedStateError> {
        self.delete_block_at_height(height).await
    }

    /// Deletes a block by explicit content.
    ///
    /// This is a fallback path used when tip-based deletion cannot safely determine the full set of
    /// keys to delete (for example, when corruption is suspected).
    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.delete_block(block).await
    }

    /// Updates the metadata singleton.
    ///
    /// NOTE: v0 does not persist metadata on disk; this is a no-op to satisfy the trait.
    async fn update_metadata(&self, _metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        Ok(())
    }
}

/// `DbCore` implementation for the legacy v0 backend.
///
/// The core lifecycle API is implemented in terms of a status flag and a lightweight background
/// maintenance task.
#[async_trait]
impl DbCore for DbV0 {
    /// Returns the current runtime status published by this backend.
    fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Requests shutdown of background tasks and syncs the LMDB environment before returning.
    ///
    /// Signals the background task via `shutdown_notify` so it wakes out of
    /// `zaino_db_handler_sleep` immediately, then awaits its join handle with
    /// a 5 s timeout (aborting only if the handle fails to exit in time).
    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Closing);
        // `notify_one` stores a permit if no waiter is currently registered,
        // so the task consumes the signal on its next `notified().await` even
        // if shutdown fires before the task has entered the select.
        // `notify_waiters` would be lost in that window (no stored permit).
        self.shutdown_notify.notify_one();

        let taken = self
            .db_handler
            .lock()
            .expect("db_handler mutex poisoned")
            .take();
        if let Some(mut handle) = taken {
            let timeout = tokio::time::sleep(Duration::from_secs(5));
            tokio::pin!(timeout);

            tokio::select! {
                res = &mut handle => {
                    match res {
                        Ok(_) => {}
                        Err(e) if e.is_cancelled() => {}
                        Err(e) => warn!("background task ended with error: {e:?}"),
                    }
                }
                _ = &mut timeout => {
                    warn!("background task didn’t exit in time – aborting");
                    handle.abort();
                }
            }
        }

        let _ = self.clean_trailing().await;
        if let Err(e) = self.env.sync(true) {
            warn!("LMDB fsync before close failed: {e}");
        }
        Ok(())
    }
}

/// [`CompactBlockExt`] capability implementation for [`DbV0`].
///
/// Exposes `zcash_client_backend`-compatible compact blocks derived from stored header +
/// transaction data.
#[async_trait]
impl CompactBlockExt for DbV0 {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        self.get_compact_block(height, pool_types).await
    }

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        self.get_compact_block_stream(start_height, end_height, pool_types)
            .await
    }
}

/// Finalised part of the chain, held in an LMDB database (legacy v0).
///
/// `DbV0` maintains two simple indices:
/// - height → hash
/// - hash → compact block
///
/// It does **not** implement the richer v1 indices (header data, tx location maps, address history,
/// commitment tree tables, etc.).
#[derive(Debug)]
pub struct DbV0 {
    /// LMDB database environment handle.
    ///
    /// The environment is shared between tasks using `Arc` and is configured for high read
    /// concurrency (`max_readers`) and reduced I/O overhead (`NO_READAHEAD`).
    env: Arc<Environment>,

    /// LMDB database containing `<block_height_be, block_hash_json>`.
    ///
    /// Heights are stored as 4-byte big-endian keys for correct lexicographic ordering.
    heights_to_hashes: Database,

    /// LMDB database containing `<block_hash_json, compact_block_json>`.
    ///
    /// The compact block is stored via the `DbCompactBlock` wrapper: raw Prost bytes embedded in a
    /// JSON payload.
    hashes_to_blocks: Database,

    /// Background maintenance task handle.
    ///
    /// Wrapped in a `Mutex` so `shutdown(&self)` can `.take()` the handle on
    /// the trait's `&self` signature. The lock is only held to swap the
    /// `Option`; no `.await` happens while it's held.
    db_handler: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Wakes the background task out of `zaino_db_handler_sleep` when shutdown
    /// is requested, so it observes `StatusType::Closing` without waiting for
    /// the next idle-sleep or maintenance-tick boundary.
    shutdown_notify: Arc<Notify>,

    /// Backend lifecycle status.
    status: NamedAtomicStatus,

    /// Configuration snapshot used for path/network selection and sizing parameters.
    config: BlockCacheConfig,
}

impl DbV0 {
    /// Spawns a new [`DbV0`] backend.
    ///
    /// This:
    /// - derives the v0 network directory name (`live` / `test` / `local`),
    /// - opens or creates the LMDB environment and required databases,
    /// - configures LMDB reader concurrency based on CPU count,
    /// - spawns a background maintenance task,
    /// - and returns the opened backend.
    ///
    /// # Errors
    /// Returns `FinalisedStateError` on any filesystem, LMDB, or task-spawn failure.
    pub(crate) async fn spawn(config: &BlockCacheConfig) -> Result<Self, FinalisedStateError> {
        info!("Launching ZainoDB");

        // Prepare database details and path.
        let db_size_bytes = config.storage.database.size.to_byte_count();
        let db_path_dir = match config.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "live",
            NetworkKind::Testnet => "test",
            NetworkKind::Regtest => "local",
        };
        let db_path = config.storage.database.path.join(db_path_dir);
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
            .set_max_dbs(12)
            .set_map_size(db_size_bytes)
            .set_max_readers(max_readers)
            .set_flags(EnvironmentFlags::NO_TLS | EnvironmentFlags::NO_READAHEAD)
            .open(&db_path)?;

        // Open individual LMDB DBs.
        let heights_to_hashes =
            Self::open_or_create_db(&env, "heights_to_hashes", DatabaseFlags::empty()).await?;
        let hashes_to_blocks =
            Self::open_or_create_db(&env, "hashes_to_blocks", DatabaseFlags::empty()).await?;

        // Create ZainoDB
        let mut zaino_db = Self {
            env: Arc::new(env),
            heights_to_hashes,
            hashes_to_blocks,
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::new(Notify::new()),
            status: NamedAtomicStatus::new("ZainoDB", StatusType::Spawning),
            config: config.clone(),
        };

        // Spawn handler task to perform background validation and trailing tx cleanup.
        zaino_db.spawn_handler().await?;

        Ok(zaino_db)
    }

    /// Returns the current backend status.
    pub(crate) fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Blocks until the backend reports `StatusType::Ready`.
    ///
    /// This is primarily used during startup sequencing so callers do not issue reads before the
    /// backend is ready to serve queries.
    pub(crate) async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if self.status.load() == StatusType::Ready {
                break;
            }
        }
    }

    // *** Internal Control Methods ***

    /// Spawns the background maintenance task.
    ///
    /// The v0 maintenance task is intentionally minimal:
    /// - publishes `StatusType::Ready` after spawning,
    /// - periodically calls `clean_trailing()` to purge stale LMDB reader slots,
    /// - exits when status transitions to `StatusType::Closing`.
    ///
    /// Note: historical comments refer to validation passes; the current implementation only
    /// performs maintenance and does not validate chain contents.
    async fn spawn_handler(&mut self) -> Result<(), FinalisedStateError> {
        // Clone everything the task needs so we can move it into the async block.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };

        let handle = tokio::spawn({
            let zaino_db = zaino_db;
            async move {
                zaino_db.status.store(StatusType::Ready);

                // *** steady-state loop ***
                let mut maintenance = interval(Duration::from_secs(60));

                loop {
                    // Check for closing status.
                    if zaino_db.status.load() == StatusType::Closing {
                        break;
                    }

                    zaino_db.zaino_db_handler_sleep(&mut maintenance).await;
                }
            }
        });

        *self.db_handler.lock().expect("db_handler mutex poisoned") = Some(handle);
        Ok(())
    }

    /// Helper method to wait for the next loop iteration or perform maintenance.
    ///
    /// This selects between:
    /// - a short sleep (steady-state pacing), and
    /// - the maintenance tick (currently reader-slot cleanup).
    async fn zaino_db_handler_sleep(&self, maintenance: &mut tokio::time::Interval) {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            _ = maintenance.tick() => {
                if let Err(e) = self.clean_trailing().await {
                    warn!("clean_trailing failed: {}", e);
                }
            }
            _ = self.shutdown_notify.notified() => {},
        }
    }

    /// Clears stale LMDB reader slots by opening and closing a read transaction.
    ///
    /// LMDB only reclaims reader slots when transactions are closed; this method is a cheap and safe
    /// way to encourage reclamation in long-running services.
    async fn clean_trailing(&self) -> Result<(), FinalisedStateError> {
        let txn = self.env.begin_ro_txn()?;
        drop(txn);
        Ok(())
    }

    /// Opens an LMDB database if present, otherwise creates it.
    ///
    /// v0 uses this helper for all tables to make environment creation idempotent across restarts.
    async fn open_or_create_db(
        env: &Environment,
        name: &str,
        flags: DatabaseFlags,
    ) -> Result<Database, FinalisedStateError> {
        match env.open_db(Some(name)) {
            Ok(db) => Ok(db),
            Err(lmdb::Error::NotFound) => env
                .create_db(Some(name), flags)
                .map_err(FinalisedStateError::LmdbError),
            Err(e) => Err(FinalisedStateError::LmdbError(e)),
        }
    }

    // *** DB write / delete methods ***
    // These should only ever be used in a single DB control task.

    /// Writes a given (finalised) [`IndexedBlock`] to the v0 database.
    ///
    /// This method enforces the v0 write invariant:
    /// - if the database is non-empty, the new block height must equal `current_tip + 1`,
    /// - if the database is empty, the first write must be genesis (`GENESIS_HEIGHT`).
    ///
    /// The following records are written atomically in a single LMDB write transaction:
    /// - `heights_to_hashes[height_be] = hash_json`
    /// - `hashes_to_blocks[hash_json] = compact_block_json`
    ///
    /// On failure, the method attempts to delete the partially-written block (best effort) and
    /// returns an `InvalidBlock` error that includes the height/hash context.
    pub(crate) async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Syncing);

        let compact_block: CompactBlock = block.to_compact_block();
        let zebra_height: ZebraHeight = block.context.height().into();
        let zebra_hash: ZebraHash = zebra_chain::block::Hash::from(*block.context.hash());

        let height_key = DbHeight(zebra_height).to_be_bytes();
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;
        let block_value = serde_json::to_vec(&DbCompactBlock(compact_block))?;

        // check this is the *next* block in the chain.
        let block_height = block.context.height().0;

        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.heights_to_hashes)?;

            // Position the cursor at the last header we currently have
            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                // Database already has blocks
                Ok((last_height_bytes, _last_hash_bytes)) => {
                    let block_height = block.context.height().0;

                    let last_height = DbHeight::from_be_bytes(
                        last_height_bytes.expect("Height is always some in the finalised state"),
                    )?
                    .0
                     .0;

                    // Height must be exactly +1 over the current tip
                    if block_height != last_height + 1 {
                        return Err(FinalisedStateError::Custom(format!(
                            "cannot write block at height {block_height:?}; \
                     current tip is {last_height:?}"
                        )));
                    }
                }
                // no block in db, this must be genesis block.
                Err(lmdb::Error::NotFound) => {
                    if block_height != GENESIS_HEIGHT.0 {
                        return Err(FinalisedStateError::Custom(format!(
                            "first block must be height 0, got {block_height:?}"
                        )));
                    }
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }
            Ok::<_, FinalisedStateError>(())
        })?;

        // if any database writes fail, or block validation fails, remove block from database and return err.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };
        let post_result = tokio::task::spawn_blocking(move || {
            // let post_result: Result<(), FinalisedStateError> = (async {
            // Write block to ZainoDB
            let mut txn = zaino_db.env.begin_rw_txn()?;

            txn.put(
                zaino_db.heights_to_hashes,
                &height_key,
                &hash_key,
                lmdb::WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.hashes_to_blocks,
                &hash_key,
                &block_value,
                lmdb::WriteFlags::NO_OVERWRITE,
            )?;

            txn.commit()?;

            Ok::<_, FinalisedStateError>(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("Tokio task error: {e}")))?;

        match post_result {
            Ok(_) => {
                tokio::task::block_in_place(|| self.env.sync(true))
                    .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
                self.status.store(StatusType::Ready);
                Ok(())
            }
            Err(e) => {
                let _ = self.delete_block(&block).await;
                tokio::task::block_in_place(|| self.env.sync(true))
                    .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
                self.status.store(StatusType::RecoverableError);
                Err(FinalisedStateError::InvalidBlock {
                    height: block_height,
                    hash: *block.context.hash(),
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Deletes the block at `height` from every v0 table.
    ///
    /// This method enforces the v0 delete invariant:
    /// - the requested height must equal the current database tip.
    ///
    /// The method determines the tip hash from `heights_to_hashes`, then deletes:
    /// - `heights_to_hashes[height_be]`
    /// - `hashes_to_blocks[hash_json]`
    pub(crate) async fn delete_block_at_height(
        &self,
        height: crate::Height,
    ) -> Result<(), FinalisedStateError> {
        let block_height = height.0;
        let height_key = DbHeight(zebra_chain::block::Height(block_height)).to_be_bytes();

        // check this is the *next* block in the chain and return the hash.
        let zebra_block_hash: zebra_chain::block::Hash = tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.heights_to_hashes)?;

            // Position the cursor at the last header we currently have
            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                // Database already has blocks
                Ok((last_height_bytes, last_hash_bytes)) => {
                    let last_height = DbHeight::from_be_bytes(
                        last_height_bytes.expect("Height is always some in the finalised state"),
                    )?
                    .0
                     .0;

                    // Check this is the block at the top of the database.
                    if block_height != last_height {
                        return Err(FinalisedStateError::Custom(format!(
                            "cannot delete block at height {block_height:?}; \
                     current tip is {last_height:?}"
                        )));
                    }

                    // Deserialize the hash
                    let db_hash: DbHash = serde_json::from_slice(last_hash_bytes)?;

                    Ok(db_hash.0)
                }
                // no block in db, this must be genesis block.
                Err(lmdb::Error::NotFound) => Err(FinalisedStateError::Custom(format!(
                    "first block must be height 1, got {block_height:?}"
                ))),
                Err(e) => Err(FinalisedStateError::LmdbError(e)),
            }
        })?;
        let hash_key = serde_json::to_vec(&DbHash(zebra_block_hash))?;

        // Delete block data
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };
        tokio::task::block_in_place(|| {
            let mut txn = zaino_db.env.begin_rw_txn()?;

            txn.del(zaino_db.heights_to_hashes, &height_key, None)?;

            txn.del(zaino_db.hashes_to_blocks, &hash_key, None)?;

            let _ = txn.commit();

            self.env
                .sync(true)
                .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
            Ok::<_, FinalisedStateError>(())
        })?;

        Ok(())
    }

    /// Deletes the provided block’s entries from every v0 table.
    ///
    /// This is used as a backup when `delete_block_at_height` fails.
    ///
    /// Takes a IndexedBlock as input and ensures all data from this block is wiped from the database.
    ///
    /// WARNING: No checks are made that this block is at the top of the finalised state, and validated tip is not updated.
    /// This enables use for correcting corrupt data within the database but it is left to the user to ensure safe use.
    /// Where possible delete_block_at_height should be used instead.
    ///
    /// NOTE: LMDB database errors are propageted as these show serious database errors,
    /// all other errors are returned as `IncorrectBlock`, if this error is returned the block requested
    /// should be fetched from the validator and this method called with the correct data.
    pub(crate) async fn delete_block(
        &self,
        block: &IndexedBlock,
    ) -> Result<(), FinalisedStateError> {
        let zebra_height: ZebraHeight = block.context.height().into();
        let zebra_hash: ZebraHash = zebra_chain::block::Hash::from(*block.context.hash());

        let height_key = DbHeight(zebra_height).to_be_bytes();
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;

        // Delete all block data from db.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };
        tokio::task::spawn_blocking(move || {
            // Delete block data
            let mut txn = zaino_db.env.begin_rw_txn()?;

            txn.del(zaino_db.heights_to_hashes, &height_key, None)?;

            txn.del(zaino_db.hashes_to_blocks, &hash_key, None)?;

            let _ = txn.commit();

            zaino_db
                .env
                .sync(true)
                .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;

            Ok::<_, FinalisedStateError>(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("Tokio task error: {e}")))??;
        Ok(())
    }

    // ***** DB fetch methods *****

    /// Returns the greatest `Height` stored in `heights_to_hashes` (`None` if empty).
    ///
    /// Heights are stored as big-endian keys, so the LMDB `MDB_LAST` cursor position corresponds to
    /// the maximum height.
    pub(crate) async fn tip_height(&self) -> Result<Option<crate::Height>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.heights_to_hashes)?;

            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                Ok((height_bytes, _hash_bytes)) => {
                    let tip_height = crate::Height(
                        DbHeight::from_be_bytes(
                            height_bytes.expect("Height is always some in the finalised state"),
                        )?
                        .0
                         .0,
                    );
                    Ok(Some(tip_height))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(e) => Err(FinalisedStateError::LmdbError(e)),
            }
        })
    }

    /// Fetches the block height for a given block hash.
    ///
    /// v0 resolves hash → compact block via `hashes_to_blocks` and then reads the embedded height
    /// from the compact block message.
    async fn get_block_height_by_hash(
        &self,
        hash: crate::BlockHash,
    ) -> Result<crate::Height, FinalisedStateError> {
        let zebra_hash: ZebraHash = zebra_chain::block::Hash::from(hash);
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let block_bytes: &[u8] = txn.get(self.hashes_to_blocks, &hash_key)?;
            let block: DbCompactBlock = serde_json::from_slice(block_bytes)?;
            let block_height = block.0.height as u32;

            Ok(crate::Height(block_height))
        })
    }

    /// Fetches the block hash for a given block height.
    ///
    /// v0 resolves height → hash via `heights_to_hashes`.
    async fn get_block_hash_by_height(
        &self,
        height: crate::Height,
    ) -> Result<crate::BlockHash, FinalisedStateError> {
        let zebra_height: ZebraHeight = height.into();
        let height_key = DbHeight(zebra_height).to_be_bytes();

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let hash_bytes: &[u8] = txn.get(self.heights_to_hashes, &height_key)?;
            let db_hash: DbHash = serde_json::from_slice(hash_bytes)?;

            Ok(crate::BlockHash::from(db_hash.0))
        })
    }

    /// Returns constructed metadata for v0.
    ///
    /// v0 does not persist real metadata. This method returns:
    /// - version `0.0.0`,
    /// - a zero schema hash,
    /// - `MigrationStatus::Complete` (v0 does not participate in resumable migrations).
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        Ok(DbMetadata {
            version: DbVersion {
                major: 0,
                minor: 0,
                patch: 0,
            },
            schema_hash: [0u8; 32],
            migration_status:
                crate::chain_index::finalised_state::capability::MigrationStatus::Complete,
        })
    }

    /// Fetches the compact block for a given height.
    ///
    /// This resolves height → hash via `heights_to_hashes`, then hash → compact block via
    /// `hashes_to_blocks`.
    async fn get_compact_block(
        &self,
        height: crate::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        let zebra_hash =
            zebra_chain::block::Hash::from(self.get_block_hash_by_height(height).await?);
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let block_bytes: &[u8] = txn.get(self.hashes_to_blocks, &hash_key)?;
            let block: DbCompactBlock = serde_json::from_slice(block_bytes)?;
            Ok(compact_block_with_pool_types(
                block.0,
                &pool_types.to_pool_types_vector(),
            ))
        })
    }

    /// Streams `CompactBlock` messages for an inclusive height range.
    ///
    /// Legacy implementation for backwards compatibility.
    ///
    /// Behaviour:
    /// - The stream covers the inclusive range `[start_height, end_height]`.
    /// - If `start_height <= end_height` the stream is ascending; otherwise it is descending.
    /// - Blocks are fetched one-by-one by calling `get_compact_block(height, pool_types)` for
    ///   each height in the range.
    ///
    /// Pool filtering:
    /// - `pool_types` controls which per-transaction components are populated.
    /// - Transactions that have no elements in any requested pool type are omitted from `vtx`,
    ///   and `CompactTx.index` preserves the original transaction index within the block.
    ///
    /// Notes:
    /// - This is intentionally not optimised (no LMDB cursor walk, no batch/range reads).
    /// - Any fetch/deserialize error terminates the stream after emitting a single `tonic::Status`.
    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        let is_ascending: bool = start_height <= end_height;

        let (sender, receiver) =
            tokio::sync::mpsc::channel::<Result<CompactBlock, tonic::Status>>(128);

        let env = self.env.clone();
        let heights_to_hashes_database: lmdb::Database = self.heights_to_hashes;
        let hashes_to_blocks_database: lmdb::Database = self.hashes_to_blocks;

        let pool_types_vector: Vec<PoolType> = pool_types.to_pool_types_vector();

        tokio::task::spawn_blocking(move || {
            fn lmdb_get_status(
                database_name: &'static str,
                height: Height,
                error: lmdb::Error,
            ) -> tonic::Status {
                match error {
                    lmdb::Error::NotFound => tonic::Status::not_found(format!(
                        "missing db entry in {database_name} at height {}",
                        height.0
                    )),
                    other_error => tonic::Status::internal(format!(
                        "lmdb get({database_name}) failed at height {}: {other_error}",
                        height.0
                    )),
                }
            }

            let mut current_height: Height = start_height;

            loop {
                let result: Result<CompactBlock, tonic::Status> = (|| {
                    let txn = env.begin_ro_txn().map_err(|error| {
                        tonic::Status::internal(format!("lmdb begin_ro_txn failed: {error}"))
                    })?;

                    // height -> hash (heights_to_hashes)
                    let zebra_height: ZebraHeight = current_height.into();
                    let height_key: [u8; 4] = DbHeight(zebra_height).to_be_bytes();

                    let hash_bytes: &[u8] = txn
                        .get(heights_to_hashes_database, &height_key)
                        .map_err(|error| {
                            lmdb_get_status("heights_to_hashes", current_height, error)
                        })?;

                    let db_hash: DbHash = serde_json::from_slice(hash_bytes).map_err(|error| {
                        tonic::Status::internal(format!(
                            "height->hash decode failed at height {}: {error}",
                            current_height.0
                        ))
                    })?;

                    // hash -> block (hashes_to_blocks)
                    let hash_key: Vec<u8> =
                        serde_json::to_vec(&DbHash(db_hash.0)).map_err(|error| {
                            tonic::Status::internal(format!(
                                "hash key encode failed at height {}: {error}",
                                current_height.0
                            ))
                        })?;

                    let block_bytes: &[u8] = txn
                        .get(hashes_to_blocks_database, &hash_key)
                        .map_err(|error| {
                            lmdb_get_status("hashes_to_blocks", current_height, error)
                        })?;

                    let db_compact_block: DbCompactBlock = serde_json::from_slice(block_bytes)
                        .map_err(|error| {
                            tonic::Status::internal(format!(
                                "block decode failed at height {}: {error}",
                                current_height.0
                            ))
                        })?;

                    Ok(compact_block_with_pool_types(
                        db_compact_block.0,
                        &pool_types_vector,
                    ))
                })();

                if sender.blocking_send(result).is_err() {
                    return;
                }

                if current_height == end_height {
                    return;
                }

                if is_ascending {
                    let next_value = match current_height.0.checked_add(1) {
                        Some(value) => value,
                        None => {
                            let _ = sender.blocking_send(Err(tonic::Status::internal(
                                "height overflow while iterating ascending".to_string(),
                            )));
                            return;
                        }
                    };
                    current_height = Height(next_value);
                } else {
                    let next_value = match current_height.0.checked_sub(1) {
                        Some(value) => value,
                        None => {
                            let _ = sender.blocking_send(Err(tonic::Status::internal(
                                "height underflow while iterating descending".to_string(),
                            )));
                            return;
                        }
                    };
                    current_height = Height(next_value);
                }
            }
        });

        Ok(CompactBlockStream::new(receiver))
    }
}

/// Wrapper for `ZebraHeight` used for key encoding.
///
/// v0 stores heights as 4-byte **big-endian** keys to preserve numeric ordering under LMDB’s
/// lexicographic key ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DbHeight(pub ZebraHeight);

impl DbHeight {
    /// Converts this height to 4-byte **big-endian** bytes.
    ///
    /// This is used when storing heights as LMDB keys so that increasing heights sort correctly.
    fn to_be_bytes(self) -> [u8; 4] {
        self.0 .0.to_be_bytes()
    }

    /// Parses a 4-byte **big-endian** key into a `DbHeight`.
    ///
    /// # Errors
    /// Returns an error if the key is not exactly 4 bytes long.
    fn from_be_bytes(bytes: &[u8]) -> Result<Self, FinalisedStateError> {
        let arr: [u8; 4] = bytes
            .try_into()
            .map_err(|_| FinalisedStateError::Custom("Invalid height key length".to_string()))?;
        Ok(DbHeight(ZebraHeight(u32::from_be_bytes(arr))))
    }
}

/// Wrapper for `ZebraHash` so it can be JSON-serialized as an LMDB value/key payload.
///
/// v0 stores hashes using `serde_json` rather than Zaino’s versioned binary encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DbHash(pub ZebraHash);

/// Wrapper for `CompactBlock` for JSON storage.
///
/// `CompactBlock` is a Prost message; v0 stores it by encoding to raw bytes and embedding those
/// bytes inside a serde payload.
#[derive(Debug, Clone, PartialEq)]
struct DbCompactBlock(pub CompactBlock);

/// Custom `Serialize` implementation using Prost's `encode_to_vec()`.
///
/// This serializes the compact block as raw bytes so it can be stored via `serde_json` as a byte
/// array payload.
impl Serialize for DbCompactBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let bytes = self.0.encode_to_vec();
        serializer.serialize_bytes(&bytes)
    }
}

/// Custom `Deserialize` implementation using Prost's `decode()`.
///
/// This reverses the `Serialize` strategy by decoding the stored raw bytes into a `CompactBlock`.
impl<'de> Deserialize<'de> for DbCompactBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes: Vec<u8> = serde::de::Deserialize::deserialize(deserializer)?;
        CompactBlock::decode(&*bytes)
            .map(DbCompactBlock)
            .map_err(serde::de::Error::custom)
    }
}
