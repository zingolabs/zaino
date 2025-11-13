//! ZainoDB V1 Implementation

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
        types::{AddrEventBytes, TransactionHash, GENESIS_HEIGHT},
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    AddrHistRecord, AddrScript, AtomicStatus, BlockHash, BlockHeaderData, CommitmentTreeData,
    CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactSize, CompactTxData,
    FixedEncodedLen as _, Height, IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint,
    SaplingCompactTx, SaplingTxList, StatusType, TransparentCompactTx, TransparentTxList,
    TxInCompact, TxLocation, TxOutCompact, TxidList, ZainoVersionedSerde as _,
};

use zebra_chain::parameters::NetworkKind;
use zebra_state::HashOrHeight;

use async_trait::async_trait;
use core2::io::{self, Read};
use dashmap::DashSet;
use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction as _, WriteFlags,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, warn};

// ───────────────────────── Schema v1 constants ─────────────────────────

/// Full V1 schema text file.
// 1. Bring the *exact* ASCII description of the on-disk layout into the binary
//    at compile-time.  The path is relative to this source file.
pub(crate) const DB_SCHEMA_V1_TEXT: &str = include_str!("db_schema_v1_0.txt");

/*
2. Compute the checksum once, outside the code:

       $ cd zaino-state/src/chain_index/finalised_state/db
       $ b2sum -l 256 db_schema_v1_0.txt
       bc135247b46bb46a4a971e4c2707826f8095e662b6919d28872c71b6bd676593  db_schema_v1_0.txt

   Optional helper if you don’t have `b2sum`:

       $ python - <<'PY'
       > import hashlib, pathlib, binascii
       > data = pathlib.Path("db_schema_v1.txt").read_bytes()
       > print(hashlib.blake2b(data, digest_size=32).hexdigest())
       > PY

3. Turn those 64 hex digits into a Rust `[u8; 32]` literal:

       echo bc135247b46bb46a4a971e4c2707826f8095e662b6919d28872c71b6bd676593 \
       | sed 's/../0x&, /g' | fold -s -w48

*/

/// *Current* database V1 schema hash, used for version validation.
pub(crate) const DB_SCHEMA_V1_HASH: [u8; 32] = [
    0xbc, 0x13, 0x52, 0x47, 0xb4, 0x6b, 0xb4, 0x6a, 0x4a, 0x97, 0x1e, 0x4c, 0x27, 0x07, 0x82, 0x6f,
    0x80, 0x95, 0xe6, 0x62, 0xb6, 0x91, 0x9d, 0x28, 0x87, 0x2c, 0x71, 0xb6, 0xbd, 0x67, 0x65, 0x93,
];

/// *Current* database V1 version.
pub(crate) const DB_VERSION_V1: DbVersion = DbVersion {
    major: 1,
    minor: 0,
    patch: 0,
};

// ───────────────────────── ZainoDb v1 Capabilities ─────────────────────────

#[async_trait]
impl DbRead for DbV1 {
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.tip_height().await
    }

    async fn get_block_height(
        &self,
        hash: BlockHash,
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
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        match self.get_block_header_data(height).await {
            Ok(header) => Ok(Some(*header.index().hash())),
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
impl DbWrite for DbV1 {
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.write_block(block).await
    }

    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalisedStateError> {
        self.delete_block_at_height(height).await
    }

    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.delete_block(block).await
    }

    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        self.update_metadata(metadata).await
    }
}

#[async_trait]
impl DbCore for DbV1 {
    fn status(&self) -> StatusType {
        self.status()
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
impl BlockCoreExt for DbV1 {
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        self.get_block_header_data(height).await
    }

    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        self.get_block_range_headers(start, end).await
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        self.get_block_txids(height).await
    }

    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        self.get_block_range_txids(start, end).await
    }

    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        self.get_txid(tx_location).await
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        self.get_tx_location(txid).await
    }
}

#[async_trait]
impl BlockTransparentExt for DbV1 {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        self.get_transparent(tx_location).await
    }

    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        self.get_block_transparent(height).await
    }

    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        self.get_block_range_transparent(start, end).await
    }
}

#[async_trait]
impl BlockShieldedExt for DbV1 {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        self.get_sapling(tx_location).await
    }

    async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        self.get_block_sapling(height).await
    }

    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
        self.get_block_range_sapling(start, end).await
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        self.get_orchard(tx_location).await
    }

    async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        self.get_block_orchard(height).await
    }

    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        self.get_block_range_orchard(start, end).await
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        self.get_block_commitment_tree_data(height).await
    }

    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
        self.get_block_range_commitment_tree_data(start, end).await
    }
}

#[async_trait]
impl CompactBlockExt for DbV1 {
    async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        self.get_compact_block(height).await
    }
}

#[async_trait]
impl IndexedBlockExt for DbV1 {
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        self.get_chain_block(height).await
    }
}

#[async_trait]
impl TransparentHistExt for DbV1 {
    async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        self.addr_records(addr_script).await
    }

    async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        self.addr_and_index_records(addr_script, tx_location).await
    }

    async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError> {
        self.addr_tx_locations_by_range(addr_script, start_height, end_height)
            .await
    }

    async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError> {
        self.addr_utxos_by_range(addr_script, start_height, end_height)
            .await
    }

    async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, FinalisedStateError> {
        self.addr_balance_by_range(addr_script, start_height, end_height)
            .await
    }

    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        self.get_outpoint_spender(outpoint).await
    }

    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError> {
        self.get_outpoint_spenders(outpoints).await
    }
}

// ───────────────────────── ZainoDb v1 Implementation ─────────────────────────

/// Zaino’s Finalised state database V1.
/// Implements a persistent LMDB-backed chain index for fast read access and verified data.
pub(crate) struct DbV1 {
    /// Shared LMDB environment.
    env: Arc<Environment>,

    /// Block headers: `Height` -> `StoredEntry<BlockHeaderData>`
    ///
    /// Stored per-block, in order.
    headers: Database,
    /// Txids: `Height` -> `StoredEntry<TxidList>`
    ///
    /// Stored per-block, in order.
    txids: Database,
    /// Transparent: `Height` -> `StoredEntry<Vec<TransparentTxList>>`
    ///
    /// Stored per-block, in order.
    transparent: Database,
    /// Sapling: `Height` -> `StoredEntry<Vec<TxData>>`
    ///
    /// Stored per-block, in order.
    sapling: Database,
    /// Orchard: `Height` -> `StoredEntry<Vec<TxData>>`
    ///
    /// Stored per-block, in order.
    orchard: Database,
    /// Block commitment tree data: `Height` -> `StoredEntry<Vec<CommitmentTreeData>>`
    ///
    /// Stored per-block, in order.
    commitment_tree_data: Database,
    /// Heights: `Hash` -> `StoredEntry<Height>`
    ///
    /// Used for hash based fetch of the best chain (and random access).
    heights: Database,
    /// Spent outpoints: `Outpoint` -> `StoredEntry<Vec<TxLocation>>`
    ///
    /// Used to check spent status of given outpoints, retuning spending tx.
    spent: Database,
    /// Transparent address history: `AddrScript` -> `StoredEntry<AddrEventBytes>`
    ///
    /// Used to search all transparent address indexes (txids, utxos, balances, deltas)
    address_history: Database,
    /// Metadata: singleton entry "metadata" -> `StoredEntry<DbMetadata>`
    metadata: Database,

    /// Contiguous **water-mark**: every height ≤ `validated_tip` is known-good.
    ///
    /// Wrapped in an `Arc` so the background validator and any foreground tasks
    /// all see (and update) the **same** atomic.
    validated_tip: Arc<AtomicU32>,
    /// Heights **above** the tip that have also been validated.
    ///
    /// Whenever the next consecutive height is inserted we pop it
    /// out of this set and bump `validated_tip`, so the map never
    /// grows beyond the number of “holes” in the sequence.
    validated_set: DashSet<u32>,

    /// Database handler task handle.
    db_handler: Option<tokio::task::JoinHandle<()>>,

    /// ZainoDB status.
    status: AtomicStatus,

    /// BlockCache config data.
    config: BlockCacheConfig,
}

impl DbV1 {
    /// Spawns a new [`DbV1`] and syncs the FinalisedState to the servers finalised state.
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
            .set_max_dbs(12)
            .set_map_size(db_size_bytes)
            .set_max_readers(max_readers)
            .set_flags(EnvironmentFlags::NO_TLS | EnvironmentFlags::NO_READAHEAD)
            .open(&db_path)?;

        // Open individual LMDB DBs.
        let headers =
            Self::open_or_create_db(&env, "headers_1_0_0", DatabaseFlags::empty()).await?;
        let txids = Self::open_or_create_db(&env, "txids_1_0_0", DatabaseFlags::empty()).await?;
        let transparent =
            Self::open_or_create_db(&env, "transparent_1_0_0", DatabaseFlags::empty()).await?;
        let sapling =
            Self::open_or_create_db(&env, "sapling_1_0_0", DatabaseFlags::empty()).await?;
        let orchard =
            Self::open_or_create_db(&env, "orchard_1_0_0", DatabaseFlags::empty()).await?;
        let commitment_tree_data =
            Self::open_or_create_db(&env, "commitment_tree_data_1_0_0", DatabaseFlags::empty())
                .await?;
        let hashes = Self::open_or_create_db(&env, "hashes_1_0_0", DatabaseFlags::empty()).await?;
        let spent = Self::open_or_create_db(&env, "spent_1_0_0", DatabaseFlags::empty()).await?;
        let address_history = Self::open_or_create_db(
            &env,
            "address_history_1_0_0",
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .await?;
        let metadata = Self::open_or_create_db(&env, "metadata", DatabaseFlags::empty()).await?;

        // Create ZainoDB
        let mut zaino_db = Self {
            env: Arc::new(env),
            headers,
            txids,
            transparent,
            sapling,
            orchard,
            commitment_tree_data,
            heights: hashes,
            spent,
            address_history,
            metadata,
            validated_tip: Arc::new(AtomicU32::new(0)),
            validated_set: DashSet::new(),
            db_handler: None,
            status: AtomicStatus::new(StatusType::Spawning),
            config: config.clone(),
        };

        // Validate (or initialise) the metadata entry before we touch any tables.
        zaino_db.check_schema_version().await?;

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
    ///
    /// TODO: check db for free readers and wait if busy.
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
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            spent: self.spent,
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: None,
            status: self.status.clone(),
            config: self.config.clone(),
        };

