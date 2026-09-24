//! Finalised State persistent database (Schema V1)
//!
//! This module provides the **V1** implementation of Zaino’s LMDB-backed finalised-state database.
//! It stores an append-only view of the best chain and exposes a set of capability traits
//! (read, write, metadata, block-range fetchers, compact-block generation, and transparent history).
//!
//! ## On-disk layout
//! The `schema` module lists every table and its flags, and computes a 32-byte BLAKE2b schema
//! hash from those tables, the canonical encoding of each stored type, and the enabled index
//! features. The hash is stored in the database metadata; a mismatch on open rebuilds the store.
//!
//! ## Trust model
//! Blocks come from the validator and are trusted. The write path checks what Zaino itself
//! derives: each block's parent must be the stored tip, which keeps the chain append-only, and
//! its txids must reproduce its header's merkle root. The maintenance task cross-checks the spent
//! index and the address history against each stored block. Heights run from genesis to the tip
//! with no gaps, so a read only needs to confirm that its heights are stored.
//!
//! ## Concurrency model
//! LMDB supports many concurrent readers and a single writer per environment. This implementation
//! uses `tokio::task::block_in_place` / `spawn_blocking` for LMDB operations to avoid blocking the
//! async runtime, and configures `max_readers` to support high read concurrency.

use crate::codec::{CompactSize, DbCodec as _, FixedEncodedLen as _};
#[cfg(feature = "transparent_address_history_experimental")]
use crate::store::capability::TransparentHistExt;
use crate::store::capability::{
    BlockCoreExt, BlockTransparentExt, CompactBlockExt, DbCore, DbMetadata, DbRead, DbWrite,
    IndexedBlockExt, SpentOutputExt, TxOutSetExt,
};
use crate::stream::CompactBlockStream;
use crate::types::{
    AbsoluteChainWork, BlockHash, BlockHeaderData, CommitmentTreeData, CompactTxData, Height,
    IndexedBlock, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx, SaplingTxList,
    TransactionHash, TransparentCompactTx, TransparentTxList, TxInCompact, TxLocation,
    TxOutCompact, TxidList, GENESIS_HEIGHT,
};
use crate::{config::StoreSettings, error::StoreError};
/// How a caller names a block when asking this backend to resolve it.
///
/// Was `zebra_state::HashOrHeight`. Defined here instead: it is the store's own
/// lookup key — every use resolves against this backend's indexes, not against
/// a validator — and taking a dependency on a node's state crate for a
/// two-variant enum would put all of zebra-state in a storage crate's graph.
///
/// The payload types are unchanged, so every `.into()` at a call site still
/// means what it did.
///
/// TODO: replace with the domain's height and hash once the reader's internals
/// stop speaking zebra types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashOrHeight {
    /// Resolve by block hash.
    Hash(zebra_chain::block::Hash),
    /// Resolve by block height.
    Height(zebra_chain::block::Height),
}

use zaino_status::{NamedAtomicStatus, StatusType};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::types::{AddrEventBytes, AddrHistRecord, AddrScript};

use zaino_proto::proto::{compact_formats::CompactBlock, utils::PoolTypeFilter};

use super::LmdbLifecycle;

use corez::io::{self, Read};
use lmdb::{Cursor, Database, Environment, EnvironmentFlags, Transaction as _, WriteFlags};
use std::collections::HashMap;
use std::{collections::HashSet, fs, sync::Arc, time::Duration};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub(crate) mod read_core;
pub(crate) mod write_core;

pub(crate) mod block_core;
#[cfg(test)]
mod block_shielded;
pub(crate) mod block_transparent;

pub(crate) mod compact_block;
pub(crate) mod indexed_block;

pub(crate) mod transparent_address_history;

pub(crate) mod tx_out_set_accumulator;

pub(crate) mod schema;

mod index_check;

/// Whether a database's stored `metadata` record matches this build's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaCheck {
    /// The stored record carries this build's schema hash.
    Matches,
    /// The stored record carries another build's schema hash.
    Differs {
        /// The schema hash the database was written under.
        stored: [u8; 32],
    },
    /// The stored record cannot be decoded by this build, so the database was written by a build whose metadata layout it does not know.
    Unreadable,
}

