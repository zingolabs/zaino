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
//! ## Validation model
//! The database maintains a monotonically increasing **validated tip** (`validated_tip`) and a set
//! of validated heights above that tip (`validated_set`) to support out-of-order validation. Reads
//! that require correctness use `resolve_validated_hash_or_height()` to ensure the requested height
//! is validated (performing on-demand validation if required).
//!
//! A background task performs:
//! - an initial full scan of the stored data for checksum / structural correctness, then
//! - steady-state incremental validation of newly appended blocks.
//!
//! ## Concurrency model
//! LMDB supports many concurrent readers and a single writer per environment. This implementation
//! uses `tokio::task::block_in_place` / `spawn_blocking` for LMDB operations to avoid blocking the
//! async runtime, and configures `max_readers` to support high read concurrency.

use crate::{
    chain_index::{
        finalised_state::{
            capability::{
                BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbCore,
                DbMetadata, DbRead, DbVersion, DbWrite, IndexedBlockExt, MigrationStatus,
            },
            entry::{StoredEntryFixed, StoredEntryVar},
        },
        types::{TransactionHash, GENESIS_HEIGHT},
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockHeaderData, CommitmentTreeData, CompactBlockStream, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactSize, CompactTxData, FixedEncodedLen as _,
    Height, IndexedBlock, NamedAtomicStatus, OrchardCompactTx, OrchardTxList, SaplingCompactTx,
    SaplingTxList, StatusType, TransparentCompactTx, TransparentTxList, TxInCompact, TxLocation,
    TxOutCompact, TxidList, ZainoVersionedSerde as _,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{
    chain_index::{finalised_state::capability::TransparentHistExt, types::AddrEventBytes},
    AddrHistRecord, AddrScript, Outpoint,
};

use zaino_proto::proto::{compact_formats::CompactBlock, utils::PoolTypeFilter};
use zebra_chain::parameters::NetworkKind;
use zebra_state::HashOrHeight;

use async_trait::async_trait;
use corez::io::{self, Read};
use dashmap::DashSet;
use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction as _, WriteFlags,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::Notify,
    time::{interval, MissedTickBehavior},
};
use tracing::{error, info, warn};

#[cfg(feature = "transparent_address_history_experimental")]
use std::collections::HashMap;

pub(crate) mod validation;

pub(crate) mod read_core;
pub(crate) mod write_core;

pub(crate) mod block_core;
pub(crate) mod block_shielded;
pub(crate) mod block_transparent;

pub(crate) mod compact_block;
pub(crate) mod indexed_block;

#[cfg(feature = "transparent_address_history_experimental")]
pub(crate) mod transparent_address_history;

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
    0xa8, 0x19, 0x61, 0x50, 0xff, 0xb6, 0x9e, 0xf8, 0xb2, 0xb5, 0x31, 0x80, 0xdd, 0x90, 0xd0, 0x67,
    0x41, 0x57, 0xfc, 0x51, 0x39, 0xa1, 0x3a, 0xbe, 0xce, 0x70, 0x4e, 0x51, 0x55, 0xc3, 0x3a, 0x0a,
];

/// *Current* database V1 version.
pub(crate) const DB_VERSION_V1: DbVersion = DbVersion {
    major: 1,
    minor: 1,
    patch: 0,
};

/// [`DbCore`] capability implementation for [`DbV1`].
///
/// This trait exposes lifecycle operations and a high-level status indicator.
#[async_trait]
impl DbCore for DbV1 {
    fn status(&self) -> StatusType {
        self.status()
    }

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

/// Zaino’s Finalised State database V1.
///
/// This type owns an LMDB [`Environment`] and a fixed set of named databases representing the V1
/// schema. It implements the capability traits used by the rest of the chain indexer.
///
/// Data is stored per-height in “best chain” order and is validated (checksums and continuity)
/// before being treated as reliable for downstream reads.
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
    #[cfg(feature = "transparent_address_history_experimental")]
    spent: Database,

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

