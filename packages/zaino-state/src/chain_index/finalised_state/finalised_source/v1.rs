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
    CompactSaplingSpend, CompactSize, CompactTxData, FixedEncodedLen as _, Height, IndexedBlock,
    NamedAtomicStatus, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList,
    StatusType, TransparentCompactTx, TransparentTxList, TxInCompact, TxLocation, TxOutCompact,
    TxidList, ZainoVersionedSerde as _,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{chain_index::types::AddrEventBytes, AddrHistRecord, AddrScript};

use zaino_proto::proto::{compact_formats::CompactBlock, utils::PoolTypeFilter};
use zebra_chain::parameters::NetworkKind;
use zebra_state::HashOrHeight;

use super::LmdbLifecycle;

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

pub(crate) mod tx_out_set_accumulator;

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
    0xaf, 0xcb, 0x80, 0xfe, 0x89, 0x2b, 0xc5, 0xba, 0x8e, 0x5d, 0x20, 0xfe, 0x56, 0x72, 0x81, 0x75,
    0x58, 0x8a, 0xb6, 0x49, 0xf7, 0xc4, 0x45, 0xcd, 0xa2, 0x8f, 0xaf, 0xb9, 0x6a, 0x95, 0xc8, 0x75,
];

/// *Current* database V1 version.
pub(crate) const DB_VERSION_V1: DbVersion = DbVersion {
    major: 1,
    minor: 3,
    patch: 0,
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

/// Maximum number of txid-prefix shards used by the bulk txout-set accumulator builder.
///
/// The builder holds the set of spent outpoints in memory while scanning the block data. Sharding
/// on the creating-txid's first byte bounds that working set to roughly `1 / shards` of the total
/// spent index, at the cost of one extra sequential pass over the block data per shard. The
/// per-shard partials recombine exactly (XOR commitment + additive counters), so the result is
/// independent of the shard count.
///
/// The shard count is chosen at rebuild time so the per-shard spent set fits the configured
/// [`zaino_common::DatabaseConfig::accumulator_rebuild_memory_size`] budget (see
/// [`DbV1::rebuild_tx_out_set_accumulator`]): a single optimal pass on hosts with enough RAM for
/// the full spent set, scaling up on memory-constrained deployments. Sharding partitions on the
/// creating-txid's first byte (256 distinct values), so the count is capped here.
pub(crate) const ACCUMULATOR_BUILD_MAX_SHARDS: u16 = 256;

/// Conservative per-entry RAM estimate for the rebuild's in-memory spent set, used only to size the
/// shard count.
///
/// Each entry is a 37-byte `spent` key heap-allocated as a `Box<[u8]>` (rounded up by the
/// allocator), a 16-byte fat pointer stored in the `HashSet` table, plus hashbrown control bytes and
/// load-factor slack — realistically ~120 bytes, but allocator behaviour varies. Set deliberately
/// *above* that so the chosen shard count over-provisions: the per-shard set then stays within the
/// budget, and over-counting only adds shards (less memory per shard), it never under-bounds.
pub(crate) const SPENT_SET_ENTRY_BYTES_ESTIMATE: u64 = 256;

/// Number of committed block writes / migration heights between explicit
/// `env.sync(true)` durability checkpoints.
///
/// This governs the durability-sync cadence of the **per-block steady-state append**
/// ([`DbWrite::write_block`]) and the **migration backfill scan**: both commit frequently but, under
/// `MDB_NOSYNC`, force an `env.sync(true)` only every `SYNC_CHECKPOINT_INTERVAL` committed
/// writes/heights. (The separate **bulk catch-up** path batches differently — by the
/// `sync_write_batch_size` byte budget, a block-count cap, and the wall-clock
/// `DatabaseConfig::sync_checkpoint_interval` — and fsyncs once per committed batch, so it does not
/// use this constant.)
///
/// The LMDB environment is opened with `MDB_NOSYNC` (see [`DbV1::spawn`]), so an individual
/// `txn.commit()` is *not* flushed to disk. On a **write-order-preserving local filesystem** this
/// only costs durability (D): LMDB's copy-on-write + dual meta pages mean a crash rolls back to the
/// last fully-flushed transaction without corrupting the database, and the checkpoint cadence bounds
/// how much committed-but-unflushed tail a crash can discard. The tail is always safe to re-do:
/// clean sync resumes from the on-disk tip and re-fetches the missing blocks, and migrations resume
/// idempotently from their progress keys.
///
/// CAVEAT: that integrity guarantee relies on the filesystem preserving write order. On networked
/// storage (NFS), overlay filesystems, or a container/pod hard-eviction that drops the unflushed
/// page cache, write order is *not* guaranteed and a crash under `MDB_NOSYNC` **can** leave torn
/// pages — surfacing later as an LMDB cursor assertion or a checksum/decode failure on the affected
/// table. The recovery is to wipe the finalised-state DB and re-index (its tables are all
/// re-derivable from the validator). A shorter checkpoint interval shrinks, but does not eliminate,
/// this window.
pub(crate) const SYNC_CHECKPOINT_INTERVAL: u32 = 1000;

/// Formats a one-line corruption report for a malformed `spent` entry: its position in the table,
/// the key (hex), the value length, and the value's leading bytes (hex), plus the re-index hint.
///
/// The leading bytes are the diagnostic that distinguishes failure modes: a value that should begin
/// with the `StoredEntryFixed` version tag but starts with arbitrary bytes (e.g. `0xfd`) is a torn
/// write, whereas a recognisable-but-old structure would indicate a format problem.
fn spent_corruption_detail(
    entry_index: u64,
    key_bytes: &[u8],
    val_bytes: &[u8],
    reason: &str,
) -> String {
    /// Cap on value bytes included in the report — enough to identify the framing, not the whole row.
    const VALUE_HEAD_BYTES: usize = 16;
    let value_head = hex::encode(&val_bytes[..val_bytes.len().min(VALUE_HEAD_BYTES)]);
    format!(
        "corrupt spent entry #{entry_index} ({reason}): key=0x{key}, value_len={value_len}, \
         value_head=0x{value_head}; the finalised-state spent table is damaged — wipe the database \
         and re-index",
        key = hex::encode(key_bytes),
        value_len = val_bytes.len(),
    )
}

/// [`DbCore`] capability implementation for [`DbV1`].
///
/// This trait exposes lifecycle operations and a high-level status indicator.
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

    /// Ironwood: `Height` -> `StoredEntryVar<OrchardTxList>`
    ///
    /// Ironwood (NU6.3) shielded-pool actions, modelled with the Orchard compact types. Stored
    /// per-block, in order. Introduced in schema v1.3.0.
    ironwood: Database,

    /// Block commitment tree data: `Height` -> `StoredEntryVar<CommitmentTreeData>`
    ///
    /// Stored per-block, in order. The value is a `StoredEntryVar` (not `StoredEntryFixed`) from
    /// schema v1.3.0 onward, because `CommitmentTreeData` V2 carries an optional Ironwood root and
    /// is therefore variable-length.
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
    /// Opens (and heals) the v1 database for the configured network **without** starting the
    /// background validator.
    ///
    /// This method:
    /// - chooses a versioned path suffix (`.../<network>/v1`),
    /// - configures LMDB map size and reader slots,
    /// - opens or creates all V1 named databases, and
    /// - validates or initializes the `"metadata"` record (schema hash + version).
    ///
    /// The validator is started separately via [`DbV1::start_validator`]. This split exists so the
    /// orchestrator can guarantee that any pending schema migration finishes *before* validation
    /// runs: the validator's `initial_block_scan` reads tables (e.g. `commitment_tree_data_1_3_0`)
    /// that a migration populates, so starting it concurrently with a migration races the migration
    /// and can fail on a not-yet-written row.
    pub(crate) async fn spawn(config: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        let zaino_db = Self::open_env_and_dbs(config).await?;

        // Validate (or initialise) the metadata entry before we touch any tables.
        zaino_db.check_schema_version().await?;

        // Temporary 0.4.0-alpha.1 compatibility: heal a cache whose alpha migration left the
        // `txid_location` index unbuilt. Runs before the background validator starts so it operates
        // on a quiescent database.
        zaino_db.reconcile_alpha_txid_location_index().await?;

        Ok(zaino_db)
    }

    /// Opens the LMDB environment and every V1 named database, returning an *unstarted* [`DbV1`]
    /// (status `Spawning`, `db_handler` = `None`, fresh atomics). Performs no metadata validation
    /// and starts no background task — each caller (`spawn`, `spawn_v1_0_0`) adds its own tail.
    ///
    /// The `commitment_tree_data` handle is the up-to-date `commitment_tree_data_1_3_0` table
    /// (`StoredEntryVar`). The v1.0.0 fixture builder ([`DbV1::spawn_v1_0_0`]) opens the legacy
    /// `commitment_tree_data_1_0_0` table (`StoredEntryFixed`) instead, via
    /// [`DbV1::open_env_and_dbs_with_commitment_table`].
    async fn open_env_and_dbs(config: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        Self::open_env_and_dbs_with_commitment_table(config, "commitment_tree_data_1_3_0").await
    }

    /// [`DbV1::open_env_and_dbs`] with the commitment-tree table name as a parameter, so the
    /// v1.0.0 fixture opener can select the legacy table without creating the v1.3.0 one
    /// (on-disk version detection keys off which tables exist).
    async fn open_env_and_dbs_with_commitment_table(
        config: &ChainIndexConfig,
        commitment_table: &str,
    ) -> Result<Self, FinalisedStateError> {
        info!("Launching FinalisedState");

        // Prepare database details and path.
        let db_size_bytes = config.storage.database.size.to_byte_count();
        let db_path_dir = match config.network.kind() {
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
        // `WRITE_MAP` is unset, so on a write-order-preserving local filesystem a crash does not
        // corrupt the database — it only discards the unflushed tail of recent commits, which clean
        // sync and migrations safely re-do. (On NFS / overlay filesystems or a hard pod eviction that
        // drops the unflushed page cache, write order is not guaranteed and a crash *can* leave torn
        // pages; the recovery there is to wipe and re-index. See `SYNC_CHECKPOINT_INTERVAL`.)
        let env = Environment::new()
            .set_max_dbs(16)
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
        let ironwood =
            super::open_or_create_db(&env, "ironwood_1_3_0", DatabaseFlags::empty()).await?;
        let commitment_tree_data =
            super::open_or_create_db(&env, commitment_table, DatabaseFlags::empty()).await?;
        let hashes = super::open_or_create_db(&env, "hashes_1_0_0", DatabaseFlags::empty()).await?;

        let spent = super::open_or_create_db(&env, "spent_1_0_0", DatabaseFlags::empty()).await?;

        let txid_location =
            super::open_or_create_db(&env, "txid_location_1_0_0", DatabaseFlags::empty()).await?;

        let metadata = super::open_or_create_db(&env, "metadata", DatabaseFlags::empty()).await?;

        #[cfg(feature = "transparent_address_history_experimental")]
        let address_history = super::open_or_create_db(
            &env,
            "address_history_1_0_0",
            DatabaseFlags::DUP_SORT | DatabaseFlags::DUP_FIXED,
        )
        .await?;

        Ok(Self {
            // Opened inline here, before `env` is moved into its `Arc` below (struct fields are
            // evaluated top-to-bottom).
            tx_out_set_info_accumulator: super::open_or_create_db(
                &env,
                TX_OUT_SET_INFO_ACCUMULATOR_DATABASE_NAME,
                DatabaseFlags::empty(),
            )
            .await?,
            env: Arc::new(env),
            headers,
            txids,
            transparent,
            sapling,
            orchard,
            ironwood,
            commitment_tree_data,
            heights: hashes,
            spent,
            txid_location,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history,
            metadata,
            validated_tip: Arc::new(AtomicU32::new(0)),
            validated_set: DashSet::new(),
            db_handler: std::sync::Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
            config: config.clone(),
        })
    }

    /// A detached handle-copy of this DB for moving into a `spawn` / `spawn_blocking`
    /// task: shares the env and atomics (`Arc`), copies the `Database` handles (they are
    /// `Copy`), and resets `db_handler` — the copy is not the background-task lifecycle owner.
    fn detached_handle(&self) -> Self {
        Self {
            env: Arc::clone(&self.env),
            headers: self.headers,
            txids: self.txids,
            transparent: self.transparent,
            sapling: self.sapling,
            orchard: self.orchard,
            ironwood: self.ironwood,
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
    ///
    /// Kept separate from [`DbV1::spawn`] so the orchestrator starts it only once all pending
    /// migrations have finished (the validator reads tables a migration populates). Takes `&self`
    /// (the join handle lives behind a `Mutex`) so it can be driven through the shared
    /// `Arc<FinalisedSource>` the router holds after spawn.
    pub(super) fn start_validator(&self) {
        // Clone everything the task needs so we can move it into the async block.
        let zaino_db = self.detached_handle();

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
                            error!(%e, desc, "initial validation failed");
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
                            error!(%e, desc, "initial validation failed");
                            zaino_db.status.store(StatusType::CriticalError);
                            // TODO: Handle error better? - Return invalid block error from validate?
                            return;
                        }
                    }
                }

                info!(
                    tip = zaino_db.validated_tip.load(Ordering::Relaxed),
                    "initial validation complete"
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
                            warn!(height = ?next_height, %e, "failed to serialize height");
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
                            warn!(%e, "block validation failed");
                        }
                        // Immediately loop – maybe the chain has more blocks ready.
                        continue;
                    }

                    zaino_db.zaino_db_handler_sleep(&mut maintenance).await;
                }
            }
        });

        *self.db_handler.lock().expect("db_handler mutex poisoned") = Some(handle);
    }

    /// Validates every stored spent-outpoint entry (`Outpoint` -> `TxLocation`) by checksum.
    ///
    /// On the first malformed entry the error dumps the offending key, the value length, and the
    /// value's leading bytes (hex). This is deliberately specific: a "version tag 253"-style failure
    /// here means the on-disk `spent` table is damaged (almost always a torn write from an unclean
    /// exit under `NO_SYNC` on networked/evicted storage — see [`SYNC_CHECKPOINT_INTERVAL`]), and the
    /// dumped bytes let an operator confirm torn-page corruption vs. a genuine format problem. The
    /// recovery is to wipe the finalised-state database and re-index.
    async fn initial_spent_scan(&self) -> Result<(), FinalisedStateError> {
        let env = self.env.clone();
        let spent = self.spent;

        tokio::task::spawn_blocking(move || {
            // Logged before the scan so that a native LMDB abort while walking a torn `spent` B-tree
            // (which bypasses Rust error handling) is still attributable to this startup phase.
            info!("validating finalised-state spent table integrity");
            let ro = env.begin_ro_txn()?;
            let cursor = ro.open_ro_cursor(spent)?;

            // Explicit cursor walk rather than `Cursor::iter` (which `debug_assert!`-panics on a
            // non-`NotFound` LMDB error in debug and silently ends the scan in release): a real LMDB
            // error propagates cleanly and the scan only ends on a genuine end-of-table `NotFound`.
            let mut entry_index: u64 = 0;
            let mut op = lmdb_sys::MDB_FIRST;
            loop {
                let (key_bytes, val_bytes) = match cursor.get(None, None, op) {
                    Ok((Some(key), value)) => (key, value),
                    // `MDB_FIRST`/`MDB_NEXT` always report the key; treat a missing key as end-of-data.
                    Ok((None, _)) => break,
                    Err(lmdb::Error::NotFound) => break,
                    Err(error) => return Err(FinalisedStateError::LmdbError(error)),
                };
                op = lmdb_sys::MDB_NEXT;

                let entry =
                    StoredEntryFixed::<TxLocation>::from_bytes(val_bytes).map_err(|error| {
                        FinalisedStateError::Custom(spent_corruption_detail(
                            entry_index,
                            key_bytes,
                            val_bytes,
                            &error.to_string(),
                        ))
                    })?;

                if !entry.verify(key_bytes) {
                    return Err(FinalisedStateError::Custom(spent_corruption_detail(
                        entry_index,
                        key_bytes,
                        val_bytes,
                        "checksum mismatch",
                    )));
                }

                entry_index += 1;
            }

            info!("finalised-state spent table integrity check passed ({entry_index} entries)");
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
        let zaino_db = self.detached_handle();

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

    /// Provides access to the (v1.3.0) `StoredEntryVar` commitment-tree-data table, required for
    /// Migration1_2_1To1_3_0 to write the rebuilt commitment rows.
    pub(crate) fn commitment_tree_data_db(&self) -> Database {
        self.commitment_tree_data
    }

    /// Provides access to the (v1.3.0) `ironwood` table, required for Migration1_2_1To1_3_0 to
    /// backfill ironwood rows for post-NU6.3 blocks from validator-fetched block data.
    pub(super) fn ironwood_db(&self) -> Database {
        self.ironwood
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
                version = %metadata.version,
                "detected 0.4.0-alpha.1 cache with unbuilt txid_location index; \
                 rolling version back to 1.1.0 for corrected migration"
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
        // The v1.0.0 fixture reproduces the legacy on-disk layout: its commitment tree data lives in
        // `commitment_tree_data_1_0_0` as a `StoredEntryFixed` (see `write_block_v1_0_0`), which the
        // v1.2.1 -> v1.3.0 migration later rebuilds into the `commitment_tree_data_1_3_0`
        // `StoredEntryVar` table. This opener therefore opens the legacy commitment table and never
        // creates the v1.3.0 table.
        let zaino_db =
            Self::open_env_and_dbs_with_commitment_table(config, "commitment_tree_data_1_0_0")
                .await?;

        // Write the historical v1.0.0 metadata record. Intentionally skips `check_schema_version`
        // (see the method doc) — that is the behavioural difference from `spawn`.
        zaino_db.write_v1_0_0_metadata()?;

        // Deliberately does NOT start the background validator. This builds a *pre-migration*
        // v1.0.0 fixture; the validator validates against the current (v1.3.0) schema — it reads
        // `commitment_tree_data_1_3_0` and the `ironwood` table — so it must run only after the
        // database has been migrated to the newest schema. Callers build the fixture with direct
        // v1.0.0 writes, shut it down, then reopen through `FinalisedState::spawn`, which migrates
        // first and starts the validator afterwards.
        //
        // With no validator to advance it, mark the empty fixture `Ready` directly so callers see a
        // settled backend.
        zaino_db.status.store(StatusType::Ready);

        Ok(zaino_db)
    }

    /// Writes the historical v1.0.0 `"metadata"` record (version 1.0.0, zero schema hash, migration
    /// status `Empty`) used only by [`DbV1::spawn_v1_0_0`]. Unlike [`DbV1::check_schema_version`],
    /// this initialises metadata with the historical v1.0.0 value the migration tests require
    /// instead of the current [`DB_VERSION_V1`] — which is why `spawn_v1_0_0` deliberately does not
    /// call `check_schema_version`.
    fn write_v1_0_0_metadata(&self) -> Result<(), FinalisedStateError> {
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

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
                self.metadata,
                b"metadata",
                &entry.to_bytes()?,
                WriteFlags::NO_OVERWRITE,
            )?;

            txn.commit()?;

            Ok::<(), FinalisedStateError>(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spent_corruption_detail_reports_key_length_and_value_head() {
        // A torn `spent` value whose framing byte is 0xfd (the "version tag 253" report) plus extra
        // padding so the value length is clearly wrong for a `StoredEntryFixed<TxLocation>` (40 B).
        let key = [0xab_u8; 37];
        let value = [0xfd_u8; 50];

        let detail = spent_corruption_detail(7, &key, &value, "unsupported Zaino version tag 253");

        assert!(detail.contains("corrupt spent entry #7"), "{detail}");
        assert!(
            detail.contains("unsupported Zaino version tag 253"),
            "{detail}"
        );
        assert!(
            detail.contains(&format!("key=0x{}", hex::encode(key))),
            "{detail}"
        );
        assert!(detail.contains("value_len=50"), "{detail}");
        // Only the leading 16 bytes of the value are dumped.
        assert!(
            detail.contains(&format!("value_head=0x{}", "fd".repeat(16))),
            "{detail}"
        );
        assert!(
            detail.contains("wipe the database and re-index"),
            "{detail}"
        );
    }

    #[test]
    fn spent_corruption_detail_handles_short_values() {
        // A value shorter than the dump cap must not panic on the slice.
        let detail = spent_corruption_detail(0, &[0x01, 0x02], &[0x09], "checksum mismatch");

        assert!(detail.contains("value_len=1"), "{detail}");
        assert!(detail.contains("value_head=0x09"), "{detail}");
    }

    /// End-to-end: a malformed value in the `spent` table makes the startup integrity scan return
    /// the actionable diagnostic (key / length / value bytes + re-index hint), not a bare error or a
    /// silent pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn initial_spent_scan_reports_corrupt_value() {
        use lmdb::{Transaction as _, WriteFlags};
        use zaino_common::network::ActivationHeights;
        use zaino_common::{DatabaseConfig, StorageConfig};

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let config = ChainIndexConfig {
            storage: StorageConfig {
                database: DatabaseConfig {
                    path: temp_dir.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ephemeral: false,
            db_version: 1,
            network: ActivationHeights::default().to_regtest_network(),
        };

        let db = DbV1::spawn(&config).await.expect("spawn empty v1 db");

        // Inject a malformed value under a valid outpoint key: a `StoredEntryFixed<TxLocation>` is 40
        // bytes beginning with a version tag of 1; this is 50 bytes of 0xfd (the "version tag 253"
        // torn-write shape).
        let key = Outpoint::new([0x11; 32], 0)
            .to_bytes()
            .expect("encode outpoint");
        let garbage = [0xfd_u8; 50];
        {
            let mut txn = db.env.begin_rw_txn().expect("rw txn");
            txn.put(db.spent, &key, &garbage, WriteFlags::empty())
                .expect("put garbage spent value");
            txn.commit().expect("commit");
        }

        let error = db
            .initial_spent_scan()
            .await
            .expect_err("a malformed spent value must be rejected");
        let message = error.to_string();

        assert!(message.contains("corrupt spent entry"), "{message}");
        assert!(message.contains("value_len=50"), "{message}");
        assert!(
            message.contains(&format!("key=0x{}", hex::encode(&key))),
            "{message}"
        );
        assert!(
            message.contains("wipe the database and re-index"),
            "{message}"
        );
    }
}
