//! ZainoDB V0 Implementation
//!
//! WARNING: This is a legacy development database and should not be used in production environments.
//!
//! NOTE: This database version was implemented before zaino's `ZainoVersionedSerde` was defined,
//! for this reason ZainoDB-V0 does not use the standard serialisation schema used elswhere in Zaino.

use crate::{
    chain_index::{
        finalised_state::capability::{
            CompactBlockExt, DbCore, DbMetadata, DbRead, DbVersion, DbWrite,
        },
        types::GENESIS_HEIGHT,
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    status::{AtomicStatus, StatusType},
    Height, IndexedBlock,
};

use zaino_proto::proto::compact_formats::CompactBlock;

use zebra_chain::{
    block::{Hash as ZebraHash, Height as ZebraHeight},
    parameters::NetworkKind,
};

use async_trait::async_trait;
use lmdb::{Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{fs, sync::Arc, time::Duration};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{info, warn};

// ───────────────────────── ZainoDb v0 Capabilities ─────────────────────────

#[async_trait]
impl DbRead for DbV0 {
    async fn db_height(&self) -> Result<Option<crate::Height>, FinalisedStateError> {
        self.tip_height().await
    }

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

    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.get_metadata().await
    }
}

#[async_trait]
impl DbWrite for DbV0 {
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.write_block(block).await
    }

    async fn delete_block_at_height(
        &self,
        height: crate::Height,
    ) -> Result<(), FinalisedStateError> {
        self.delete_block_at_height(height).await
    }

    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.delete_block(block).await
    }

    /// NOTE: V0 does not hold metadata!
    async fn update_metadata(&self, _metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        Ok(())
    }
}

#[async_trait]
impl DbCore for DbV0 {
    fn status(&self) -> StatusType {
        self.status.load()
    }

    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Closing);

        if let Some(handle) = &self.db_handler {
            let timeout = tokio::time::sleep(Duration::from_secs(5));
            timeout.await;
            // TODO: Check if handle is returned else abort
            handle.abort();
        }
        let _ = self.clean_trailing().await;
        if let Err(e) = self.env.sync(true) {
            warn!("LMDB fsync before close failed: {e}");
        }
        Ok(())
    }
}

#[async_trait]
impl CompactBlockExt for DbV0 {
    async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        self.get_compact_block(height).await
    }
}

/// Finalised part of the chain, held in an LMDB database.
#[derive(Debug)]
pub struct DbV0 {
    /// LMDB Database Environmant.
    env: Arc<Environment>,

    /// LMDB Databas containing `<block_height, block_hash>`.
    heights_to_hashes: Database,

    /// LMDB Databas containing `<block_hash, compact_block>`.
    hashes_to_blocks: Database,

    /// Database handler task handle.
    db_handler: Option<tokio::task::JoinHandle<()>>,

    /// Non-finalised state status.
    status: AtomicStatus,
    /// BlockCache config data.
    config: BlockCacheConfig,
}