    /// Background validator / maintenance task handle.
    ///
    /// Wrapped in a `Mutex` so `shutdown(&self)` can `.take()` the handle on
    /// the trait's `&self` signature. The lock is only held to swap the
    /// `Option`; no `.await` happens while it's held.
    db_handler: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    /// Wakes the background task out of `zaino_db_handler_sleep` when shutdown
    /// is requested, so it observes `StatusType::Closing` without waiting for
    /// the next idle-sleep or maintenance-tick boundary.
    shutdown_notify: Arc<Notify>,

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

        let metadata = Self::open_or_create_db(&env, "metadata", DatabaseFlags::empty()).await?;

        // Create the DbV1 instance. We declare the variable in the outer scope and
        // initialise it in the two cfg arms so `zaino_db` is available afterwards.
        let mut zaino_db: Self;

        #[cfg(feature = "transparent_address_history_experimental")]
        {
            let spent =
                Self::open_or_create_db(&env, "spent_1_0_0", DatabaseFlags::empty()).await?;

            let address_history = Self::open_or_create_db(
                &env,
                "address_history_1_0_0",
                DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
            )
            .await?;

            zaino_db = Self {
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
                db_handler: std::sync::Mutex::new(None),
                shutdown_notify: Arc::new(Notify::new()),
                status: NamedAtomicStatus::new("ZainoDB", StatusType::Spawning),
                config: config.clone(),
            };
        }

        #[cfg(not(feature = "transparent_address_history_experimental"))]
        {
            zaino_db = Self {
                env: Arc::new(env),
                headers,
                txids,
                transparent,
                sapling,
                orchard,
                commitment_tree_data,
                heights: hashes,
                metadata,
                validated_tip: Arc::new(AtomicU32::new(0)),
                validated_set: DashSet::new(),
                db_handler: std::sync::Mutex::new(None),
                shutdown_notify: Arc::new(Notify::new()),
                status: NamedAtomicStatus::new("ZainoDB", StatusType::Spawning),
                config: config.clone(),
            };
        }

        // Validate (or initialise) the metadata entry before we touch any tables.
        zaino_db.check_schema_version().await?;

        // Spawn handler task to perform background validation and trailing tx cleanup.
        zaino_db.spawn_handler().await?;

        Ok(zaino_db)
    }

    /// Returns the status of ZainoDB.
    pub(crate) fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Waits until the DB reaches [`StatusType::Ready`].
    ///
    /// NOTE: This does not currently backpressure on LMDB reader availability.
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
    /// The task runs:
    /// - **Startup:** full validation passes (`initial_spent_scan`, `initial_address_history_scan`,
    ///   `initial_block_scan`).
    /// - **Steady state:** periodically attempts to validate the next height after `validated_tip`.
    ///   Separately, it performs periodic trailing-reader cleanup via `clean_trailing()`.
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
            #[cfg(feature = "transparent_address_history_experimental")]
            spent: self.spent,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::clone(&self.shutdown_notify),
            status: self.status.clone(),
            config: self.config.clone(),
        };

        let handle = tokio::spawn({
            let zaino_db = zaino_db;
            async move {
                // *** initial validation ***
                zaino_db.status.store(StatusType::Syncing);

                #[cfg(feature = "transparent_address_history_experimental")]
                {
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
                }
                #[cfg(not(feature = "transparent_address_history_experimental"))]
                {
                    if let Err(e) = zaino_db.initial_block_scan().await {
                        error!("initial block scan failed: {e}");
                        zaino_db.status.store(StatusType::CriticalError);
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

        *self.db_handler.lock().expect("db_handler mutex poisoned") = Some(handle);
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
            _ = self.shutdown_notify.notified() => {},
        }
    }

    /// Validates every stored spent-outpoint entry (`Outpoint` -> `TxLocation`) by checksum.
    #[cfg(feature = "transparent_address_history_experimental")]
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

    /// Validates every stored address-history record (`AddrScript` duplicates of `AddrEventBytes`) by checksum.
    #[cfg(feature = "transparent_address_history_experimental")]
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

    /// Scans the whole finalised chain once at start-up and validates every block by checksum and continuity.
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
            #[cfg(feature = "transparent_address_history_experimental")]
            spent: self.spent,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            shutdown_notify: Arc::clone(&self.shutdown_notify),
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
