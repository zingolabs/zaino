//! Finalised State persistent database (Schema V1)
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
                TransparentHistExt,
            },
            entry::{StoredEntryFixed, StoredEntryVar},
        },
        types::{TransactionHash, GENESIS_HEIGHT},
    },
    config::ChainIndexConfig,
    error::FinalisedStateError,
    BlockHash, BlockHeaderData, CommitmentTreeData, CompactBlockStream, CompactOrchardAction,
    CompactSaplingOutput, CompactSaplingSpend, CompactSize, CompactTxData, FixedEncodedLen as _,
    Height, IndexedBlock, NamedAtomicStatus, OrchardCompactTx, OrchardTxList, Outpoint,
    SaplingCompactTx, SaplingTxList, StatusType, TransparentCompactTx, TransparentTxList,
    TxInCompact, TxLocation, TxOutCompact, TxidList, ZainoVersionedSerde as _,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{chain_index::types::AddrEventBytes, AddrHistRecord, AddrScript};

use zaino_proto::proto::{compact_formats::CompactBlock, utils::PoolTypeFilter};
use zebra_chain::parameters::NetworkKind;
use zebra_state::HashOrHeight;

use super::LmdbLifecycle;

use async_trait::async_trait;
use corez::io::{self, Read};
use dashmap::DashSet;
use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, Transaction as _, WriteFlags,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::{
    collections::HashSet,
    fs,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub(crate) mod validation;

pub(crate) mod read_core;
pub(crate) mod write_core;

pub(crate) mod block_core;
pub(crate) mod block_shielded;
pub(crate) mod block_transparent;

pub(crate) mod compact_block;
pub(crate) mod indexed_block;

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
    0x11, 0xb2, 0x6a, 0x12, 0x08, 0x67, 0xf0, 0x42, 0xf6, 0x31, 0x45, 0xea, 0x87, 0xe7, 0x23, 0x75,
    0x40, 0x3b, 0xf2, 0x14, 0xaa, 0x2b, 0x00, 0x12, 0xec, 0xa4, 0x4d, 0x00, 0xe9, 0x0b, 0x07, 0x9b,
];

/// *Current* database V1 version.
pub(crate) const DB_VERSION_V1: DbVersion = DbVersion {
    major: 1,
    minor: 2,
    patch: 1,
};

/// LMDB table name for the finalised txout-set accumulator.
pub(crate) const TX_OUT_SET_INFO_ACCUMULATOR_DATABASE_NAME: &str =
    "tx_out_set_info_accumulator_1_2_0";

/// Singleton key for the finalised txout-set accumulator table.
pub(crate) const TX_OUT_SET_INFO_ACCUMULATOR_KEY: &[u8] = b"tx_out_set_info_accumulator";

/// Metadata key recording the height the finalised txout-set accumulator currently reflects.
///
/// Stored in the `metadata` table as `StoredEntryFixed<Height>`. The accumulator is not maintained
/// per block on the bulk-sync write path. After a catch-up run it is brought up to the tip either by
/// a full from-genesis rebuild ([`DbV1::rebuild_tx_out_set_accumulator`], used for the first build /
/// an unusually large gap) or, in steady state, by applying just the delta for the newly-written
/// range ([`DbV1::update_tx_out_set_accumulator_for_range`]). Both advance this watermark to the new
/// tip in the same transaction as the accumulator. It lets the dispatch pick the cheap incremental
/// path and lets readers detect a *stale* accumulator (watermark `<` db tip) after a sync was
/// interrupted before the accumulator step ran, rather than serving incorrect `gettxoutsetinfo` data.
pub(crate) const TX_OUT_SET_ACCUMULATOR_BUILT_HEIGHT_KEY: &[u8] =
    b"_tx_out_set_accumulator_built_height";

/// Maximum accumulator staleness (`db_tip - watermark`, in blocks) still updated incrementally.
///
/// Below this gap, [`DbV1::write_blocks_to_height`] advances the persisted txout-set accumulator by
/// applying only the delta for the just-written range — O(range) work, independent of chain length.
/// At or above it (the first build, or a sync interrupted far behind the on-disk tip) it falls back
/// to the full from-genesis [`DbV1::rebuild_tx_out_set_accumulator`]. The incremental path does
/// ~O(range outputs) random `spent`/prev-output lookups (page faults once the DB exceeds RAM), so
/// this is set conservatively — well under the fixed full-scan cost — while still covering a
/// multi-hour offline catch-up. It is a performance knob, not a correctness one:
/// both paths produce the identical accumulator at the tip.
pub(crate) const ACCUMULATOR_INCREMENTAL_MAX_GAP: u32 = 1_000;