impl SchemaCheck {
    /// The directory name a database with this check result is moved to beside `v1`, or `None` when it matches.
    fn stale_dir_name(self) -> Option<String> {
        match self {
            Self::Matches => None,
            Self::Differs { stored } => Some(format!(
                "v1.stale-{:02x}{:02x}{:02x}{:02x}",
                stored[0], stored[1], stored[2], stored[3]
            )),
            Self::Unreadable => Some("v1.stale-unreadable".to_string()),
        }
    }
}

/// Singleton key of the metadata record in the metadata table.
pub(crate) const METADATA_KEY: &[u8] = b"metadata";

/// Singleton key for the finalised txout-set accumulator table.
pub(crate) const TX_OUT_SET_INFO_ACCUMULATOR_KEY: &[u8] = b"tx_out_set_info_accumulator";

/// Metadata key recording the height the finalised txout-set accumulator currently reflects.
///
/// Stored in the `metadata` table as a `Height`. The accumulator is not maintained
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

/// Minimum wall-clock interval between successive progress logs emitted by a long-running
/// finalised-state scan (bulk sync, the txout-set accumulator rebuild, and the startup `spent`
/// integrity check).
///
/// Throttling on *time* rather than on a height/entry modulus keeps the output bounded no matter
/// how the underlying work is partitioned. The accumulator rebuild in particular scans the whole
/// chain once per shard, and the shard count scales with how little memory the host has (up to
/// [`ACCUMULATOR_BUILD_MAX_SHARDS`]), so a per-height modulus would emit orders of magnitude more
/// lines on exactly the constrained deployments where logs are most expensive.
pub(super) const PROGRESS_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Number of committed block writes between explicit
/// `env.sync(true)` durability checkpoints.
///
/// This governs the durability-sync cadence of the **per-block steady-state append**
/// ([`DbV1::write_block`]): it commits frequently but, under
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
/// clean sync resumes from the on-disk tip and re-fetches the missing blocks.
///
/// CAVEAT: that integrity guarantee relies on the filesystem preserving write order. On networked
/// storage (NFS), overlay filesystems, or a container/pod hard-eviction that drops the unflushed
/// page cache, write order is *not* guaranteed and a crash under `MDB_NOSYNC` **can** leave torn
/// pages — surfacing later as an LMDB cursor assertion or a decode failure on the affected
/// table. The recovery is to wipe the finalised-state DB and re-index (its tables are all
/// re-derivable from the validator). A shorter checkpoint interval shrinks, but does not eliminate,
/// this window.
pub(crate) const SYNC_CHECKPOINT_INTERVAL: u32 = 1000;

/// [`DbCore`] capability implementation for [`DbV1`].
///
/// This trait exposes lifecycle operations and a high-level status indicator.
impl DbCore for DbV1 {
    fn status(&self) -> StatusType {
        LmdbLifecycle::status(self)
    }