        let handle = tokio::spawn({
            let zaino_db = zaino_db;
            async move {
                // *** initial validation ***
                zaino_db.status.store(StatusType::Syncing);
                let (r1, r2, r3) = tokio::join!(
                    zaino_db.initial_spent_scan(),
                    zaino_db.initial_address_history_scan(),
                    zaino_db.initial_block_scan(),
                );

                for (desc, result) in [
                    ("spent scan", r1),
                    ("addrhist scan", r2),
                    ("block scan", r3),
                ] {
                    if let Err(e) = result {
                        error!("initial {desc} failed: {e}");
                        zaino_db.status.store(StatusType::CriticalError);
                        // TODO: Handle error better? - Return invalid block error from validate?
                        return;
                    }
                }

                info!(
                    "initial validation complete – tip={}",
                    zaino_db.validated_tip.load(Ordering::Relaxed)
                );
                zaino_db.status.store(StatusType::Ready);

                // *** steady-state loop ***
                let mut maintenance = interval(Duration::from_secs(60));

                loop {
                    // Check for closing status.
                    if zaino_db.status.load() == StatusType::Closing {
                        break;
                    }
                    // try to validate the next consecutive block.
                    let next_h = zaino_db.validated_tip.load(Ordering::Acquire) + 1;
                    let next_height = match Height::try_from(next_h) {
                        Ok(h) => h,
                        Err(_) => {
                            warn!("height overflow – validated_tip too large");
                            zaino_db.zaino_db_handler_sleep(&mut maintenance).await;
                            continue;
                        }
                    };

                    // Fetch hash of `next_h` from Heights.
                    let hkey = match next_height.to_bytes() {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!("Failed to serialize height {}: {}", next_height, e);
                            zaino_db.zaino_db_handler_sleep(&mut maintenance).await;
                            continue;
                        }
                    };

                    let hash_opt = (|| -> Option<BlockHash> {
                        let ro = zaino_db.env.begin_ro_txn().ok()?;
                        let bytes = ro.get(zaino_db.headers, &hkey).ok()?;
                        let entry = StoredEntryVar::<BlockHeaderData>::deserialize(bytes).ok()?;
                        Some(*entry.inner().index().hash())
                    })();

                    if let Some(hash) = hash_opt {
                        if let Err(e) = zaino_db.validate_block_blocking(next_height, hash) {
                            warn!("{e}");
                        }
                        // Immediately loop – maybe the chain has more blocks ready.
                        continue;
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

    /// Validate every stored `TxLocation`.
    async fn initial_spent_scan(&self) -> Result<(), FinalisedStateError> {
        let env = self.env.clone();
        let spent = self.spent;

        tokio::task::spawn_blocking(move || {
            let ro = env.begin_ro_txn()?;
            let mut cursor = ro.open_ro_cursor(spent)?;

            for (key_bytes, val_bytes) in cursor.iter() {
                let entry = StoredEntryFixed::<TxLocation>::from_bytes(val_bytes).map_err(|e| {
                    FinalisedStateError::Custom(format!("corrupt spent entry: {e}"))
                })?;

                if !entry.verify(key_bytes) {
                    return Err(FinalisedStateError::Custom(
                        "spent record checksum mismatch".into(),
                    ));
                }
            }

            Ok(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("Tokio task error: {e}")))?
    }

    /// Validate every stored `AddrEventBytes`.
    async fn initial_address_history_scan(&self) -> Result<(), FinalisedStateError> {
        let env = self.env.clone();
        let address_history = self.address_history;

        tokio::task::spawn_blocking(move || {
            let ro = env.begin_ro_txn()?;
            let mut cursor = ro.open_ro_cursor(address_history)?;

            for (addr_bytes, record_bytes) in cursor.iter() {
                let entry =
                    StoredEntryFixed::<AddrEventBytes>::from_bytes(record_bytes).map_err(|e| {
                        FinalisedStateError::Custom(format!("corrupt addrhist entry: {e}"))
                    })?;

                if !entry.verify(addr_bytes) {
                    return Err(FinalisedStateError::Custom(
                        "addrhist record checksum mismatch".into(),
                    ));
                }
            }

            Ok(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("spawn_blocking failed: {e}")))?
    }

    /// Scan the whole finalised chain once at start-up and validate every block.
    async fn initial_block_scan(&self) -> Result<(), FinalisedStateError> {
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            spent: self.spent,
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: None,
            status: self.status.clone(),
            config: self.config.clone(),
        };

        tokio::task::spawn_blocking(move || {
            let ro = zaino_db.env.begin_ro_txn()?;
            let mut cursor = ro.open_ro_cursor(zaino_db.heights)?;

            for (hash_bytes, height_entry_bytes) in cursor.iter() {
                let hash = BlockHash::from_bytes(hash_bytes)?;
                let height = *StoredEntryFixed::<Height>::from_bytes(height_entry_bytes)
                    .map_err(|e| FinalisedStateError::Custom(format!("corrupt height entry: {e}")))?
                    .inner();

                zaino_db.validate_block_blocking(height, hash)?
            }

            Ok(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("spawn_blocking failed: {e}")))?
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
        let block_hash = *block.index().hash();
        let block_hash_bytes = block_hash.to_bytes()?;
        let block_height = block.index().height().ok_or(FinalisedStateError::Custom(
            "finalised state received non finalised block".to_string(),
        ))?;
        let block_height_bytes = block_height.to_bytes()?;

        // check this is the *next* block in the chain.
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.headers)?;

            // Position the cursor at the last header we currently have
            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                // Database already has blocks
                Ok((last_height_bytes, _last_header_bytes)) => {
                    let last_height = Height::from_bytes(
                        last_height_bytes.expect("Height is always some in the finalised state"),
                    )?;

                    // Height must be exactly +1 over the current tip
                    if block_height.0 != last_height.0 + 1 {
                        return Err(FinalisedStateError::Custom(format!(
                            "cannot write block at height {block_height:?}; \
                     current tip is {last_height:?}"
                        )));
                    }
                }
                // no block in db, this must be genesis block.
                Err(lmdb::Error::NotFound) => {
                    if block_height.0 != GENESIS_HEIGHT.0 {
                        return Err(FinalisedStateError::Custom(format!(
                            "first block must be height 0, got {block_height:?}"
                        )));
                    }
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }
            Ok::<_, FinalisedStateError>(())
        })?;

        // Build DBHeight
        let height_entry = StoredEntryFixed::new(
            &block_hash_bytes,
            block.index().height().ok_or(FinalisedStateError::Custom(
                "finalised state received non finalised block".to_string(),
            ))?,
        );

        // Build header
        let header_entry = StoredEntryVar::new(
            &block_height_bytes,
            BlockHeaderData::new(*block.index(), *block.data()),
        );

        // Build commitment tree data
        let commitment_tree_entry =
            StoredEntryFixed::new(&block_height_bytes, *block.commitment_tree_data());

        // Build transaction indexes
        let tx_len = block.transactions().len();
        let mut txids = Vec::with_capacity(tx_len);
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
        let mut transparent = Vec::with_capacity(tx_len);
        let mut sapling = Vec::with_capacity(tx_len);
        let mut orchard = Vec::with_capacity(tx_len);

        let mut spent_map: HashMap<Outpoint, TxLocation> = HashMap::new();
        #[allow(clippy::type_complexity)]
        let mut addrhist_inputs_map: HashMap<
            AddrScript,
            Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>,
        > = HashMap::new();
        let mut addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>> = HashMap::new();

        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let hash = tx.txid();

            if txid_set.insert(*hash) {
                txids.push(*hash);
            }

            // Transparent transactions
            let transparent_data =
                if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
            transparent.push(transparent_data);

            // Sapling transactions
            let sapling_data = if tx.sapling().value().is_none() {
                None
            } else {
                Some(tx.sapling().clone())
            };
            sapling.push(sapling_data);

            // Orchard transactions
            let orchard_data = if tx.orchard().value().is_none() {
                None
            } else {
                Some(tx.orchard().clone())
            };
            orchard.push(orchard_data);

            // Transaction location
            let tx_location = TxLocation::new(block_height.into(), tx_index as u16);

            // Transparent Outputs: Build Address History
            DbV1::build_transaction_output_histories(
                &mut addrhist_outputs_map,
                tx_location,
                tx.transparent().outputs().iter().enumerate(),
            );

            // Transparent Inputs: Build Spent Outpoints Index and Address History
            for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                if input.is_null_prevout() {
                    continue;
                }
                let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                spent_map.insert(prev_outpoint, tx_location);

                //Check if output is in *this* block, else fetch from DB.
                let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                if txid_set.contains(&prev_tx_hash) {
                    // Fetch transaction index within block
                    if let Some(tx_index) = txids.iter().position(|h| h == &prev_tx_hash) {
                        // Fetch Transparent data for transaction
                        if let Some(Some(prev_transparent)) = transparent.get(tx_index) {
                            // Fetch output from transaction
                            if let Some(prev_output) = prev_transparent
                                .outputs()
                                .get(prev_outpoint.prev_index() as usize)
                            {
                                let prev_output_tx_location =
                                    TxLocation::new(block_height.0, tx_index as u16);
                                DbV1::build_input_history(
                                    &mut addrhist_inputs_map,
                                    tx_location,
                                    input_index as u16,
                                    input,
                                    prev_output,
                                    prev_output_tx_location,
                                );
                            }
                        }
                    }
                } else if let Ok((prev_output, prev_output_tx_location)) =
                    tokio::task::block_in_place(|| {
                        let prev_output = self.get_previous_output_blocking(prev_outpoint)?;
                        let prev_output_tx_location = self
                            .find_txid_index_blocking(&TransactionHash::from(
                                *prev_outpoint.prev_txid(),
                            ))?
                            .ok_or_else(|| {
                                FinalisedStateError::Custom("Previous txid not found".into())
                            })?;
                        Ok::<(_, _), FinalisedStateError>((prev_output, prev_output_tx_location))
                    })
                {
                    DbV1::build_input_history(
                        &mut addrhist_inputs_map,
                        tx_location,
                        input_index as u16,
                        input,
                        &prev_output,
                        prev_output_tx_location,
                    );
                } else {
                    return Err(FinalisedStateError::InvalidBlock {
                        height: block.height().expect("already  checked height is some").0,
                        hash: *block.hash(),
                        reason: "Invalid block data: invalid transparent input.".to_string(),
                    });
                }
            }
        }

        let txid_entry = StoredEntryVar::new(&block_height_bytes, TxidList::new(txids));
        let transparent_entry =
            StoredEntryVar::new(&block_height_bytes, TransparentTxList::new(transparent));
        let sapling_entry = StoredEntryVar::new(&block_height_bytes, SaplingTxList::new(sapling));
        let orchard_entry = StoredEntryVar::new(&block_height_bytes, OrchardTxList::new(orchard));

        // if any database writes fail, or block validation fails, remove block from database and return err.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            spent: self.spent,
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: None,
            status: self.status.clone(),
            config: self.config.clone(),
        };
        let post_result = tokio::task::spawn_blocking(move || {
            // let post_result: Result<(), FinalisedStateError> = (async {
            // Write block to ZainoDB
            let mut txn = zaino_db.env.begin_rw_txn()?;

            txn.put(
                zaino_db.headers,
                &block_height_bytes,
                &header_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.heights,
                &block_hash_bytes,
                &height_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.txids,
                &block_height_bytes,
                &txid_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.transparent,
                &block_height_bytes,
                &transparent_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.sapling,
                &block_height_bytes,
                &sapling_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.orchard,
                &block_height_bytes,
                &orchard_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.put(
                zaino_db.commitment_tree_data,
                &block_height_bytes,
                &commitment_tree_entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.commit()?;

            // Write spent to ZainoDB
            let mut txn = zaino_db.env.begin_rw_txn()?;

            for (outpoint, tx_location) in spent_map {
                let outpoint_bytes = &outpoint.to_bytes()?;
                let tx_location_entry_bytes =
                    StoredEntryFixed::new(outpoint_bytes, tx_location).to_bytes()?;
                txn.put(
                    zaino_db.spent,
                    &outpoint_bytes,
                    &tx_location_entry_bytes,
                    WriteFlags::NO_OVERWRITE,
                )?;
            }

            txn.commit()?;

            // Write outputs to ZainoDB addrhist
            let mut txn = zaino_db.env.begin_rw_txn()?;

            for (addr_script, records) in addrhist_outputs_map {
                let addr_bytes = addr_script.to_bytes()?;

                // Convert all records to their StoredEntryFixed<AddrEventBytes> for ordering.
                let mut stored_entries = Vec::with_capacity(records.len());
                for record in records {
                    let packed_record = AddrEventBytes::from_record(&record).map_err(|e| {
                        FinalisedStateError::Custom(format!("AddrEventBytes pack error: {e:?}"))
                    })?;
                    let entry = StoredEntryFixed::new(&addr_bytes, packed_record);
                    let entry_bytes = entry.to_bytes()?;
                    stored_entries.push((record, entry_bytes));
                }

                // Order by byte encoding for LMDB DUP_SORT insertion order
                stored_entries.sort_by(|a, b| a.1.cmp(&b.1));

                for (_record, record_entry_bytes) in stored_entries {
                    txn.put(
                        zaino_db.address_history,
                        &addr_bytes,
                        &record_entry_bytes,
                        WriteFlags::empty(),
                    )?;
                }
            }

            txn.commit()?;

            // Write inputs to ZainoDB addrhist
            for (addr_script, records) in addrhist_inputs_map {
                let addr_bytes = addr_script.to_bytes()?;

                // Convert all records to their StoredEntryFixed<AddrEventBytes> for ordering.
                let mut stored_entries = Vec::with_capacity(records.len());
                for (record, prev_output) in records {
                    let packed_record = AddrEventBytes::from_record(&record).map_err(|e| {
                        FinalisedStateError::Custom(format!("AddrEventBytes pack error: {e:?}"))
                    })?;
                    let entry = StoredEntryFixed::new(&addr_bytes, packed_record);
                    let entry_bytes = entry.to_bytes()?;
                    stored_entries.push((record, entry_bytes, prev_output));
                }

                // Order by byte encoding for LMDB DUP_SORT insertion order
                stored_entries.sort_by(|a, b| a.1.cmp(&b.1));

                for (_record, record_entry_bytes, (prev_output_script, prev_output_record)) in
                    stored_entries
                {
                    let mut txn = zaino_db.env.begin_rw_txn()?;
                    txn.put(
                        zaino_db.address_history,
                        &addr_bytes,
                        &record_entry_bytes,
                        WriteFlags::empty(),
                    )?;
                    txn.commit()?;
                    // mark corresponding output as spent
                    let _updated = zaino_db.mark_addr_hist_record_spent_blocking(
                        &prev_output_script,
                        prev_output_record.tx_location(),
                        prev_output_record.out_index(),
                    )?;
                }
            }

            zaino_db.validate_block_blocking(block_height, block_hash)?;

            Ok::<_, FinalisedStateError>(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("Tokio task error: {e}")))?;

        match post_result {
            Ok(_) => {
                tokio::task::block_in_place(|| self.env.sync(true))
                    .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
                self.status.store(StatusType::Ready);

                info!(
                    "Successfully committed block {} at height {} to ZainoDB.",
                    &block.index().hash(),
                    &block
                        .index()
                        .height()
                        .expect("height always some in the finalised state")
                );

                Ok(())
            }
            Err(e) => {
                warn!("Error writing block to DB.");

                let _ = self.delete_block(&block).await;
                tokio::task::block_in_place(|| self.env.sync(true))
                    .map_err(|e| FinalisedStateError::Custom(format!("LMDB sync failed: {e}")))?;
                self.status.store(StatusType::RecoverableError);
                Err(FinalisedStateError::InvalidBlock {
                    height: block_height.0,
                    hash: block_hash,
                    reason: e.to_string(),
                })
            }
        }
    }

    /// Deletes a block identified height from every finalised table.
    pub(crate) async fn delete_block_at_height(
        &self,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        // Check block is at the top of the finalised state
        tokio::task::block_in_place(|| {
            let height_bytes = height.to_bytes()?;
            let ro = self.env.begin_ro_txn()?;
            let mut cursor = ro.open_ro_cursor(self.headers)?;

            let mut iter = cursor.iter_from(&height_bytes);

            let Some((current_height_bytes, _)) = iter.next() else {
                return Err(FinalisedStateError::Custom("block not found".into()));
            };
            if current_height_bytes != height_bytes.as_slice() {
                return Err(FinalisedStateError::Custom(format!(
                    "block with height {:?} not found in headers",
                    Height::from_bytes(&height_bytes)?
                )));
            }

            if iter.next().is_some() {
                return Err(FinalisedStateError::Custom(format!(
                    "can only delete tip block at height {:?}, but higher blocks exist",
                    Height::from_bytes(&height_bytes)?
                )));
            }
            Ok::<_, FinalisedStateError>(())
        })?;

        // fetch chain_block from db and delete
        let Some(chain_block) = self.get_chain_block(height).await? else {
            return Err(FinalisedStateError::DataUnavailable(format!(
                "attempted to delete missing block: {}",
                height.0
            )));
        };
        self.delete_block(&chain_block).await?;

        // update validated_tip / validated_set
        let validated_tip = self.validated_tip.load(Ordering::Acquire);
        if height.0 > validated_tip {
            self.validated_set.remove(&height.0);
        } else if height.0 == validated_tip {
            self.validated_tip
                .store(validated_tip.saturating_sub(1), Ordering::Release);
        }

        tokio::task::block_in_place(|| {
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
    /// The IndexedBlock ir required to ensure that Outputs spent at this block height are re-marked as unspent.
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
        // Check block height and hash
        let block_height = block
            .index()
            .height()
            .ok_or(FinalisedStateError::InvalidBlock {
                height: 0,
                hash: *block.hash(),
                reason: "Invalid block data: Block does not contain finalised height".to_string(),
            })?;
        let block_height_bytes =
            block_height
                .to_bytes()
                .map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block.height().expect("already  checked height is some").0,
                    hash: *block.hash(),
                    reason: "Corrupt block data: failed to serialise hash".to_string(),
                })?;

        let block_hash = *block.index().hash();
        let block_hash_bytes =
            block_hash
                .to_bytes()
                .map_err(|_| FinalisedStateError::InvalidBlock {
                    height: block.height().expect("already  checked height is some").0,
                    hash: *block.hash(),
                    reason: "Corrupt block data: failed to serialise hash".to_string(),
                })?;

        // Build transaction indexes
        let tx_len = block.transactions().len();
        let mut txids = Vec::with_capacity(tx_len);
        let mut txid_set: HashSet<TransactionHash> = HashSet::with_capacity(tx_len);
        let mut transparent = Vec::with_capacity(tx_len);
        let mut spent_map: Vec<Outpoint> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut addrhist_inputs_map: HashMap<
            AddrScript,
            Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>,
        > = HashMap::new();
        let mut addrhist_outputs_map: HashMap<AddrScript, Vec<AddrHistRecord>> = HashMap::new();

        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let hash = tx.txid();

            if txid_set.insert(*hash) {
                txids.push(*hash);
            }

            // Transparent transactions
            let transparent_data =
                if tx.transparent().inputs().is_empty() && tx.transparent().outputs().is_empty() {
                    None
                } else {
                    Some(tx.transparent().clone())
                };
            transparent.push(transparent_data);

            // Transaction location
            let tx_location = TxLocation::new(block_height.into(), tx_index as u16);

            // Transparent Outputs: Build Address History
            DbV1::build_transaction_output_histories(
                &mut addrhist_outputs_map,
                tx_location,
                tx.transparent().outputs().iter().enumerate(),
            );

            // Transparent Inputs: Build Spent Outpoints Index and Address History
            for (input_index, input) in tx.transparent().inputs().iter().enumerate() {
                if input.is_null_prevout() {
                    continue;
                }
                let prev_outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                spent_map.push(prev_outpoint);

                //Check if output is in *this* block, else fetch from DB.
                let prev_tx_hash = TransactionHash(*prev_outpoint.prev_txid());
                if txid_set.contains(&prev_tx_hash) {
                    // Fetch transaction index within block
                    if let Some(tx_index) = txids.iter().position(|h| h == &prev_tx_hash) {
                        // Fetch Transparent data for transaction
                        if let Some(Some(prev_transparent)) = transparent.get(tx_index) {
                            // Fetch output from transaction
                            if let Some(prev_output) = prev_transparent
                                .outputs()
                                .get(prev_outpoint.prev_index() as usize)
                            {
                                let prev_output_tx_location =
                                    TxLocation::new(block_height.0, tx_index as u16);
                                DbV1::build_input_history(
                                    &mut addrhist_inputs_map,
                                    tx_location,
                                    input_index as u16,
                                    input,
                                    prev_output,
                                    prev_output_tx_location,
                                );
                            }
                        }
                    }
                } else if let Ok((prev_output, prev_output_tx_location)) =
                    tokio::task::block_in_place(|| {
                        let prev_output = self.get_previous_output_blocking(prev_outpoint)?;

                        let prev_output_tx_location = self
                            .find_txid_index_blocking(&TransactionHash::from(
                                *prev_outpoint.prev_txid(),
                            ))
                            .map_err(|e| FinalisedStateError::InvalidBlock {
                                height: block.height().expect("already  checked height is some").0,
                                hash: *block.hash(),
                                reason: e.to_string(),
                            })?
                            .ok_or_else(|| FinalisedStateError::InvalidBlock {
                                height: block.height().expect("already  checked height is some").0,
                                hash: *block.hash(),
                                reason: "Invalid block data: invalid txid data.".to_string(),
                            })?;

                        Ok::<(_, _), FinalisedStateError>((prev_output, prev_output_tx_location))
                    })
                {
                    DbV1::build_input_history(
                        &mut addrhist_inputs_map,
                        tx_location,
                        input_index as u16,
                        input,
                        &prev_output,
                        prev_output_tx_location,
                    );
                } else {
                    return Err(FinalisedStateError::InvalidBlock {
                        height: block.height().expect("already  checked height is some").0,
                        hash: *block.hash(),
                        reason: "Invalid block data: invalid transparent input.".to_string(),
                    });
                }
            }
        }

        // Delete all block data from db.
        let zaino_db = Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            commitment_tree_data: self.commitment_tree_data,
            heights: self.heights,
            spent: self.spent,
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: None,
            status: self.status.clone(),
            config: self.config.clone(),
        };
        tokio::task::spawn_blocking(move || {
            // Delete spent data
            let mut txn = zaino_db.env.begin_rw_txn()?;

            for outpoint in &spent_map {
                let outpoint_bytes =
                    &outpoint
                        .to_bytes()
                        .map_err(|_| FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: "Corrupt block data: failed to serialise outpoint".to_string(),
                        })?;
                match txn.del(zaino_db.spent, outpoint_bytes, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            }
            let _ = txn.commit();

            // Delete addrhist input data and mark old outputs spent in this block as unspent
            for (addr_script, records) in &addrhist_inputs_map {
                // Mark outputs spent in this block as unspent
                for (_record, (prev_output_script, prev_output_record)) in records {
                    {
                        let _updated = zaino_db
                            .mark_addr_hist_record_unspent_blocking(
                                prev_output_script,
                                prev_output_record.tx_location(),
                                prev_output_record.out_index(),
                            )
                            // TODO: check internals to propagate important errors.
                            .map_err(|_| FinalisedStateError::InvalidBlock {
                                height: block_height.0,
                                hash: block_hash,
                                reason: "Corrupt block data: failed to mark output unspent"
                                    .to_string(),
                            })?;
                    }
                }

                // Delete all input records created in this block.
                zaino_db
                    .delete_addrhist_dups_blocking(
                        &addr_script
                            .to_bytes()
                            .map_err(|_| FinalisedStateError::InvalidBlock {
                                height: block_height.0,
                                hash: block_hash,
                                reason: "Corrupt block data: failed to serialise addr_script"
                                    .to_string(),
                            })?,
                        block_height,
                        true,
                        false,
                        records.len(),
                    )
                    // TODO: check internals to propagate important errors.
                    .map_err(|_| FinalisedStateError::InvalidBlock {
                        height: block_height.0,
                        hash: block_hash,
                        reason: "Corrupt block data: failed to delete inputs".to_string(),
                    })?;
            }

            // Delete addrhist output data
            for (addr_script, records) in &addrhist_outputs_map {
                zaino_db.delete_addrhist_dups_blocking(
                    &addr_script
                        .to_bytes()
                        .map_err(|_| FinalisedStateError::InvalidBlock {
                            height: block_height.0,
                            hash: block_hash,
                            reason: "Corrupt block data: failed to serialise addr_script"
                                .to_string(),
                        })?,
                    block_height,
                    false,
                    true,
                    records.len(),
                )?;
            }

            // Delete block data
            let mut txn = zaino_db.env.begin_rw_txn()?;

            for &db in &[
                zaino_db.headers,
                zaino_db.txids,
                zaino_db.transparent,
                zaino_db.sapling,
                zaino_db.orchard,
                zaino_db.commitment_tree_data,
            ] {
                match txn.del(db, &block_height_bytes, None) {
                    Ok(()) | Err(lmdb::Error::NotFound) => {}
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            }

            match txn.del(zaino_db.heights, &block_hash_bytes, None) {
                Ok(()) | Err(lmdb::Error::NotFound) => {}
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            }

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

    /// Updates the metadata hed by the database.
    pub(crate) async fn update_metadata(
        &self,
        metadata: DbMetadata,
    ) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

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

    // *** Public fetcher methods - Used by DbReader ***

    /// Returns the greatest `Height` stored in `headers`
    /// (`None` if the DB is still empty).
    pub(crate) async fn tip_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let ro = self.env.begin_ro_txn()?;
            let cur = ro.open_ro_cursor(self.headers)?;

            match cur.get(None, None, lmdb_sys::MDB_LAST) {
                Ok((key_bytes, _val_bytes)) => {
                    // `key_bytes` is exactly what `Height::to_bytes()` produced
                    let h = Height::from_bytes(
                        key_bytes.expect("height is always some in the finalised state"),
                    )
                    .map_err(|e| FinalisedStateError::Custom(format!("height decode: {e}")))?;
                    Ok(Some(h))
                }
                Err(lmdb::Error::NotFound) => Ok(None),
                Err(e) => Err(FinalisedStateError::LmdbError(e)),
            }
        })
    }

    /// Fetch the block height in the main chain for a given block hash.
    async fn get_block_height_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Height, FinalisedStateError> {
        let height = self
            .resolve_validated_hash_or_height(HashOrHeight::Hash(hash.into()))
            .await?;
        Ok(height)
    }

    /// Fetch the height range for the given block hashes.
    async fn get_block_range_by_hash(
        &self,
        start_hash: BlockHash,
        end_hash: BlockHash,
    ) -> Result<(Height, Height), FinalisedStateError> {
        let start_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Hash(start_hash.into()))
            .await?;
        let end_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Hash(end_hash.into()))
            .await?;

        let (validated_start, validated_end) =
            self.validate_block_range(start_height, end_height).await?;

        Ok((validated_start, validated_end))
    }