/// Number of txid-prefix shards used by the bulk txout-set accumulator builder.
///
/// The builder holds the set of spent outpoints in memory while scanning the block data. Sharding
/// on the creating-txid's first byte bounds that working set to roughly `1 / shards` of the total
/// spent index, at the cost of one extra sequential pass over the block data per shard. The
/// per-shard partials recombine exactly (XOR commitment + additive counters), so the result is
/// independent of the shard count. `1` is a single optimal pass and is correct on any host with
/// enough RAM for the full spent set; raise it on memory-constrained deployments.
pub(crate) const ACCUMULATOR_BUILD_SHARDS: u16 = 1;

/// Number of committed block writes / migration heights between explicit
/// `env.sync(true)` durability checkpoints.
///
/// The LMDB environment is opened with `MDB_NOSYNC` (see [`DbV1::spawn`]), so an individual
/// `txn.commit()` is *not* flushed to disk. Because the environment does not use `WRITE_MAP`,
/// LMDB still guarantees ACI on crash — only durability (D) is lost: a crash rolls the database
/// back to the last on-disk-consistent transaction, it never corrupts it (copy-on-write + dual
/// meta pages always leave a recoverable committed snapshot). Forcing a sync every
/// `SYNC_CHECKPOINT_INTERVAL` writes bounds how much committed-but-unflushed tail a crash can
/// discard. The tail is always safe to re-do: clean sync resumes from the on-disk tip and
/// re-fetches the missing blocks, and migrations resume idempotently from their progress keys.
pub(crate) const SYNC_CHECKPOINT_INTERVAL: u32 = 1000;

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

    /// Cancels the background task so it observes shutdown without waiting for
    /// the next idle-sleep or maintenance-tick boundary. Cloning the token
    /// shares cancellation state with every clone, so all background tasks
    /// (current and future) wake on a single `cancel()` call.
    cancel_token: CancellationToken,

    /// FinalisedState status.
    status: NamedAtomicStatus,

    /// BlockCache config data.
    config: ChainIndexConfig,
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
    pub(crate) async fn spawn(config: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        info!("Launching FinalisedState");

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
        //
        // `NO_SYNC`: commits are not fsynced. The core write path now does many random-key
        // inserts per block (the `spent` and `txid_location` B-trees are keyed by 32-byte
        // hashes), which made per-commit fsync the dominant sync cost once those trees outgrew
        // the page cache. Under `NO_SYNC` the OS batches that write-back; we force durability at
        // explicit checkpoints (`SYNC_CHECKPOINT_INTERVAL`) and on graceful shutdown instead.
        // `WRITE_MAP` is unset, so a crash never corrupts the database — it only discards the
        // unflushed tail of recent commits, which clean sync and migrations safely re-do.
        let env = Environment::new()
            .set_max_dbs(15)
            .set_map_size(db_size_bytes)
            .set_max_readers(max_readers)
            .set_flags(
                EnvironmentFlags::NO_TLS
                    | EnvironmentFlags::NO_READAHEAD
                    | EnvironmentFlags::NO_SYNC,
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

        // Create the DbV1 instance. We declare the variable in the outer scope and
        // initialise it in the two cfg arms so `zaino_db` is available afterwards.
        let mut zaino_db: Self;

        #[cfg(feature = "transparent_address_history_experimental")]
        {
            let address_history = super::open_or_create_db(
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
                txid_location,
                tx_out_set_info_accumulator,
                address_history,
                metadata,
                validated_tip: Arc::new(AtomicU32::new(0)),
                validated_set: DashSet::new(),
                db_handler: std::sync::Mutex::new(None),
                cancel_token: CancellationToken::new(),
                status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
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
                spent,
                txid_location,
                tx_out_set_info_accumulator,
                metadata,
                validated_tip: Arc::new(AtomicU32::new(0)),
                validated_set: DashSet::new(),
                db_handler: std::sync::Mutex::new(None),
                cancel_token: CancellationToken::new(),
                status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
                config: config.clone(),
            };
        }

        // Validate (or initialise) the metadata entry before we touch any tables.
        zaino_db.check_schema_version().await?;

        // Temporary 0.4.0-alpha.1 compatibility: heal a cache whose alpha migration left the
        // `txid_location` index unbuilt. Runs before the background validator starts so it operates
        // on a quiescent database.
        zaino_db.reconcile_alpha_txid_location_index().await?;

        // Spawn handler task to perform background validation and trailing tx cleanup.
        zaino_db.spawn_handler().await?;

        Ok(zaino_db)
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
            spent: self.spent,
            txid_location: self.txid_location,
            tx_out_set_info_accumulator: self.tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
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
                    let (r1, r2) =
                        tokio::join!(zaino_db.initial_spent_scan(), zaino_db.initial_block_scan(),);

                    for (desc, result) in [("spent scan", r1), ("block scan", r2)] {
                        if let Err(e) = result {
                            error!("initial {desc} failed: {e}");
                            zaino_db.status.store(StatusType::CriticalError);
                            // TODO: Handle error better? - Return invalid block error from validate?
                            return;
                        }
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
                        Some(entry.inner().context.index.hash)
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

    /// Validates every stored spent-outpoint entry (`Outpoint` -> `TxLocation`) by checksum.
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

    /// Scans the whole finalised chain once at start-up and validates every block by checksum and
    /// continuity.
    ///
    /// Iterates the height-keyed `headers` table, which LMDB orders by big-endian height — i.e. in
    /// **ascending block-height order**. This lets `validated_tip` advance monotonically as each
    /// height is validated (every height is `validated_tip + 1` in turn), and surfaces any gap
    /// immediately (the parent-hash continuity check in `validate_block_blocking` fails at the first
    /// missing height). The previous implementation iterated the hash-keyed `heights` table, which
    /// validated in pseudo-random height order — thrashing the cache and preventing the tip from
    /// advancing until the whole set had been validated.
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
            txid_location: self.txid_location,
            tx_out_set_info_accumulator: self.tx_out_set_info_accumulator,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history: self.address_history,
            metadata: self.metadata,
            validated_tip: Arc::clone(&self.validated_tip),
            validated_set: self.validated_set.clone(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
        };

        tokio::task::spawn_blocking(move || {
            let ro = zaino_db.env.begin_ro_txn()?;
            let mut cursor = ro.open_ro_cursor(zaino_db.headers)?;

            // `headers` is keyed by big-endian height, so the cursor yields blocks in ascending
            // height order. Both the height and hash are read from the header entry itself.
            for (height_bytes, header_entry_bytes) in cursor.iter() {
                let height = Height::from_bytes(height_bytes)?;
                let header_entry = StoredEntryVar::<BlockHeaderData>::from_bytes(
                    header_entry_bytes,
                )
                .map_err(|e| FinalisedStateError::Custom(format!("corrupt header entry: {e}")))?;
                let hash = *header_entry.inner().context.hash();

                zaino_db.validate_block_blocking(height, hash)?
            }

            Ok(())
        })
        .await
        .map_err(|e| FinalisedStateError::Custom(format!("spawn_blocking failed: {e}")))?
    }

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

    /// Provides access to the transparent DB table, required for Migration1_1_0To1_2_0 Stage B to
    /// read stored block transparent data directly. Reading the table raw (rather than via the
    /// `BlockTransparentExt` accessor) deliberately bypasses `validate_block_blocking`: the
    /// migration backfills from already-on-disk, already-trusted data, so per-height block
    /// re-validation (merkle-root recompute + full-payload checksums) is redundant cost. The
    /// background validator started at spawn is responsible for validating the on-disk chain.
    pub(crate) fn transparent_db(&self) -> Database {
        self.transparent
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

    /// Provides access to the finalised txout-set accumulator DB table.
    pub(crate) fn tx_out_set_info_accumulator_db(&self) -> Database {
        self.tx_out_set_info_accumulator
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
        config: &ChainIndexConfig,
    ) -> Result<Self, FinalisedStateError> {
        info!("Launching FinalisedState");

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
        //
        // `NO_SYNC`: commits are not fsynced. The core write path now does many random-key
        // inserts per block (the `spent` and `txid_location` B-trees are keyed by 32-byte
        // hashes), which made per-commit fsync the dominant sync cost once those trees outgrew
        // the page cache. Under `NO_SYNC` the OS batches that write-back; we force durability at
        // explicit checkpoints (`SYNC_CHECKPOINT_INTERVAL`) and on graceful shutdown instead.
        // `WRITE_MAP` is unset, so a crash never corrupts the database — it only discards the
        // unflushed tail of recent commits, which clean sync and migrations safely re-do.
        let env = Environment::new()
            .set_max_dbs(15)
            .set_map_size(db_size_bytes)
            .set_max_readers(max_readers)
            .set_flags(
                EnvironmentFlags::NO_TLS
                    | EnvironmentFlags::NO_READAHEAD
                    | EnvironmentFlags::NO_SYNC,
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

        let mut zaino_db: Self;
        #[cfg(feature = "transparent_address_history_experimental")]
        {
            let address_history = super::open_or_create_db(
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
                txid_location,
                tx_out_set_info_accumulator,
                address_history,
                metadata,
                validated_tip: Arc::new(AtomicU32::new(0)),
                validated_set: DashSet::new(),
                db_handler: std::sync::Mutex::new(None),
                cancel_token: CancellationToken::new(),
                status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
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
                spent,
                txid_location,
                tx_out_set_info_accumulator,
                metadata,
                validated_tip: Arc::new(AtomicU32::new(0)),
                validated_set: DashSet::new(),
                db_handler: std::sync::Mutex::new(None),
                cancel_token: CancellationToken::new(),
                status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
                config: config.clone(),
            };
        }

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

        // Spawn handler task to perform background validation and trailing tx cleanup.
        zaino_db.spawn_handler().await?;

        Ok(zaino_db)
    }
}