    async fn shutdown(&self) -> Result<(), StoreError> {
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

/// - One definition: `open` creates the env here, the size metric measures the file in
///   it; two copies drift onto different networks
fn db_path(config: &StoreSettings) -> std::path::PathBuf {
    config
        .store
        .path()
        .join(super::super::network_dir(config.db.network().kind()))
        .join("v1")
}

/// Zaino’s Finalised State database V1.
///
/// This type owns an LMDB [`Environment`] and a fixed set of named databases representing the V1
/// schema. It implements the capability traits used by the rest of the chain indexer.
///
/// Data is stored per-height in “best chain” order.
/// Lower bound on LMDB reader slots, whatever the core count.
///
/// Each slot is 64 bytes, so this floor costs ~128 KiB — cheap enough that a small host should
/// not be the reason a node stops serving concurrent readers.
const MIN_LMDB_READERS: usize = 2048;

/// Upper bound on LMDB reader slots.
///
/// 8192 slots is ~512 KiB of shared memory, and leaves headroom above a 5000 concurrent-client
/// target for Zaino's own internal readers: the sync loop, startup
/// validation, the chain head, and the mempool all take slots of their own.
const MAX_LMDB_READERS: usize = 8192;

#[derive(Debug)]
pub(crate) struct DbV1 {
    /// Shared LMDB environment.
    env: Arc<Environment>,

    /// Block headers: `Height` -> `BlockHeaderData`
    ///
    /// Stored per-block, in order.
    headers: Database,

    /// Txids: `Height` -> `TxidList`
    ///
    /// Stored per-block, in order.
    txids: Database,

    /// Transparent: `Height` -> `TransparentTxList`
    ///
    /// Stored per-block, in order.
    transparent: Database,

    /// Sapling: `Height` -> `SaplingTxList`
    ///
    /// Stored per-block, in order.
    sapling: Database,

    /// Orchard: `Height` -> `OrchardTxList`
    ///
    /// Stored per-block, in order.
    orchard: Database,

    /// Ironwood: `Height` -> `OrchardTxList`
    ///
    /// Ironwood (NU6.3) shielded-pool actions, modelled with the Orchard compact types. Stored
    /// per-block, in order. Introduced in schema v1.3.0.
    ironwood: Database,

    /// Block commitment tree data: `Height` -> `CommitmentTreeData`, variable-length because it carries an optional Ironwood root.
    commitment_tree_data: Database,

    /// Heights: `Hash` -> `Height`
    ///
    /// Used for hash based fetch of the best chain (and random access).
    heights: Database,

    /// Spent outpoints: `Outpoint` -> spending `TxLocation`
    ///
    /// Used to check spent status of given outpoints, retuning spending tx.
    spent: Database,

    /// Reverse txid index: `TransactionHash` -> `TxLocation`
    ///
    /// Maps a transaction id to its on-chain `TxLocation`, giving O(log n) previous-output
    /// resolution instead of a full scan of the height-keyed `txids` table.
    txid_location: Database,

    /// Finalised txout-set accumulator:
    /// `"tx_out_set_info_accumulator"` -> `FinalisedTxOutSetInfoAccumulator`.
    ///
    /// Stores the finalised-state portion of `gettxoutsetinfo` that can be maintained cheaply
    /// without adding per-UTXO storage.
    tx_out_set_info_accumulator: Database,

    /// Transparent address history: `AddrScript` -> duplicate values of `AddrEventBytes`.
    ///
    /// Stored as an LMDB `DUP_SORT | DUP_FIXED` database keyed by address script bytes. Each duplicate
    /// value is a fixed-size entry encoding one address event (mined output or spending input),
    /// including flags.
    ///
    /// Used to search all transparent address indexes (txids, utxos, balances, deltas)
    #[cfg(feature = "transparent_address_history_experimental")]
    address_history: Database,

    /// Metadata: singleton entry "metadata" -> `DbMetadata`
    metadata: Database,

    /// Background maintenance task handle.
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
    config: StoreSettings,

    /// How many times this backend has looked up its tip height, so a test can count the transactions a read costs.
    #[cfg(test)]
    tip_lookups: std::sync::atomic::AtomicUsize,
}

/// Inherent implementation for [`DbV1`].
///
/// This block contains:
/// - environment / database setup (`spawn`, `open_or_create_db`, schema checks),
/// - background maintenance task management,
/// - write/delete operations for finalised blocks,
/// - read fetchers used by the capability trait implementations, and
/// - internal indexing helpers.
impl DbV1 {
    /// Opens the v1 database without starting the maintenance task, moving it aside and creating a fresh one when its stored schema is not this build's.
    pub(crate) async fn spawn(config: &StoreSettings) -> Result<Self, StoreError> {
        let zaino_db = Self::open_env_and_dbs(config).await?;
        let check = zaino_db.check_schema_version().await?;
        let Some(stale_dir_name) = check.stale_dir_name() else {
            return Ok(zaino_db);
        };

        let db_path = db_path(config);
        let stale_path = db_path.with_file_name(stale_dir_name);
        if stale_path.exists() {
            return Err(StoreError::Custom(format!(
                "the database at {} was written by another build, and {} already holds one \
                 moved aside for the same reason; remove or move that directory before starting",
                db_path.display(),
                stale_path.display()
            )));
        }
        warn!(
            path = %db_path.display(),
            stale = %stale_path.display(),
            ?check,
            "stored schema is not this build's; moving the database aside and resyncing from the validator"
        );
        drop(zaino_db);
        fs::rename(&db_path, &stale_path)?;

        let zaino_db = Self::open_env_and_dbs(config).await?;
        match zaino_db.check_schema_version().await? {
            SchemaCheck::Matches => Ok(zaino_db),
            other => Err(StoreError::Custom(format!(
                "a freshly created database at {} does not carry this build's schema: {other:?}",
                db_path.display()
            ))),
        }
    }

    /// Opens the LMDB environment and every V1 named database as an unstarted, unvalidated [`DbV1`].
    async fn open_env_and_dbs(config: &StoreSettings) -> Result<Self, StoreError> {
        info!("Launching FinalisedState");

        // Prepare database details and path.
        let db_size_bytes = config.db.size().to_byte_count();
        let db_path = db_path(config);
        if !db_path.exists() {
            fs::create_dir_all(&db_path)?;
        }
        // Fixed for the env's lifetime → published here, not per commit
        metrics::gauge!(crate::metric_names::DB_MAP_SIZE_BYTES).set(db_size_bytes as f64);

        // LMDB reader slots, from CPU count, clamped to [MIN_LMDB_READERS, MAX_LMDB_READERS].
        //
        // A slot is one cache line: the measured `lock.mdb` is 32,896 bytes at 512 readers, so
        // 64B per slot plus a small header. 8192 readers costs ~512 KiB of shared memory, which
        // is why the ceiling is set by how many concurrent clients we intend to serve rather
        // than by memory.
        //
        // The bound is a real serving limit, not a tuning hint. `NO_TLS` is set below, so a slot
        // belongs to a read *transaction* rather than a thread: every concurrent read holds one,
        // and exhausting the table fails reads with `MDB_READERS_FULL`. The old ceiling of 4096
        // with a floor of 512 gave exactly 512 on any host with 16 cores or fewer — low enough
        // that ordinary concurrent load exhausted it.
        //
        // Raising this does not make exhaustion safe to hit. A client can still open more
        // concurrent reads than there are slots.
        let cpu_cnt = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let max_readers = u32::try_from((cpu_cnt * 32).clamp(MIN_LMDB_READERS, MAX_LMDB_READERS))
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
        // sync safely re-does. (On NFS / overlay filesystems or a hard pod eviction that
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
        let headers = schema::HEADERS.open(&env).await?;
        let txids = schema::TXIDS.open(&env).await?;
        let transparent = schema::TRANSPARENT.open(&env).await?;
        let sapling = schema::SAPLING.open(&env).await?;
        let orchard = schema::ORCHARD.open(&env).await?;
        let ironwood = schema::IRONWOOD.open(&env).await?;
        let commitment_tree_data = schema::COMMITMENT_TREE_DATA.open(&env).await?;
        let heights = schema::HEIGHTS.open(&env).await?;
        let spent = schema::SPENT.open(&env).await?;
        let txid_location = schema::TXID_LOCATION.open(&env).await?;
        let tx_out_set_info_accumulator = schema::TX_OUT_SET_INFO_ACCUMULATOR.open(&env).await?;
        let metadata = schema::METADATA.open(&env).await?;
        #[cfg(feature = "transparent_address_history_experimental")]
        let address_history = schema::ADDRESS_HISTORY.open(&env).await?;

        Ok(Self {
            tx_out_set_info_accumulator,
            env: Arc::new(env),
            headers,
            txids,
            transparent,
            sapling,
            orchard,
            ironwood,
            commitment_tree_data,
            heights,
            spent,
            txid_location,
            #[cfg(feature = "transparent_address_history_experimental")]
            address_history,
            metadata,
            db_handler: std::sync::Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("FinalisedState", StatusType::Spawning),
            config: config.clone(),
            #[cfg(test)]
            tip_lookups: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// How many tip-height lookups this backend has made so far.
    #[cfg(test)]
    pub(crate) fn tip_lookups(&self) -> usize {
        self.tip_lookups.load(std::sync::atomic::Ordering::Relaxed)
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
            db_handler: std::sync::Mutex::new(None),
            cancel_token: self.cancel_token.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
            #[cfg(test)]
            tip_lookups: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    // *** Internal Control Methods ***

    /// Stores a new runtime status, so background work can report a failure after spawn returns.
    pub(in crate::store) fn store_status(&self, status: StatusType) {
        self.status.store(status);
    }

    /// Marks the database ready and spawns the maintenance task that refreshes gauges, cross-checks the indexes of each stored block, and releases trailing readers until shutdown.
    pub(in crate::store) fn start_maintenance(&self) {
        let zaino_db = self.detached_handle();
        zaino_db.status.store(StatusType::Ready);

        let handle = tokio::spawn(async move {
            let mut maintenance = interval(Duration::from_secs(60));
            let mut next_to_check = GENESIS_HEIGHT;
            while zaino_db.status.load() != StatusType::Closing {
                // Sampled here because a quiet chain writes no blocks to publish them.
                zaino_db.record_db_used_bytes();
                if let Ok(Some(built)) = zaino_db.read_tx_out_set_accumulator_built_height().await {
                    metrics::gauge!(crate::metric_names::SYNC_ACCUMULATOR_HEIGHT)
                        .set(built.0 as f64);
                }

                if let Err(error) = zaino_db.check_indexes_to_tip(&mut next_to_check).await {
                    error!(%error, height = next_to_check.0, "finalised index cross-check failed");
                    zaino_db.status.store(StatusType::CriticalError);
                    return;
                }

                zaino_db.zaino_db_handler_sleep(&mut maintenance).await;
            }
        });

        *self.db_handler.lock().expect("db_handler mutex poisoned") = Some(handle);
    }

    /// Cross-checks the indexes of every stored block from `next` to the tip, advancing `next` past each block that passes.
    async fn check_indexes_to_tip(&self, next: &mut Height) -> Result<(), StoreError> {
        let Some(tip) = self.tip_height().await? else {
            return Ok(());
        };
        while *next <= tip && self.status.load() != StatusType::Closing {
            tokio::task::block_in_place(|| self.check_block_indexes_blocking(*next))?;
            let Some(following) = next.0.checked_add(1) else {
                break;
            };
            *next = Height(following);
        }
        Ok(())
    }

    /// Compares the stored `metadata` record with this build's schema, writing it first when the database is fresh.
    async fn check_schema_version(&self) -> Result<SchemaCheck, StoreError> {
        let this_build = DbMetadata::new(schema::schema_hash()?);
        tokio::task::block_in_place(|| {
            let mut txn = self.env.begin_rw_txn()?;

            match txn.get(self.metadata, &METADATA_KEY) {
                Ok(raw_bytes) => {
                    // A row of another length is another build's layout, whatever its first
                    // 32 bytes decode to; the codec reads a prefix and ignores the rest.
                    if raw_bytes.len() != DbMetadata::ENCODED_LEN {
                        return Ok(SchemaCheck::Unreadable);
                    }
                    return Ok(match DbMetadata::from_bytes(raw_bytes) {
                        Ok(stored) if stored == this_build => SchemaCheck::Matches,
                        Ok(stored) => SchemaCheck::Differs {
                            stored: stored.schema_hash,
                        },
                        Err(_) => SchemaCheck::Unreadable,
                    });
                }
                Err(lmdb::Error::NotFound) => {
                    txn.put(
                        self.metadata,
                        &METADATA_KEY,
                        &this_build.to_bytes()?,
                        WriteFlags::NO_OVERWRITE,
                    )?;
                }
                Err(e) => return Err(StoreError::LmdbError(e)),
            }

            txn.commit()?;
            Ok(SchemaCheck::Matches)
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