    // Fetch the TxLocation for the given txid, transaction data is indexed by TxLocation internally.
    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        if let Some(index) = tokio::task::block_in_place(|| self.find_txid_index_blocking(txid))? {
            Ok(Some(index))
        } else {
            Ok(None)
        }
    }

    /// Fetch block header data by height.
    async fn get_block_header_data(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "header data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let entry = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))?;

            Ok(*entry.inner())
        })
    }

    /// Fetches block headers for the given height range.
    ///
    /// Uses cursor based fetch.
    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.headers) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "header data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<BlockHeaderData>::from_bytes(&bytes)
                    .map(|e| *e.inner())
                    .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))
            })
            .collect()
    }

    /// Fetch the txid bytes for a given TxLocation.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    ///
    /// NOTE: This method currently ignores the txid version byte for efficiency.
    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            use std::io::Cursor;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Parse StoredEntryVar<TxidList>:

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            let _body_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Read [1] TxidList Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of txids
            let list_len = CompactSize::read(&mut cursor)
                .map_err(|e| FinalisedStateError::Custom(format!("txid list len error: {e}")))?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in txid list".to_string(),
                ));
            }

            // Each txid entry is: [0] version tag + [1..32] txid

            // So we skip idx * 33 bytes to reach the start of the correct Hash
            let offset = cursor.position() + (idx as u64) * TransactionHash::VERSIONED_LEN as u64;
            cursor.set_position(offset);

            // Read [0] Txid Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Then read 32 bytes for the txid
            let mut txid_bytes = [0u8; TransactionHash::ENCODED_LEN];
            cursor
                .read_exact(&mut txid_bytes)
                .map_err(|e| FinalisedStateError::Custom(format!("txid read error: {e}")))?;

            Ok(TransactionHash::from(txid_bytes))
        })
    }

    /// Fetch block txids by height.
    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<TxidList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block txids for the given height range.
    ///
    /// Uses cursor based fetch.
    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.txids) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "txid data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<TxidList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))
            })
            .collect()
    }

    /// Fetch the serialized TransparentCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "transparent data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            let _body_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Read [1] TransparentTxList Record version (skip 1 byte)
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of records
            let list_len = CompactSize::read(&mut cursor)
                .map_err(|e| FinalisedStateError::Custom(format!("txid list len error: {e}")))?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in transparent tx data".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_transparent_entry(&mut cursor)
                    .map_err(|e| FinalisedStateError::Custom(format!("skip entry error: {e}")))?;
            }

            let start = cursor.position();

            // Peek at the 1-byte presence flag
            let mut presence = [0u8; 1];
            cursor.read_exact(&mut presence).map_err(|e| {
                FinalisedStateError::Custom(format!("failed to read Option tag: {e}"))
            })?;

            if presence[0] == 0 {
                return Ok(None);
            } else if presence[0] != 1 {
                return Err(FinalisedStateError::Custom(format!(
                    "invalid Option tag: {}",
                    presence[0]
                )));
            }

            cursor.set_position(start);
            // Skip this entry to compute length
            Self::skip_opt_transparent_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();
            let slice = &raw[start as usize..end as usize];

            Ok(Some(TransparentCompactTx::from_bytes(slice)?))
        })
    }

    /// Fetch block transparent transaction data by height.
    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "transparent data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<TransparentTxList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| {
                    FinalisedStateError::Custom(format!("transparent decode error: {e}"))
                })?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block transparent tx data for the given height range.
    ///
    /// Uses cursor based fetch.
    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.transparent) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "transparent data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<TransparentTxList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| {
                        FinalisedStateError::Custom(format!("transparent decode error: {e}"))
                    })
            })
            .collect()
    }

    /// Fetch the serialized SaplingCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "sapling data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut cursor = Cursor::new(raw);

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Skip SaplingTxList version byte
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of entries
            let list_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("sapling tx list len error: {e}"))
            })?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in sapling tx list".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_sapling_entry(&mut cursor)
                    .map_err(|e| FinalisedStateError::Custom(format!("skip entry error: {e}")))?;
            }

            let start = cursor.position();

            // Peek presence flag
            let mut presence = [0u8; 1];
            cursor.read_exact(&mut presence).map_err(|e| {
                FinalisedStateError::Custom(format!("failed to read Option tag: {e}"))
            })?;

            if presence[0] == 0 {
                return Ok(None);
            } else if presence[0] != 1 {
                return Err(FinalisedStateError::Custom(format!(
                    "invalid Option tag: {}",
                    presence[0]
                )));
            }

            // Rewind to include tag in returned bytes
            cursor.set_position(start);
            Self::skip_opt_sapling_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();

            Ok(Some(SaplingCompactTx::from_bytes(
                &raw[start as usize..end as usize],
            )?))
        })
    }

    /// Fetch block sapling transaction data by height.
    async fn get_block_sapling(
        &self,
        height: Height,
    ) -> Result<SaplingTxList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "sapling data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<SaplingTxList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block sapling tx data for the given height range.
    ///
    /// Uses cursor based fetch.
    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.sapling) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "sapling data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<SaplingTxList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))
            })
            .collect()
    }

    /// Fetch the serialized OrchardCompactTx for the given TxLocation, if present.
    ///
    /// This uses an optimized lookup without decoding the full TxidList.
    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        use std::io::{Cursor, Read};

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let height = Height::try_from(tx_location.block_height())
                .map_err(|e| FinalisedStateError::Custom(e.to_string()))?;
            let height_bytes = height.to_bytes()?;

            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "orchard data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let mut cursor = Cursor::new(raw);

            // Skip [0] StoredEntry version
            cursor.set_position(1);

            // Read CompactSize: length of serialized body
            CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("compact size read error: {e}"))
            })?;

            // Skip OrchardTxList version byte
            cursor.set_position(cursor.position() + 1);

            // Read CompactSize: number of entries
            let list_len = CompactSize::read(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("orchard tx list len error: {e}"))
            })?;

            let idx = tx_location.tx_index() as usize;
            if idx >= list_len as usize {
                return Err(FinalisedStateError::Custom(
                    "tx_index out of range in orchard tx list".to_string(),
                ));
            }

            // Skip preceding entries
            for _ in 0..idx {
                Self::skip_opt_orchard_entry(&mut cursor)
                    .map_err(|e| FinalisedStateError::Custom(format!("skip entry error: {e}")))?;
            }

            let start = cursor.position();

            // Peek presence flag
            let mut presence = [0u8; 1];
            cursor.read_exact(&mut presence).map_err(|e| {
                FinalisedStateError::Custom(format!("failed to read Option tag: {e}"))
            })?;

            if presence[0] == 0 {
                return Ok(None);
            } else if presence[0] != 1 {
                return Err(FinalisedStateError::Custom(format!(
                    "invalid Option tag: {}",
                    presence[0]
                )));
            }

            // Rewind to include presence flag in output
            cursor.set_position(start);
            Self::skip_opt_orchard_entry(&mut cursor).map_err(|e| {
                FinalisedStateError::Custom(format!("skip entry error (second pass): {e}"))
            })?;

            let end = cursor.position();

            Ok(Some(OrchardCompactTx::from_bytes(
                &raw[start as usize..end as usize],
            )?))
        })
    }

    /// Fetch block orchard transaction data by height.
    async fn get_block_orchard(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "orchard data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry: StoredEntryVar<OrchardTxList> = StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))?;

            Ok(entry.inner().clone())
        })
    }

    /// Fetches block orchard tx data for the given height range.
    ///
    /// Uses cursor based fetch.
    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.orchard) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "orchard data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryVar::<OrchardTxList>::from_bytes(&bytes)
                    .map(|e| e.inner().clone())
                    .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))
            })
            .collect()
    }

    /// Fetch block commitment tree data by height.
    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "commitment tree data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry = StoredEntryFixed::from_bytes(raw).map_err(|e| {
                FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
            })?;

            Ok(entry.item)
        })
    }

    /// Fetches block commitment tree data for the given height range.
    ///
    /// Uses cursor based fetch.
    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
        self.validate_block_range(start, end).await?;
        let start_bytes = start.to_bytes()?;
        let end_bytes = end.to_bytes()?;

        let raw_entries = tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let mut raw_entries = Vec::new();
            let mut cursor = match txn.open_ro_cursor(self.commitment_tree_data) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "commitment tree data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            for (k, v) in cursor.iter_from(&start_bytes[..]) {
                if k > &end_bytes[..] {
                    break;
                }
                raw_entries.push(v.to_vec());
            }
            Ok::<Vec<Vec<u8>>, FinalisedStateError>(raw_entries)
        })?;

        raw_entries
            .into_iter()
            .map(|bytes| {
                StoredEntryFixed::<CommitmentTreeData>::from_bytes(&bytes)
                    .map(|e| e.item)
                    .map_err(|e| {
                        FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
                    })
            })
            .collect()
    }

    /// Fetch the `TxLocation` that spent a given outpoint, if any.
    ///
    /// Returns:
    /// - `Ok(Some(TxLocation))` if the outpoint is spent.
    /// - `Ok(None)` if no entry exists (not spent or not known).
    /// - `Err(...)` on deserialization or DB error.
    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        let key = outpoint.to_bytes()?;
        let txn = self.env.begin_ro_txn()?;

        tokio::task::block_in_place(|| match txn.get(self.spent, &key) {
            Ok(bytes) => {
                let entry = StoredEntryFixed::<TxLocation>::from_bytes(bytes).map_err(|e| {
                    FinalisedStateError::Custom(format!("spent entry decode error: {e}"))
                })?;
                Ok(Some(entry.item))
            }
            Err(lmdb::Error::NotFound) => Ok(None),
            Err(e) => Err(FinalisedStateError::LmdbError(e)),
        })
    }

    /// Fetch the `TxLocation` entries for a batch of outpoints.
    ///
    /// For each input:
    /// - Returns `Some(TxLocation)` if spent,
    /// - `None` if not found,
    /// - or returns `Err` immediately if any DB or decode error occurs.
    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            outpoints
                .into_iter()
                .map(|outpoint| {
                    let key = outpoint.to_bytes()?;
                    match txn.get(self.spent, &key) {
                        Ok(bytes) => {
                            let entry =
                                StoredEntryFixed::<TxLocation>::from_bytes(bytes).map_err(|e| {
                                    FinalisedStateError::Custom(format!(
                                        "spent entry decode error for {outpoint:?}: {e}"
                                    ))
                                })?;
                            Ok(Some(entry.item))
                        }
                        Err(lmdb::Error::NotFound) => Ok(None),
                        Err(e) => Err(FinalisedStateError::LmdbError(e)),
                    }
                })
                .collect()
        })
    }

    /// Fetch all address history records for a given transparent address.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more valid records exist,
    /// - `Ok(None)` if no records exist (not an error),
    /// - `Err(...)` if any decoding or DB error occurs.
    async fn addr_records(
        &self,
        addr_script: AddrScript,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let mut raw_records = Vec::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN {
                    continue;
                }
                if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                    continue;
                }
                raw_records.push(val.to_vec());
            }

            if raw_records.is_empty() {
                return Ok(None);
            }

            let mut records = Vec::with_capacity(raw_records.len());
            for val in raw_records {
                let entry = StoredEntryFixed::<AddrEventBytes>::from_bytes(&val).map_err(|e| {
                    FinalisedStateError::Custom(format!("addrhist decode error: {e}"))
                })?;
                records.push(entry.item);
            }

            Ok(Some(records))
        })
    }

    /// Fetch all address history records for a given address and TxLocation.
    ///
    /// Returns:
    /// - `Ok(Some(records))` if one or more matching records are found at that index,
    /// - `Ok(None)` if no matching records exist (not an error),
    /// - `Err(...)` on decode or DB failure.
    async fn addr_and_index_records(
        &self,
        addr_script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<AddrEventBytes>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        let rec_results = tokio::task::block_in_place(|| {
            self.addr_hist_records_by_addr_and_index_blocking(&addr_bytes, tx_location)
        });

        let raw_records = match rec_results {
            Ok(records) => records,
            Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => return Ok(None),
            Err(e) => return Err(e),
        };

        if raw_records.is_empty() {
            return Ok(None);
        }

        let mut records = Vec::with_capacity(raw_records.len());

        for val in raw_records {
            let entry = StoredEntryFixed::<AddrEventBytes>::from_bytes(&val)
                .map_err(|e| FinalisedStateError::Custom(format!("addrhist decode error: {e}")))?;
            records.push(entry.item);
        }

        Ok(Some(records))
    }

    /// Fetch all distinct `TxLocation` values for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more matching records are found,
    /// - `Ok(None)` if no matches found (not an error),
    /// - `Err(...)` on decode or DB failure.
    async fn addr_tx_locations_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut set: HashSet<TxLocation> = HashSet::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN
                    || val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                {
                    continue;
                }

                // Parse the tx_location out of val:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let block_height = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
                if block_height < start_height.0 || block_height > end_height.0 {
                    continue;
                }

                let tx_index = u16::from_be_bytes([val[6], val[7]]);
                set.insert(TxLocation::new(block_height, tx_index));
            }
            let mut indices: Vec<_> = set.into_iter().collect();
            indices.sort_by_key(|txi| (txi.block_height(), txi.tx_index()));

            if indices.is_empty() {
                Ok(None)
            } else {
                Ok(Some(indices))
            }
        })
    }

    /// Fetch all UTXOs (unspent mined outputs) for `addr_script` within the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Each entry is `(TxLocation, vout, value)`.
    ///
    /// Returns:
    /// - `Ok(Some(vec))` if one or more UTXOs are found,
    /// - `Ok(None)` if none found (not an error),
    /// - `Err(...)` on decode or DB failure.
    async fn addr_utxos_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let mut utxos = Vec::new();

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => return Ok(None),
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN
                    || val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                {
                    continue;
                }

                // Parse the tx_location out of val:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let block_height = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
                if block_height < start_height.0 || block_height > end_height.0 {
                    continue;
                }

                let flags = val[10];
                if (flags & AddrEventBytes::FLAG_MINED == 0)
                    || (flags & AddrEventBytes::FLAG_SPENT != 0)
                {
                    continue;
                }

                let tx_index = u16::from_be_bytes([val[6], val[7]]);
                let vout = u16::from_be_bytes([val[8], val[9]]);
                let value = u64::from_le_bytes([
                    val[11], val[12], val[13], val[14], val[15], val[16], val[17], val[18],
                ]);

                utxos.push((TxLocation::new(block_height, tx_index), vout, value));
            }

            if utxos.is_empty() {
                Ok(None)
            } else {
                Ok(Some(utxos))
            }
        })
    }

    /// Computes the transparent balance change for `addr_script` over the
    /// height range `[start_height, end_height]` (inclusive).
    ///
    /// Includes:
    /// - `+value` for mined outputs
    /// - `−value` for spent inputs
    ///
    /// Returns the signed net value as `i64`, or error on failure.
    async fn addr_balance_by_range(
        &self,
        addr_script: AddrScript,
        start_height: Height,
        end_height: Height,
    ) -> Result<i64, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            let mut cursor = match txn.open_ro_cursor(self.address_history) {
                Ok(cursor) => cursor,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "no data for address".to_string(),
                    ))
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let mut balance: i64 = 0;

            let iter = match cursor.iter_dup_of(&addr_bytes) {
                Ok(iter) => iter,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "no data for address".to_string(),
                    ))
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            for (key, val) in iter {
                if key.len() != AddrScript::VERSIONED_LEN
                    || val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                {
                    continue;
                }

                // Parse the tx_location out of val:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let height = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
                if height < start_height.0 || height > end_height.0 {
                    continue;
                }

                let flags = val[10];
                let value = u64::from_le_bytes([
                    val[11], val[12], val[13], val[14], val[15], val[16], val[17], val[18],
                ]) as i64;

                if flags & AddrEventBytes::FLAG_IS_INPUT != 0 {
                    balance -= value;
                } else if flags & AddrEventBytes::FLAG_MINED != 0 {
                    balance += value;
                }
            }

            Ok(balance)
        })
    }

    /// Returns the IndexedBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        let validated_height = match self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await
        {
            Ok(height) => height,
            Err(FinalisedStateError::DataUnavailable(_)) => return Ok(None),
            Err(other) => return Err(other),
        };
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            // Fetch header data
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let header: BlockHeaderData = *StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))?
                .inner();

            // fetch transaction data
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let txids_list = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))?
                .inner()
                .clone();
            let txids = txids_list.txids();

            let raw = match txn.get(self.transparent, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let transparent_list = StoredEntryVar::<TransparentTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("transparent decode error: {e}")))?
                .inner()
                .clone();
            let transparent = transparent_list.tx();

            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let sapling_list = StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))?
                .inner()
                .clone();
            let sapling = sapling_list.tx();

            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let orchard_list = StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))?
                .inner()
                .clone();
            let orchard = orchard_list.tx();

            // Build CompactTxData
            let len = txids.len();
            if transparent.len() != len || sapling.len() != len || orchard.len() != len {
                return Err(FinalisedStateError::Custom(
                    "mismatched tx list lengths in block data".to_string(),
                ));
            }

            let txs: Vec<CompactTxData> = (0..len)
                .map(|i| {
                    let txid = txids[i];
                    let transparent_tx = transparent[i]
                        .clone()
                        .unwrap_or_else(|| TransparentCompactTx::new(vec![], vec![]));
                    let sapling_tx = sapling[i]
                        .clone()
                        .unwrap_or_else(|| SaplingCompactTx::new(None, vec![], vec![]));
                    let orchard_tx = orchard[i]
                        .clone()
                        .unwrap_or_else(|| OrchardCompactTx::new(None, vec![]));

                    CompactTxData::new(i as u64, txid, transparent_tx, sapling_tx, orchard_tx)
                })
                .collect();

            // fetch commitment tree data
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let commitment_tree_data: CommitmentTreeData = *StoredEntryFixed::from_bytes(raw)
                .map_err(|e| {
                    FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
                })?
                .inner();

            // Construct IndexedBlock
            Ok(Some(IndexedBlock::new(
                *header.index(),
                *header.data(),
                txs,
                commitment_tree_data,
            )))
        })
    }

    /// Returns the CompactBlock for the given Height.
    ///
    /// TODO: Add separate range fetch method!
    async fn get_compact_block(
        &self,
        height: Height,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        let validated_height = self
            .resolve_validated_hash_or_height(HashOrHeight::Height(height.into()))
            .await?;
        let height_bytes = validated_height.to_bytes()?;

        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;

            // Fetch header data
            let raw = match txn.get(self.headers, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let header: BlockHeaderData = *StoredEntryVar::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("header decode error: {e}")))?
                .inner();

            // fetch transaction data
            let raw = match txn.get(self.txids, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let txids_list = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("txids decode error: {e}")))?
                .inner()
                .clone();
            let txids = txids_list.txids();

            let raw = match txn.get(self.sapling, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let sapling_list = StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("sapling decode error: {e}")))?
                .inner()
                .clone();
            let sapling = sapling_list.tx();

            let raw = match txn.get(self.orchard, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };
            let orchard_list = StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("orchard decode error: {e}")))?
                .inner()
                .clone();
            let orchard = orchard_list.tx();

            let vtx: Vec<zaino_proto::proto::compact_formats::CompactTx> = txids
                .iter()
                .enumerate()
                .filter_map(|(i, txid)| {
                    let spends = sapling
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|s| {
                            s.spends()
                                .iter()
                                .map(|sp| sp.into_compact())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    let outputs = sapling
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|s| {
                            s.outputs()
                                .iter()
                                .map(|o| o.into_compact())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    let actions = orchard
                        .get(i)
                        .and_then(|opt| opt.as_ref())
                        .map(|o| {
                            o.actions()
                                .iter()
                                .map(|a| a.into_compact())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    // SKIP transparent-only txs:
                    if spends.is_empty() && outputs.is_empty() && actions.is_empty() {
                        return None;
                    }

                    Some(zaino_proto::proto::compact_formats::CompactTx {
                        index: i as u64,
                        hash: txid.0.to_vec(),
                        fee: 0,
                        spends,
                        outputs,
                        actions,
                    })
                })
                .collect();

            // fetch commitment tree data
            let raw = match txn.get(self.commitment_tree_data, &height_bytes) {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let commitment_tree_data: CommitmentTreeData = *StoredEntryFixed::from_bytes(raw)
                .map_err(|e| {
                    FinalisedStateError::Custom(format!("commitment_tree decode error: {e}"))
                })?
                .inner();

            let chain_metadata = zaino_proto::proto::compact_formats::ChainMetadata {
                sapling_commitment_tree_size: commitment_tree_data.sizes().sapling(),
                orchard_commitment_tree_size: commitment_tree_data.sizes().orchard(),
            };

            // Construct CompactBlock
            Ok(zaino_proto::proto::compact_formats::CompactBlock {
                proto_version: 4,
                height: header
                    .index()
                    .height()
                    .expect("height always present in finalised state.")
                    .0 as u64,
                hash: header.index().hash().0.to_vec(),
                prev_hash: header.index().parent_hash().0.to_vec(),
                // Is this safe?
                time: header.data().time() as u32,
                header: Vec::new(),
                vtx,
                chain_metadata: Some(chain_metadata),
            })
        })
    }

    /// Fetch database metadata.
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let txn = self.env.begin_ro_txn()?;
            let raw = match txn.get(self.metadata, b"metadata") {
                Ok(val) => val,
                Err(lmdb::Error::NotFound) => {
                    return Err(FinalisedStateError::DataUnavailable(
                        "block data missing from db".into(),
                    ));
                }
                Err(e) => return Err(FinalisedStateError::LmdbError(e)),
            };

            let entry = StoredEntryFixed::from_bytes(raw)
                .map_err(|e| FinalisedStateError::Custom(format!("metadata decode error: {e}")))?;

            Ok(entry.item)
        })
    }

    // *** Internal DB validation / varification ***

    /// Return `true` if *height* is already known-good.
    ///
    /// O(1) look-ups: we check the tip first (fast) and only hit the DashSet
    /// when `h > tip`.
    fn is_validated(&self, h: u32) -> bool {
        let tip = self.validated_tip.load(Ordering::Acquire);
        h <= tip || self.validated_set.contains(&h)
    }

    /// Mark *height* as validated and coalesce contiguous ranges.
    ///
    /// 1. Insert it into the DashSet (if it was a “hole”).
    /// 2. While `validated_tip + 1` is now present, pop it and advance the tip.
    fn mark_validated(&self, h: u32) {
        let mut next = h;
        loop {
            let tip = self.validated_tip.load(Ordering::Acquire);

            // Fast-path: extend the tip directly?
            if next == tip + 1 {
                // Try to claim the new tip.
                if self
                    .validated_tip
                    .compare_exchange(tip, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    // Successfully advanced; now look for further consecutive heights
                    // already in the DashSet.
                    next += 1;
                    while self.validated_set.remove(&next).is_some() {
                        self.validated_tip.store(next, Ordering::Release);
                        next += 1;
                    }
                    break;
                }
                // CAS failed: someone else updated the tip – retry loop.
            } else if next > tip {
                // Out-of-order hole: just remember it and exit.
                self.validated_set.insert(next);
                break;
            } else {
                // Already below tip – nothing to do.
                break;
            }
        }
    }

    /// Lightweight per-block validation.
    ///
    /// *Confirms the checksum* in each of the three per-block tables.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn validate_block_blocking(
        &self,
        height: Height,
        hash: BlockHash,
    ) -> Result<(), FinalisedStateError> {
        if self.is_validated(height.into()) {
            return Ok(());
        }

        let height_key = height
            .to_bytes()
            .map_err(|e| FinalisedStateError::Custom(format!("height serialize: {e}")))?;
        let hash_key = hash
            .to_bytes()
            .map_err(|e| FinalisedStateError::Custom(format!("hash serialize: {e}")))?;

        // Helper to fabricate the error.
        let fail = |reason: &str| FinalisedStateError::InvalidBlock {
            height: height.into(),
            hash,
            reason: reason.to_owned(),
        };

        let ro = self.env.begin_ro_txn()?;

        // *** header ***
        let header_entry = {
            let raw = ro
                .get(self.headers, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<BlockHeaderData>::from_bytes(raw)
                .map_err(|e| fail(&format!("header corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("header checksum mismatch"));
            }
            entry
        };

        // *** txids ***
        let txid_list_entry = {
            let raw = ro
                .get(self.txids, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<TxidList>::from_bytes(raw)
                .map_err(|e| fail(&format!("txids corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("txids checksum mismatch"));
            }
            entry
        };

        // *** transparent ***
        let transparent_tx_list = {
            let raw = ro.get(self.transparent, &height_key)?;
            let entry = StoredEntryVar::<TransparentTxList>::from_bytes(raw)
                .map_err(|e| fail(&format!("transparent corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("transparent checksum mismatch"));
            }
            entry
        };

        // *** sapling ***
        {
            let raw = ro
                .get(self.sapling, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<SaplingTxList>::from_bytes(raw)
                .map_err(|e| fail(&format!("sapling corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("sapling checksum mismatch"));
            }
        }

        // *** orchard ***
        {
            let raw = ro
                .get(self.orchard, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryVar::<OrchardTxList>::from_bytes(raw)
                .map_err(|e| fail(&format!("orchard corrupt data: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("orchard checksum mismatch"));
            }
        }

        // *** commitment_tree_data (fixed) ***
        {
            let raw = ro
                .get(self.commitment_tree_data, &height_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryFixed::<CommitmentTreeData>::from_bytes(raw)
                .map_err(|e| fail(&format!("commitment_tree corrupt bytes: {e}")))?;
            if !entry.verify(&height_key) {
                return Err(fail("commitment_tree checksum mismatch"));
            }
        }

        // *** hash→height mapping ***
        {
            let raw = ro
                .get(self.heights, &hash_key)
                .map_err(FinalisedStateError::LmdbError)?;
            let entry = StoredEntryFixed::<Height>::from_bytes(raw)
                .map_err(|e| fail(&format!("hash -> height corrupt bytes: {e}")))?;
            if !entry.verify(&hash_key) {
                return Err(fail("hash -> height checksum mismatch"));
            }
            if entry.item != height {
                return Err(fail("hash -> height mapping mismatch"));
            }
        }

        // *** Parent block hash validation (chain continuity) ***
        if height.0 > 1 {
            let parent_block_hash = {
                let parent_block_height = Height::try_from(height.0.saturating_sub(1))
                    .map_err(|e| fail(&format!("invalid parent height: {e}")))?;
                let parent_block_height_key = parent_block_height
                    .to_bytes()
                    .map_err(|e| fail(&format!("parent height serialize: {e}")))?;
                let raw = ro
                    .get(self.headers, &parent_block_height_key)
                    .map_err(FinalisedStateError::LmdbError)?;
                let entry = StoredEntryVar::<BlockHeaderData>::from_bytes(raw)
                    .map_err(|e| fail(&format!("parent header corrupt data: {e}")))?;

                *entry.inner().index().hash()
            };

            let check_hash = header_entry.inner().index().parent_hash();

            if &parent_block_hash != check_hash {
                return Err(fail("parent hash mismatch"));
            }
        }

        // *** Merkle root / Txid validation ***
        let txids: Vec<[u8; 32]> = txid_list_entry
            .inner()
            .txids()
            .iter()
            .map(|h| h.0)
            .collect();

        let header_merkle_root = header_entry.inner().data().merkle_root();

        let check_root = Self::calculate_block_merkle_root(&txids);

        if &check_root != header_merkle_root {
            return Err(fail("merkle root mismatch"));
        }

        // *** spent + addrhist validation ***
        let tx_list = transparent_tx_list.inner().tx();

        for (tx_index, tx_opt) in tx_list.iter().enumerate() {
            let tx_index = tx_index as u16;
            let txid_index = TxLocation::new(height.0, tx_index);

            let Some(tx) = tx_opt else { continue };

            // Outputs: check addrhist mined record
            for (vout, output) in tx.outputs().iter().enumerate() {
                let addr_bytes =
                    AddrScript::new(*output.script_hash(), output.script_type()).to_bytes()?;
                let rec_bytes =
                    self.addr_hist_records_by_addr_and_index_blocking(&addr_bytes, txid_index)?;

                let matched = rec_bytes.iter().any(|val| {
                    // avoid deserialization: check IS_MINED + correct vout
                    // - [0] StoredEntry tag
                    // - [1] record tag
                    // - [2..=5] height
                    // - [6..=7] tx_index
                    // - [8..=9] vout
                    // - [10] flags
                    // - [11..=18] value
                    // - [19..=50] checksum

                    let flags = val[10];
                    let vout_rec = u16::from_be_bytes([val[8], val[9]]);
                    (flags & AddrEventBytes::FLAG_MINED) != 0 && vout_rec as usize == vout
                });

                if !matched {
                    return Err(fail("missing addrhist mined output record"));
                }
            }

            // Inputs: check spent + addrhist input record
            for input in tx.inputs() {
                // Continue if coinbase.
                if input.is_null_prevout() {
                    continue;
                }

                // Check spent record
                let outpoint = Outpoint::new(*input.prevout_txid(), input.prevout_index());
                let outpoint_bytes = outpoint.to_bytes()?;
                let val = ro
                    .get(self.spent, &outpoint_bytes)
                    .map_err(|_| fail(&format!("missing spent index for outpoint {outpoint:?}")))?;
                let entry = StoredEntryFixed::<TxLocation>::from_bytes(val)
                    .map_err(|e| fail(&format!("corrupt spent entry: {e}")))?;
                if !entry.verify(&outpoint_bytes) {
                    return Err(fail("spent entry checksum mismatch"));
                }
                if entry.inner() != &txid_index {
                    return Err(fail("spent entry has wrong TxLocation"));
                }

                // Check addrhist input record
                let prev_output = self.get_previous_output_blocking(outpoint)?;
                let addr_bytes =
                    AddrScript::new(*prev_output.script_hash(), prev_output.script_type())
                        .to_bytes()?;
                let rec_bytes =
                    self.addr_hist_records_by_addr_and_index_blocking(&addr_bytes, txid_index)?;

                let matched = rec_bytes.iter().any(|val| {
                    // avoid deserialization: check IS_INPUT + correct vout
                    // - [0] StoredEntry tag
                    // - [1] record tag
                    // - [2..=5] height
                    // - [6..=7] tx_index
                    // - [8..=9] vout
                    // - [10] flags
                    // - [11..=18] value
                    // - [19..=50] checksum

                    let flags = val[10];
                    let vout = u16::from_be_bytes([val[8], val[9]]);
                    (flags & AddrEventBytes::FLAG_IS_INPUT) != 0
                        && vout == input.prevout_index() as u16
                });

                if !matched {
                    return Err(fail("missing addrhist input record"));
                }
            }
        }

        self.mark_validated(height.into());
        Ok(())
    }

    /// Double‑SHA‑256 (SHA256d) as used by Bitcoin/Zcash headers and Merkle nodes.
    fn sha256d(data: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        Digest::update(&mut hasher, data); // first pass
        let first = hasher.finalize_reset();
        Digest::update(&mut hasher, first); // second pass
        let second = hasher.finalize();

        let mut out = [0u8; 32];
        out.copy_from_slice(&second);
        out
    }

    /// Compute the Merkle root of a non‑empty slice of 32‑byte transaction IDs.
    /// `txids` must be in block order and already in internal (little‑endian) byte order.
    fn calculate_block_merkle_root(txids: &[[u8; 32]]) -> [u8; 32] {
        assert!(
            !txids.is_empty(),
            "block must contain at least the coinbase"
        );
        let mut layer: Vec<[u8; 32]> = txids.to_vec();

        // Iterate until we have reduced to one hash.
        while layer.len() > 1 {
            let mut next = Vec::with_capacity(layer.len().div_ceil(2));

            // Combine pairs (duplicate the last when the count is odd).
            for chunk in layer.chunks(2) {
                let left = &chunk[0];
                let right = if chunk.len() == 2 {
                    &chunk[1]
                } else {
                    &chunk[0]
                };

                // Concatenate left‖right and hash twice.
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(left);
                buf[32..].copy_from_slice(right);
                next.push(Self::sha256d(&buf));
            }

            layer = next;
        }

        layer[0]
    }

    /// Validate a contiguous range of block heights `[start, end]` inclusive.
    ///
    /// Optimized to skip blocks already known to be validated.
    /// Returns the full requested `(start, end)` range on success.
    async fn validate_block_range(
        &self,
        start: Height,
        end: Height,
    ) -> Result<(Height, Height), FinalisedStateError> {
        if end.0 < start.0 {
            return Err(FinalisedStateError::Custom(
                "invalid block range: end < start".to_string(),
            ));
        }

        let tip = self.validated_tip.load(Ordering::Acquire);
        let mut h = std::cmp::max(start.0, tip);

        if h > end.0 {
            return Ok((start, end));
        }

        tokio::task::block_in_place(|| {
            while h <= end.0 {
                if self.is_validated(h) {
                    h += 1;
                    continue;
                }

                let height = Height(h);
                let height_bytes = height.to_bytes()?;
                let bytes = {
                    let ro = self.env.begin_ro_txn()?;
                    let bytes = ro.get(self.headers, &height_bytes).map_err(|e| {
                        if e == lmdb::Error::NotFound {
                            FinalisedStateError::Custom("height not found in best chain".into())
                        } else {
                            FinalisedStateError::LmdbError(e)
                        }
                    })?;
                    bytes.to_vec()
                };

                let hash = *StoredEntryVar::<BlockHeaderData>::deserialize(&*bytes)?
                    .inner()
                    .index()
                    .hash();

                match self.validate_block_blocking(height, hash) {
                    Ok(()) => {}
                    Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                        return Err(FinalisedStateError::DataUnavailable(
                            "block data unavailable".into(),
                        ));
                    }
                    Err(e) => return Err(e),
                }

                h += 1;
            }
            Ok::<_, FinalisedStateError>((start, end))
        })
    }

    /// Same as `resolve_hash_or_height`, **but guarantees the block is validated**.
    ///
    /// * If the block hasn’t been validated yet we do it on-demand
    /// * On success the block hright is returned; on any failure you get a
    ///   `FinalisedStateError`
    ///
    /// TODO: Remove HashOrHeight?
    async fn resolve_validated_hash_or_height(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<Height, FinalisedStateError> {
        let height = match hash_or_height {
            // Height lookup path.
            HashOrHeight::Height(z_height) => {
                let height = Height::try_from(z_height.0)
                    .map_err(|_| FinalisedStateError::Custom("height out of range".into()))?;

                // Check if height is below validated tip,
                // this avoids hash lookups for height based fetch under the valdated tip.
                if height.0 <= self.validated_tip.load(Ordering::Acquire) {
                    return Ok(height);
                }

                let hkey = height.to_bytes()?;

                tokio::task::block_in_place(|| {
                    let ro = self.env.begin_ro_txn()?;
                    let bytes = ro.get(self.headers, &hkey).map_err(|e| {
                        if e == lmdb::Error::NotFound {
                            FinalisedStateError::DataUnavailable(
                                "height not found in best chain".into(),
                            )
                        } else {
                            FinalisedStateError::LmdbError(e)
                        }
                    })?;

                    let hash = *StoredEntryVar::<BlockHeaderData>::deserialize(bytes)?
                        .inner()
                        .index()
                        .hash();

                    match self.validate_block_blocking(height, hash) {
                        Ok(()) => {}
                        Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                            return Err(FinalisedStateError::DataUnavailable(
                                "block data unavailable".into(),
                            ));
                        }
                        Err(e) => return Err(e),
                    }

                    Ok::<BlockHash, FinalisedStateError>(hash)
                })?;
                height
            }

            // Hash lookup path.
            HashOrHeight::Hash(z_hash) => {
                let height = self.resolve_hash_or_height(hash_or_height).await?;
                let hash = BlockHash::from(z_hash);
                tokio::task::block_in_place(|| {
                    match self.validate_block_blocking(height, hash) {
                        Ok(()) => {}
                        Err(FinalisedStateError::LmdbError(lmdb::Error::NotFound)) => {
                            return Err(FinalisedStateError::DataUnavailable(
                                "block data unavailable".into(),
                            ));
                        }
                        Err(e) => return Err(e),
                    }

                    Ok::<Height, FinalisedStateError>(height)
                })?;
                height
            }
        };

        Ok(height)
    }

    /// Resolve a `HashOrHeight` to the block height stored on disk.
    ///
    /// * Height  ->  returned unchanged (zero cost).
    /// * Hash ->  lookup in `hashes` db.
    ///
    /// TODO: Remove HashOrHeight?
    async fn resolve_hash_or_height(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<Height, FinalisedStateError> {
        match hash_or_height {
            // Fast path: we already have the hash.
            HashOrHeight::Height(z_height) => Ok(Height::try_from(z_height.0)
                .map_err(|_| FinalisedStateError::DataUnavailable("height out of range".into()))?),

            // Height lookup path.
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

    // *** Internal DB methods ***

    /// Skips one `Option<TransparentCompactTx>` entry from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<TransparentTxList>`
    ///
    /// Advances the cursor past either:
    /// - 1 byte (`0x00`) if `None`, or
    /// - 1 + 1 + vin_size + vout_size if `Some(TransparentCompactTx)`
    ///   (presence + version + variable vin/vout sections)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_transparent_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        let _start_pos = cursor.position();

        // Read 1-byte presence flag
        let mut presence = [0u8; 1];
        cursor.read_exact(&mut presence)?;

        if presence[0] == 0 {
            return Ok(());
        } else if presence[0] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option tag: {}", presence[0]),
            ));
        }

        // Read version (1 byte)
        cursor.read_exact(&mut [0u8; 1])?;

        // Read vin_len (CompactSize)
        let vin_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip vin entries: each is 1-byte version + 36-byte body
        let vin_skip = vin_len * TxInCompact::VERSIONED_LEN;
        cursor.set_position(cursor.position() + vin_skip as u64);

        // Read vout_len (CompactSize)
        let vout_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip vout entries: each is 1-byte version + 29-byte body
        let vout_skip = vout_len * TxOutCompact::VERSIONED_LEN;
        cursor.set_position(cursor.position() + vout_skip as u64);

        Ok(())
    }

    /// Skips one `Option<SaplingCompactTx>` from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<SaplingTxList>`
    ///
    /// Advances past:
    /// - 1 byte `0x00` if None, or
    /// - 1 + 1 + value + spends + outputs if Some (presence + version + body)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_sapling_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        // Read presence byte
        let mut presence = [0u8; 1];
        cursor.read_exact(&mut presence)?;

        if presence[0] == 0 {
            return Ok(());
        } else if presence[0] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option tag: {}", presence[0]),
            ));
        }

        // Read version
        cursor.read_exact(&mut [0u8; 1])?;

        // Read value: Option<i64>
        let mut value_tag = [0u8; 1];
        cursor.read_exact(&mut value_tag)?;
        if value_tag[0] == 1 {
            // Some(i64): read 8 bytes
            cursor.set_position(cursor.position() + 8);
        } else if value_tag[0] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option<i64> tag: {}", value_tag[0]),
            ));
        }

        // Read number of spends (CompactSize)
        let spend_len = CompactSize::read(&mut *cursor)? as usize;
        let spend_skip = spend_len * CompactSaplingSpend::VERSIONED_LEN;
        cursor.set_position(cursor.position() + spend_skip as u64);

        // Read number of outputs (CompactSize)
        let output_len = CompactSize::read(&mut *cursor)? as usize;
        let output_skip = output_len * CompactSaplingOutput::VERSIONED_LEN;
        cursor.set_position(cursor.position() + output_skip as u64);

        Ok(())
    }

    /// Skips one `Option<OrchardCompactTx>` from the current cursor position.
    ///
    /// The input should be a cursor over just the inner item "list" bytes of a:
    /// - `StoredEntryVar<OrchardTxList>`
    ///
    /// Advances past:
    /// - 1 byte `0x00` if None, or
    /// - 1 + 1 + value + actions if Some (presence + version + body)
    ///
    /// This is faster than deserialising the whole struct as we only read the compact sizes.
    #[inline]
    fn skip_opt_orchard_entry(cursor: &mut std::io::Cursor<&[u8]>) -> io::Result<()> {
        // Read presence byte
        let mut presence = [0u8; 1];
        cursor.read_exact(&mut presence)?;

        if presence[0] == 0 {
            return Ok(());
        } else if presence[0] != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option tag: {}", presence[0]),
            ));
        }

        // Read version
        cursor.read_exact(&mut [0u8; 1])?;

        // Read value: Option<i64>
        let mut value_tag = [0u8; 1];
        cursor.read_exact(&mut value_tag)?;
        if value_tag[0] == 1 {
            // Some(i64): read 8 bytes
            cursor.set_position(cursor.position() + 8);
        } else if value_tag[0] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Option<i64> tag: {}", value_tag[0]),
            ));
        }

        // Read number of actions (CompactSize)
        let action_len = CompactSize::read(&mut *cursor)? as usize;

        // Skip actions: each is 1-byte version + 148-byte body
        let action_skip = action_len * CompactOrchardAction::VERSIONED_LEN;
        cursor.set_position(cursor.position() + action_skip as u64);

        Ok(())
    }

    /// Returns all raw AddrHist records for a given AddrScript and TxLocation.
    ///
    /// Returns a Vec of serialized entries, for given addr_script and ix_index.
    ///
    /// Efficiently filters by matching block + tx index bytes in-place.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn addr_hist_records_by_addr_and_index_blocking(
        &self,
        addr_script_bytes: &Vec<u8>,
        tx_location: TxLocation,
    ) -> Result<Vec<Vec<u8>>, FinalisedStateError> {
        let txn = self.env.begin_ro_txn()?;

        let mut cursor = txn.open_ro_cursor(self.address_history)?;
        let mut results = Vec::new();

        for (key, val) in cursor.iter_dup_of(&addr_script_bytes)? {
            if key.len() != AddrScript::VERSIONED_LEN {
                // TODO: Return error?
                break;
            }
            if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                // TODO: Return error?
                break;
            }

            // Check tx_location match without deserializing
            // - [0] StoredEntry tag
            // - [1] record tag
            // - [2..=5] height
            // - [6..=7] tx_index
            // - [8..=9] vout
            // - [10] flags
            // - [11..=18] value
            // - [19..=50] checksum

            let block_index = u32::from_be_bytes([val[2], val[3], val[4], val[5]]);
            let tx_idx = u16::from_be_bytes([val[6], val[7]]);

            if block_index == tx_location.block_height() && tx_idx == tx_location.tx_index() {
                results.push(val.to_vec());
            }
        }

        Ok(results)
    }

    /// Inserts a mined-output record into the address‐history map.
    #[inline]
    fn build_transaction_output_histories<'a>(
        map: &mut HashMap<AddrScript, Vec<AddrHistRecord>>,
        tx_location: TxLocation,
        outputs: impl Iterator<Item = (usize, &'a TxOutCompact)>,
    ) {
        for (output_idx, output) in outputs {
            let addr_script = AddrScript::new(*output.script_hash(), output.script_type());
            let output_record = AddrHistRecord::new(
                tx_location,
                output_idx as u16,
                output.value(),
                AddrHistRecord::FLAG_MINED,
            );
            map.entry(addr_script)
                .and_modify(|v| v.push(output_record))
                .or_insert_with(|| vec![output_record]);
        }
    }

    /// Inserts both the “spend” record and the “mined” previous‐output record
    /// (used to update the output record spent in this transaction).
    #[inline]
    #[allow(clippy::type_complexity)]
    fn build_input_history(
        map: &mut HashMap<AddrScript, Vec<(AddrHistRecord, (AddrScript, AddrHistRecord))>>,
        input_tx_location: TxLocation,
        input_index: u16,
        input: &TxInCompact,
        prev_output: &TxOutCompact,
        prev_output_tx_location: TxLocation,
    ) {
        let addr_script = AddrScript::new(*prev_output.script_hash(), prev_output.script_type());
        let input_record = AddrHistRecord::new(
            input_tx_location,
            input_index,
            prev_output.value(),
            AddrHistRecord::FLAG_IS_INPUT,
        );
        let prev_output_record = (
            AddrScript::new(*prev_output.script_hash(), prev_output.script_type()),
            AddrHistRecord::new(
                prev_output_tx_location,
                input.prevout_index() as u16,
                prev_output.value(),
                AddrHistRecord::FLAG_MINED,
            ),
        );
        map.entry(addr_script)
            .and_modify(|v| v.push((input_record, prev_output_record)))
            .or_insert_with(|| vec![(input_record, prev_output_record)]);
    }

    /// Delete all `addrhist` duplicates for `addr_bytes` that
    ///   * belong to `block_height`, **and**
    ///   * match the requested record type(s).
    ///
    /// * `delete_inputs`  – remove records whose flag-byte contains FLAG_IS_INPUT
    /// * `delete_outputs` – remove records whose flag-byte contains FLAG_MINED
    ///
    /// `expected` is the number of records to delete;
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn delete_addrhist_dups_blocking(
        &self,
        addr_bytes: &[u8],
        block_height: Height,
        delete_inputs: bool,
        delete_outputs: bool,
        expected: usize,
    ) -> Result<(), FinalisedStateError> {
        if !delete_inputs && !delete_outputs {
            return Err(FinalisedStateError::Custom(
                "called delete_addrhist_dups with neither inputs nor outputs to delete".into(),
            ));
        }
        if expected == 0 {
            return Err(FinalisedStateError::Custom(
                "called delete_addrhist_dups with 0 expected deletes".into(),
            ));
        }

        let mut remaining = expected;
        let height_be = block_height.0.to_be_bytes();

        let mut txn = self.env.begin_rw_txn()?;
        let mut cur = txn.open_rw_cursor(self.address_history)?;

        match cur
            .get(Some(addr_bytes), None, lmdb_sys::MDB_SET_KEY)
            .and_then(|_| cur.get(None, None, lmdb_sys::MDB_LAST_DUP))
        {
            Ok((_k, mut val)) => loop {
                // Parse AddrEventBytes:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum
                if val.len() == StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN
                    && val[2..6] == height_be
                {
                    let flags = val[10];
                    let is_input = flags & AddrEventBytes::FLAG_IS_INPUT != 0;
                    let is_output = flags & AddrEventBytes::FLAG_MINED != 0;

                    if (delete_inputs && is_input) || (delete_outputs && is_output) {
                        cur.del(WriteFlags::empty())?;
                        remaining -= 1;
                        if remaining == 0 {
                            break;
                        }
                    }
                } else if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                    tracing::warn!("bad addrhist dup (len={})", val.len());
                }

                // step backwards through duplicates
                match cur.get(None, None, lmdb_sys::MDB_PREV_DUP) {
                    Ok((_k, v)) => val = v,
                    Err(lmdb::Error::NotFound) => {
                        if remaining == 0 {
                            break;
                        }
                        return Err(FinalisedStateError::Custom(format!(
                            "expected {expected} records, deleted {}",
                            expected - remaining
                        )));
                    }
                    Err(e) => return Err(FinalisedStateError::LmdbError(e)),
                }
            },
            Err(lmdb::Error::NotFound) => {
                return Err(FinalisedStateError::Custom(
                    "no addrhist record for key".into(),
                ));
            }
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        }

        drop(cur);
        txn.commit()?;
        Ok(())
    }

    /// Mark a specific AddrHistRecord as spent in the addrhist DB.
    /// Looks up a record by script and tx_location, sets FLAG_SPENT, and updates it in place.
    ///
    /// Returns Ok(true) if a record was updated, Ok(false) if not found, or Err on DB error.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn mark_addr_hist_record_spent_blocking(
        &self,
        addr_script: &AddrScript,
        tx_location: TxLocation,
        vout: u16,
    ) -> Result<bool, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;
        let mut txn = self.env.begin_rw_txn()?;
        {
            let mut cur = txn.open_rw_cursor(self.address_history)?;

            for (key, val) in cur.iter_dup_of(&addr_bytes)? {
                if key.len() != AddrScript::VERSIONED_LEN {
                    // TODO: Return error?
                    break;
                }
                if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                    // TODO: Return error?
                    break;
                }
                let mut hist_record = [0u8; StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN];
                hist_record.copy_from_slice(val);

                // Parse the tx_location out of arr:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let block_height = u32::from_be_bytes([
                    hist_record[2],
                    hist_record[3],
                    hist_record[4],
                    hist_record[5],
                ]);
                let tx_idx = u16::from_be_bytes([hist_record[6], hist_record[7]]);
                let rec_vout = u16::from_be_bytes([hist_record[8], hist_record[9]]);
                let flags = hist_record[10];

                // Skip if this row is an input or already marked spent.
                if flags & AddrHistRecord::FLAG_IS_INPUT != 0
                    || flags & AddrHistRecord::FLAG_SPENT != 0
                {
                    continue;
                }

                // Match on (height, tx_location, vout).
                if block_height == tx_location.block_height()
                    && tx_idx == tx_location.tx_index()
                    && rec_vout == vout
                {
                    // Flip FLAG_SPENT.
                    hist_record[10] |= AddrHistRecord::FLAG_SPENT;

                    // Recompute checksum over entry header + payload (bytes 1‥19).
                    let checksum = StoredEntryFixed::<AddrEventBytes>::blake2b256(
                        &[&addr_bytes, &hist_record[1..19]].concat(),
                    );
                    hist_record[19..51].copy_from_slice(&checksum);

                    // Write back in place.
                    cur.put(&addr_bytes, &hist_record, WriteFlags::CURRENT)?;
                    drop(cur);
                    txn.commit()?;

                    return Ok(true);
                }
            }
        }
        txn.commit()?;
        Ok(false)
    }

    /// Mark a specific AddrHistRecord as unspent in the addrhist DB.
    /// Looks up a record by script and tx_location, sets FLAG_SPENT, and updates it in place.
    ///
    /// Returns Ok(true) if a record was updated, Ok(false) if not found, or Err on DB error.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn mark_addr_hist_record_unspent_blocking(
        &self,
        addr_script: &AddrScript,
        tx_location: TxLocation,
        vout: u16,
    ) -> Result<bool, FinalisedStateError> {
        let addr_bytes = addr_script.to_bytes()?;
        let mut txn = self.env.begin_rw_txn()?;
        {
            let mut cur = txn.open_rw_cursor(self.address_history)?;

            for (key, val) in cur.iter_dup_of(&addr_bytes)? {
                if key.len() != AddrScript::VERSIONED_LEN {
                    // TODO: Return error?
                    break;
                }
                if val.len() != StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN {
                    // TODO: Return error?
                    break;
                }
                let mut hist_record = [0u8; StoredEntryFixed::<AddrEventBytes>::VERSIONED_LEN];
                hist_record.copy_from_slice(val);

                // Parse the tx_location out of arr:
                // - [0] StoredEntry tag
                // - [1] record tag
                // - [2..=5] height
                // - [6..=7] tx_index
                // - [8..=9] vout
                // - [10] flags
                // - [11..=18] value
                // - [19..=50] checksum

                let block_height = u32::from_be_bytes([
                    hist_record[2],
                    hist_record[3],
                    hist_record[4],
                    hist_record[5],
                ]);
                let tx_idx = u16::from_be_bytes([hist_record[6], hist_record[7]]);
                let rec_vout = u16::from_be_bytes([hist_record[8], hist_record[9]]);
                let flags = hist_record[10];

                // Skip if this row is an input or already marked unspent.
                if (flags & AddrHistRecord::FLAG_IS_INPUT) != 0
                    || (flags & AddrHistRecord::FLAG_SPENT) == 0
                {
                    continue;
                }

                // Match on (height, tx_location, vout).
                if block_height == tx_location.block_height()
                    && tx_idx == tx_location.tx_index()
                    && rec_vout == vout
                {
                    // Flip FLAG_SPENT.
                    hist_record[10] &= !AddrHistRecord::FLAG_SPENT;

                    // Recompute checksum over entry header + payload (bytes 1‥19).
                    let checksum = StoredEntryFixed::<AddrEventBytes>::blake2b256(
                        &[&addr_bytes, &hist_record[1..19]].concat(),
                    );
                    hist_record[19..51].copy_from_slice(&checksum);

                    // Write back in place.
                    cur.put(&addr_bytes, &hist_record, WriteFlags::CURRENT)?;
                    drop(cur);
                    txn.commit()?;

                    return Ok(true);
                }
            }
        }
        txn.commit()?;
        Ok(false)
    }

    /// Fetches the previous transparent output for the given outpoint.
    /// Returns `TxOutCompact` or an explicit error if not found or invalid.
    ///
    /// Used to build addrhist records.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn get_previous_output_blocking(
        &self,
        outpoint: Outpoint,
    ) -> Result<TxOutCompact, FinalisedStateError> {
        // Find the tx’s location in the chain
        let prev_txid = TransactionHash::from(*outpoint.prev_txid());
        let tx_location = self
            .find_txid_index_blocking(&prev_txid)?
            .ok_or_else(|| FinalisedStateError::Custom("Previous txid not found".into()))?;

        // Fetch the output from the transparent db.
        let block_height = tx_location.block_height();
        let tx_index = tx_location.tx_index() as usize;
        let out_index = outpoint.prev_index() as usize;

        let ro = self.env.begin_ro_txn()?;
        let height_key = Height(block_height).to_bytes()?;
        let stored_bytes = ro.get(self.transparent, &height_key)?;

        Self::find_txout_in_stored_transparent_tx_list(stored_bytes, tx_index, out_index)
            .ok_or_else(|| {
                FinalisedStateError::Custom("Previous output not found at given index".into())
            })
    }

    /// Finds a TxLocation [block_height, tx_index] from a given txid.
    /// Used for Txid based lookup in transaction DBs.
    ///
    /// WARNING: This is a blocking function and **MUST** be called within a blocking thread / task.
    fn find_txid_index_blocking(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        let ro = self.env.begin_ro_txn()?;
        let mut cursor = ro.open_ro_cursor(self.txids)?;

        let target: [u8; 32] = (*txid).into();

        for (height_bytes, stored_bytes) in cursor.iter() {
            if let Some(tx_index) =
                Self::find_txid_position_in_stored_txid_list(&target, stored_bytes)
            {
                let height = Height::from_bytes(height_bytes)?;
                return Ok(Some(TxLocation::new(height.0, tx_index as u16)));
            }
        }
        Ok(None)
    }

    /// Efficiently scans a raw `StoredEntryVar<TxidList>` buffer to locate the index
    /// of a given transaction ID without full deserialization.
    ///
    /// The format is:
    /// - 1 byte: StoredEntryVar version
    /// - CompactSize: length of the item
    /// - 1 byte: TxidList version
    /// - CompactSize: number of the item
    /// - N x (1 byte + 32 bytes): tagged Hash items
    /// - 32 bytes: checksum
    ///
    /// # Arguments
    /// - `target_txid`: A `[u8; 32]` representing the transaction ID to match.
    /// - `stored`: Raw LMDB byte slice from a `StoredEntryVar<TxidList>`.
    ///
    /// # Returns
    /// - `Some(index)` if a matching txid is found
    /// - `None` if the format is invalid or no match
    #[inline]
    fn find_txid_position_in_stored_txid_list(
        target_txid: &[u8; 32],
        stored: &[u8],
    ) -> Option<usize> {
        const CHECKSUM_LEN: usize = 32;

        // Check is at least sotred version + compactsize + checksum
        // else return none.
        if stored.len() < TransactionHash::VERSION_TAG_LEN + 8 + CHECKSUM_LEN {
            return None;
        }

        let mut cursor = &stored[TransactionHash::VERSION_TAG_LEN..];
        let item_len = CompactSize::read(&mut cursor).ok()? as usize;
        if cursor.len() < item_len + CHECKSUM_LEN {
            return None;
        }

        let (_record_version, mut remaining) = cursor.split_first()?;
        let vec_len = CompactSize::read(&mut remaining).ok()? as usize;

        for idx in 0..vec_len {
            // Each entry is 1-byte tag + 32-byte hash
            let (_tag, rest) = remaining.split_first()?;
            let hash_bytes: &[u8; 32] = rest.get(..32)?.try_into().ok()?;
            if hash_bytes == target_txid {
                return Some(idx);
            }
            remaining = &rest[32..];
        }

        None
    }

    /// Efficiently scans a raw `StoredEntryVar<TransparentTxList>` buffer to locate the
    /// specific output at [tx_idx, output_idx] without full deserialization.
    ///
    /// # Arguments
    /// - `stored`: the raw LMDB byte buffer
    /// - `target_tx_idx`: index in the tx list
    /// - `target_output_idx`: index in the outputs of that tx
    ///
    /// # Returns
    /// - `Some(TxOutCompact)` if found and present, otherwise `None`
    #[inline]
    fn find_txout_in_stored_transparent_tx_list(
        stored: &[u8],
        target_tx_idx: usize,
        target_output_idx: usize,
    ) -> Option<TxOutCompact> {
        const CHECKSUM_LEN: usize = 32;

        if stored.len() < TransactionHash::VERSION_TAG_LEN + 8 + CHECKSUM_LEN {
            return None;
        }

        let mut cursor = &stored[TransactionHash::VERSION_TAG_LEN..];
        let item_len = CompactSize::read(&mut cursor).ok()? as usize;
        if cursor.len() < item_len + CHECKSUM_LEN {
            return None;
        }

        let (_record_version, mut remaining) = cursor.split_first()?;
        let vec_len = CompactSize::read(&mut remaining).ok()? as usize;

        for i in 0..vec_len {
            let (option_tag, rest) = remaining.split_first()?;
            remaining = rest;

            if *option_tag == 0 {
                // None: nothing to skip, go to next
                if i == target_tx_idx {
                    return None;
                }
            } else if *option_tag == 1 {
                let (_tx_version, rest) = remaining.split_first()?;
                remaining = rest;

                let vin_len = CompactSize::read(&mut remaining).ok()? as usize;

                for _ in 0..vin_len {
                    if remaining.len() < TxInCompact::VERSIONED_LEN {
                        return None;
                    }
                    remaining = &remaining[TxInCompact::VERSIONED_LEN..];
                }

                let vout_len = CompactSize::read(&mut remaining).ok()? as usize;

                for out_idx in 0..vout_len {
                    if remaining.len() < TxOutCompact::VERSIONED_LEN {
                        return None;
                    }

                    let out_bytes = &remaining[..TxOutCompact::VERSIONED_LEN];

                    if i == target_tx_idx && out_idx == target_output_idx {
                        return TxOutCompact::from_bytes(out_bytes).ok();
                    }

                    remaining = &remaining[TxOutCompact::VERSIONED_LEN..];
                }
            } else {
                // Non-canonical Option tag
                return None;
            }
        }
        None
    }
}

impl Drop for DbV1 {
    fn drop(&mut self) {
        if let Some(handle) = self.db_handler.take() {
            handle.abort();
        }
    }
}