impl DbV0 {
    /// Spawns a new [`DbV0`] and syncs the FinalisedState to the servers finalised state.
    ///
    /// Uses ReadStateService to fetch chain data if given else uses JsonRPC client.
    ///
    /// Inputs:
    /// - config: ChainIndexConfig.
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
            db_handler: None,
            status: AtomicStatus::new(StatusType::Spawning),
            config: config.clone(),
        };

        // Spawn handler task to perform background validation and trailing tx cleanup.
        zaino_db.spawn_handler().await?;

        Ok(zaino_db)
    }

    /// Try graceful shutdown, fall back to abort after a timeout.
    pub(crate) async fn close(&mut self) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Closing);

        if let Some(mut handle) = self.db_handler.take() {
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

    /// Returns the status of ZainoDB.
    pub(crate) fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Awaits until the DB returns a Ready status.
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

    /// Spawns the background validator / maintenance task.
    ///
    /// *   **Startup** – runs a full‐DB validation pass (`initial_root_scan` →
    ///     `initial_block_scan`).
    /// *   **Steady-state** – every 5 s tries to validate the next block that
    ///     appeared after the current `validated_tip`.
    ///     Every 60 s it also calls `clean_trailing()` to purge stale reader slots.
    async fn spawn_handler(&mut self) -> Result<(), FinalisedStateError> {
        // Clone everything the task needs so we can move it into the async block.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            db_handler: None,
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

        self.db_handler = Some(handle);
        Ok(())
    }

    /// Helper method to wait for the next loop iteration or perform maintenance.
    async fn zaino_db_handler_sleep(&self, maintenance: &mut tokio::time::Interval) {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            _ = maintenance.tick() => {
                if let Err(e) = self.clean_trailing().await {
                    warn!("clean_trailing failed: {}", e);
                }
            }
        }
    }

    /// Clears stale reader slots by opening and closing a read transaction.
    async fn clean_trailing(&self) -> Result<(), FinalisedStateError> {
        let txn = self.env.begin_ro_txn()?;
        drop(txn);
        Ok(())
    }

    /// Opens an lmdb database if present else creates a new one.
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

    /// Writes a given (finalised) [`IndexedBlock`] to ZainoDB.
    pub(crate) async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.status.store(StatusType::Syncing);

        let compact_block: CompactBlock = block.to_compact_block();
        let zebra_height: ZebraHeight = block
            .index()
            .height()
            .expect("height always some in the finalised state")
            .into();
        let zebra_hash: ZebraHash = zebra_chain::block::Hash::from(*block.index().hash());

        let height_key = DbHeight(zebra_height).to_be_bytes();
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;
        let block_value = serde_json::to_vec(&DbCompactBlock(compact_block))?;

        // check this is the *next* block in the chain.
        let block_height = block
            .index()
            .height()
            .expect("height always some in finalised state")
            .0;

        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.heights_to_hashes)?;

            // Position the cursor at the last header we currently have
            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                // Database already has blocks
                Ok((last_height_bytes, _last_hash_bytes)) => {
                    let block_height = block
                        .index()
                        .height()
                        .expect("height always some in finalised state")
                        .0;

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
            db_handler: None,
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
                    hash: *block.index().hash(),
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Deletes a block identified height from every finalised table.
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
            db_handler: None,
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

    /// This is used as a backup when delete_block_at_height fails.
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
        let zebra_height: ZebraHeight = block
            .index()
            .height()
            .expect("height always some in the finalised state")
            .into();
        let zebra_hash: ZebraHash = zebra_chain::block::Hash::from(*block.index().hash());

        let height_key = DbHeight(zebra_height).to_be_bytes();
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;

        // Delete all block data from db.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            db_handler: None,
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

    // Returns the greatest `Height` stored in `headers`
    /// (`None` if the DB is still empty).
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

    /// Fetch the block height in the main chain for a given block hash.
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

    async fn get_compact_block(
        &self,
        height: crate::Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        let zebra_hash =
            zebra_chain::block::Hash::from(self.get_block_hash_by_height(height).await?);
        let hash_key = serde_json::to_vec(&DbHash(zebra_hash))?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let block_bytes: &[u8] = txn.get(self.hashes_to_blocks, &hash_key)?;
            let block: DbCompactBlock = serde_json::from_slice(block_bytes)?;
            Ok(block.0)
        })
    }
}

/// Wrapper for `Height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DbHeight(pub ZebraHeight);

impl DbHeight {
    /// Converts `[DbHeight]` to 4-byte **big-endian** bytes.
    /// Used when storing as an LMDB key.
    fn to_be_bytes(self) -> [u8; 4] {
        self.0 .0.to_be_bytes()
    }

    /// Parse a 4-byte **big-endian** array into a `[DbHeight]`.
    fn from_be_bytes(bytes: &[u8]) -> Result<Self, FinalisedStateError> {
        let arr: [u8; 4] = bytes
            .try_into()
            .map_err(|_| FinalisedStateError::Custom("Invalid height key length".to_string()))?;
        Ok(DbHeight(ZebraHeight(u32::from_be_bytes(arr))))
    }
}

/// Wrapper for `Hash`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DbHash(pub ZebraHash);

/// Wrapper for `CompactBlock`.
#[derive(Debug, Clone, PartialEq)]
struct DbCompactBlock(pub CompactBlock);

/// Custom `Serialize` implementation using Prost's `encode_to_vec()`.
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
