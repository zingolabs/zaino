//! Finalised ChainIndex state (FinalisedState)
//!
//! This module provides `FinalisedState`, the *finalised* portion of the chain index.
//!
//! “Finalised” in this context means: All but the top 100 blocks in the blockchain. This follows
//! Zebra's model where a reorg of depth greater than 100 would require a complete network restart.
//!
//! `FinalisedState` is a facade over a `FinalisedSource` — the
//! backing implementation that actually serves finalised data. That backing is **not necessarily a
//! database**: it is one of
//! - a versioned, LMDB-backed persistent database (`V0` / `V1`), or
//! - an **ephemeral** passthrough that serves finalised reads directly from the upstream
//!   [`BlockchainSource`](crate::chain_index::source::BlockchainSource) and persists nothing
//!   (selected by `ChainIndexConfig::ephemeral`).
//!
//! `FinalisedState` is responsible for:
//! - opening or creating the correct backing source (persistent version or ephemeral),
//! - coordinating **database version migrations** when an on-disk version is older than the
//!   configured target — in the **background**, while continuing to serve,
//! - syncing the persistent database up to a target height — in the **background** for large
//!   ranges, while continuing to serve from a ephemeral passthrough,
//! - exposing a small set of core read/write operations to the rest of `chain_index`,
//! - and providing a read-only handle (`DbReader`) that should be used for all chain fetches.
//!
//! Note the naming: `FinalisedSource` is the finalised-state *backing* (persistent or ephemeral
//! passthrough); it is a distinct, lower layer from the upstream `BlockchainSource` (the validator /
//! node connector) that the ephemeral variant passes through to.
//!
//! # Code layout (submodules)
//!
//! The finalised-state subsystem is split into the following files:
//!
//! - `capability`
//!   - Defines the *capability model* used to represent which features a given backing source supports.
//!   - Defines the core traits (`DbRead`, `DbWrite`, `DbCore`) and extension traits
//!     (`BlockCoreExt`, `TransparentHistExt`, etc.).
//!   - Defines versioned metadata (`DbMetadata`, `DbVersion`, `MigrationStatus`) persisted on disk.
//!
//! - `finalised_source`
//!   - Houses the concrete backing implementations: persistent databases by **major** version
//!     (`finalised_source::v0`, `finalised_source::v1`), the ephemeral passthrough
//!     (`finalised_source::ephemeral`), and the version-and-mode-erased facade enum
//!     `finalised_source::FinalisedSource` that implements the capability traits.
//!
//! - `router`
//!   - Implements `router::Router`, a capability router that can direct calls to the primary backing
//!     source, a shadow database during major migrations, or a ephemeral passthrough during
//!     background sync.
//!
//! - `migrations`
//!   - Implements migration orchestration (`MigrationManager`) and concrete migration steps.
//!
//! - `reader`
//!   - Defines `reader::DbReader`, a read-only view that routes each query through the router
//!     using the appropriate capability request.
//!
//! - `entry`
//!   - Defines integrity-preserving wrappers (`StoredEntryFixed`, `StoredEntryVar`) used by
//!     versioned database implementations for checksummed key/value storage.
//!
//! # Architecture overview
//!
//! At runtime the layering is:
//!
//! ```text
//! FinalisedState (facade; owns config; exposes simple methods)
//!   └─ Router (capability routing; primary + optional shadow + optional ephemeral passthrough)
//!       └─ FinalisedSource (enum; V0 / V1 / Ephemeral; implements core + extension traits)
//!           ├─ finalised_source::v0::DbV0 (legacy persistent schema; compact-block streamer)
//!           ├─ finalised_source::v1::DbV1 (current persistent schema; full indices incl. transparent history)
//!           └─ finalised_source::ephemeral::EphemeralFinalisedState (passthrough to the BlockchainSource)
//! ```
//!
//! Consumers should avoid depending on the concrete backing version; they should prefer `DbReader`,
//! which automatically routes each read to a backing source that actually supports the requested
//! feature.
//!
//! # Ephemeral mode and background sync / migration
//!
//! `FinalisedState` never blocks serving on persistence work:
//!
//! - **Ephemeral mode** (`ChainIndexConfig::ephemeral == true`): no persistent database is opened;
//!   the primary backing source is `Ephemeral`, which answers finalised reads straight from the
//!   `BlockchainSource`. `sync_to_height` is a no-op and `db_height` reports `0`.
//! - **Background sync**: `sync_to_height` runs **inline** for ranges within
//!   `LONG_RUNNING_SYNC_THRESHOLD` (so a caller that reads straight back sees the data), and
//!   **spawns** for larger ranges. While a large sync runs, read-only ephemeral routing is installed
//!   so reads are served from the source; the spawned task retries transient failures and escalates
//!   to `StatusType::CriticalError` after `MAX_BACKGROUND_SYNC_RETRIES` attempts.
//! - **Background migration**: a version migration likewise runs in a spawned task while a ephemeral
//!   passthrough serves reads; on failure it sets `StatusType::CriticalError`.
//!
//! Readiness has two distinct waits: `FinalisedState::wait_until_ready` reflects *serving*
//! readiness (returns once reads can be served, including from a passthrough), whereas
//! `FinalisedState::wait_until_synced` waits for in-progress background sync/migration to actually
//! finish (the persistent database reaching its target, or a terminal error).
//!
//! Caveat during a large background sync/migration: blocks served by the ephemeral passthrough carry
//! a chainwork of `0`. This is consistent for the non-finalised state's *relative* fork-choice (every
//! block shares the same baseline) but means absolute chainwork is offset-low until the persistent
//! database catches up and the anchor is re-seeded. See `chain_index::non_finalised_state`
//! (`MAX_NFS_DEPTH`).
//!
//! # Database types and serialization strategy
//!
//! The finalised database stores **only** types that are explicitly designed for persistence.
//! Concretely, values written into LMDB are composed from the database-serializable types in
//! [`crate::chain_index::types::db`] (re-exported via [`crate::chain_index::types`]).
//!
//! All persisted types implement [`crate::chain_index::encoding::ZainoVersionedSerde`], which
//! defines Zaino’s on-disk wire format:
//! - a **one-byte version tag** (`encoding::version::V1`, `V2`, …),
//! - followed by a version-specific body (little-endian unless stated otherwise).
//!
//! This “version-tagged value” model allows individual record layouts to evolve while keeping
//! backward compatibility via `decode_vN` implementations. Any incompatible change to persisted
//! types must be coordinated with the database schema versioning in this module (see
//! `capability::DbVersion`) and, where required, accompanied by a migration (see `migrations`).
//!
//! Database implementations additionally use the integrity wrappers in `entry` to store values
//! with a BLAKE2b-256 checksum bound to the encoded key (`key || encoded_value`), providing early
//! detection of corruption or key/value mismatches.
//!
//! # On-disk layout and version detection
//!
//! Database discovery is intentionally conservative: `try_find_current_db_version` returns the
//! **oldest** detected version, because the process may have been terminated mid-migration, leaving
//! multiple version directories on disk.
//!
//! The current logic recognises two layouts:
//!
//! - **Legacy v0 layout:** network directories `live/`, `test/`, `local/` containing LMDB
//!   `data.mdb` + `lock.mdb`.
//! - **Versioned v1+ layout:** network directories `mainnet/`, `testnet/`, `regtest/` containing
//!   version subdirectories enumerated by `finalised_source::VERSION_DIRS` (e.g. `v1/`).
//!
//! # Versioning and migration strategy
//!
//! `FinalisedState::spawn` selects a **target version** from `BlockCacheConfig::db_version` and compares it
//! against the **current on-disk version** read from `DbMetadata`.
//!
//! - If no database exists, a new DB is created at the configured target version.
//! - If a database exists and `current_version < target_version`, the `migrations::MigrationManager`
//!   is invoked to migrate the database.
//!
//! Major migrations are designed to be low-downtime and disk-conscious:
//! - a *shadow* DB of the new version is built in parallel,
//! - the router continues serving from the primary DB until the shadow is complete,
//! - then the shadow is promoted to primary, and the old DB is deleted once all handles are dropped.
//!
//! Migration progress is tracked via `DbMetadata::migration_status` (see `capability::MigrationStatus`)
//! to support resumption after crashes.
//!
//! **Downgrades are not supported.** If a higher version exists on disk than the configured target,
//! the code currently opens the on-disk DB as-is; do not rely on “forcing” an older version via
//! config.
//!
//! # Core API and invariants
//!
//! `FinalisedState` provides:
//!
//! - Lifecycle:
//!   - `FinalisedState::spawn`, `FinalisedState::shutdown`, `FinalisedState::status`, `FinalisedState::wait_until_ready`
//!
//! - Writes:
//!   - `FinalisedState::write_block`: append-only; **must** write `db_tip + 1`
//!   - `FinalisedState::delete_block_at_height`/`FinalisedState::delete_block`: pop-only; **must** delete tip
//!   - `FinalisedState::sync_to_height`: convenience sync loop that fetches blocks from a `BlockchainSource`
//!
//! - Reads:
//!   - `db_height`, `get_block_height`, `get_block_hash`, `get_metadata`
//!
//! **Write invariants** matter for correctness across all DB versions:
//! - `write_block` must be called in strictly increasing height order and must not skip heights.
//! - `delete_block*` must only remove the current tip, and must keep all secondary indices consistent.
//!
//! # Usage (recommended pattern)
//!
//! - Construct the DB once at startup.
//! - Await readiness.
//! - Hand out `DbReader` handles for all read/query operations.
//!
//! ```rust,no_run
//! use std::sync::Arc;
//!
//! let db = Arc::new(crate::chain_index::finalised_state::FinalisedState::spawn(cfg, source).await?);
//! db.wait_until_ready().await;
//!
//! let reader = db.to_reader();
//! let tip = reader.db_height().await?;
//! ```
//!
//! # Development: extending the finalised DB safely
//!
//! Common tasks and where they belong:
//!
//! - **Add a new query/index:** implement it in the latest DB version (e.g. `finalised_source::v1`), then expose it
//!   via a capability extension trait in `capability`, route it via `reader`, and gate it via
//!   `Capability` / `DbVersion::capability`.
//!
//! - **Add a new DB major version (v2):**
//!   1. Add `db::v2` module and `DbV2` implementation.
//!   2. Extend `finalised_source::FinalisedSource` with a `V2(DbV2)` variant and delegate trait impls.
//!   3. Append `"v2"` to `finalised_source::VERSION_DIRS` (no gaps; order matters for discovery).
//!   4. Extend `FinalisedState::spawn` config mapping to accept `cfg.db_version == 2`.
//!   5. Update `capability::DbVersion::capability` for `(2, 0)`.
//!   6. Add a migration step in `migrations` and register it in `MigrationManager::get_migration`.
//!
//! - **Change an on-disk encoding:** treat it as a schema change. Either implement a migration or
//!   bump the DB major version and rebuild in shadow.
//!

// TODO / FIX - REMOVE THIS ONCE CHAININDEX LANDS!
#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod entry;
pub(crate) mod finalised_source;
pub(crate) mod migrations;
pub(crate) mod reader;
pub(crate) mod router;

use capability::*;
use finalised_source::{FinalisedSource, VERSION_DIRS};
use migrations::MigrationManager;
use reader::*;
use router::Router;
use tracing::{info, instrument};
use zebra_chain::parameters::NetworkKind;

use crate::{
    chain_index::{
        finalised_state::{finalised_source::v1::DB_VERSION_V1, router::EphemeralMode},
        source::BlockchainSourceError,
        types::GENESIS_HEIGHT,
    },
    config::ChainIndexConfig,
    error::FinalisedStateError,
    BlockHash, BlockMetadata, BlockWithMetadata, ChainWork, Height, IndexedBlock, StatusType,
};

use std::{sync::Arc, time::Duration};
use tokio::time::{interval, MissedTickBehavior};

/// Fetches the block at `height_int` from `source` and builds its [`IndexedBlock`], threading
/// `parent_chainwork` into the block metadata.
///
/// Shared by every backend's [`capability::DbWrite::write_blocks_to_height`] ingestion loop so the
/// fetch + commitment-tree-root + metadata assembly lives in one place regardless of which backend
/// owns the loop.
pub(crate) async fn build_indexed_block_from_source<S: BlockchainSource>(
    source: &S,
    network: zaino_common::Network,
    sapling_activation_height: zebra_chain::block::Height,
    nu5_activation_height: Option<zebra_chain::block::Height>,
    height_int: u32,
    parent_chainwork: ChainWork,
) -> Result<IndexedBlock, FinalisedStateError> {
    let block = match source
        .get_block(zebra_state::HashOrHeight::Height(
            zebra_chain::block::Height(height_int),
        ))
        .await?
    {
        Some(block) => block,
        None => {
            return Err(FinalisedStateError::BlockchainSourceError(
                BlockchainSourceError::Unrecoverable(format!(
                    "error fetching block at height {height_int} from validator"
                )),
            ));
        }
    };

    let block_hash = BlockHash::from(block.hash().0);

    // Fetch sapling / orchard commitment tree data if above the relevant network upgrade.
    let (sapling_opt, orchard_opt) = source.get_commitment_tree_roots(block_hash).await?;
    let is_sapling_active = height_int >= sapling_activation_height.0;
    let is_orchard_active = nu5_activation_height
        .is_some_and(|nu5_activation_height| height_int >= nu5_activation_height.0);

    let (sapling_root, sapling_size) = if is_sapling_active {
        sapling_opt.ok_or_else(|| {
            FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                format!("missing Sapling commitment tree root for block {block_hash}"),
            ))
        })?
    } else {
        (zebra_chain::sapling::tree::Root::default(), 0)
    };

    let (orchard_root, orchard_size) = if is_orchard_active {
        orchard_opt.ok_or_else(|| {
            FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                format!("missing Orchard commitment tree root for block {block_hash}"),
            ))
        })?
    } else {
        (zebra_chain::orchard::tree::Root::default(), 0)
    };

    let metadata = BlockMetadata::new(
        sapling_root,
        sapling_size as u32,
        orchard_root,
        orchard_size as u32,
        parent_chainwork,
        network.to_zebra_network(),
    );

    let block_with_metadata = BlockWithMetadata::new(block.as_ref(), metadata);
    IndexedBlock::try_from(block_with_metadata).map_err(|_| {
        FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(format!(
            "error building block data at height {height_int}"
        )))
    })
}

use super::source::BlockchainSource;

/// A sync wider than this many blocks runs in the background (with ephemeral
/// passthrough serving reads); anything within it runs to completion inline so
/// callers that read straight back (e.g. ChainIndex NFS init) observe the data.
const LONG_RUNNING_SYNC_THRESHOLD: u32 = 10;

/// Maximum attempts a background sync makes before escalating to
/// `StatusType::CriticalError`.
const MAX_BACKGROUND_SYNC_RETRIES: u32 = 5;

/// Delay between background-sync retry attempts.
const BACKGROUND_SYNC_RETRY_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Debug)]
/// Handle to the finalised on-disk chain index.
///
/// `FinalisedState` is the owner-facing facade for the finalised portion of the ChainIndex:
/// - it opens or creates the appropriate on-disk database version,
/// - it coordinates migrations when `current_version < target_version`,
/// - and it exposes a small set of lifecycle, write, and core read methods.
///
/// ## Concurrency model
/// Internally, `FinalisedState` holds an [`Arc`] to a [`Router`]. The router provides lock-free routing
/// between a primary database and (during major migrations) an optional shadow database.
///
/// Query paths should not call `FinalisedState` methods directly. Instead, construct a [`DbReader`] using
/// [`FinalisedState::to_reader`] and perform all reads via that read-only API. This ensures capability-
/// correct routing (especially during migrations).
///
/// ## Configuration
/// `FinalisedState` stores the [`ChainIndexConfig`] used to:
/// - determine network-specific on-disk paths,
/// - select a target database version (`cfg.db_version`),
/// - and compute per-block metadata (e.g., network selection for `BlockMetadata`).
pub(crate) struct FinalisedState<T: BlockchainSource> {
    /// Capability router for the active database backend(s).
    ///
    /// - In steady state, all requests route to the primary backend.
    /// - During a major migration, some or all capabilities may route to a shadow backend until
    ///   promotion completes.
    db: Arc<Router<T>>,

    /// Immutable configuration snapshot used for sync and metadata construction.
    cfg: ChainIndexConfig,
}

/// Lifecycle, migration control, and core read/write API for the finalised database.
///
/// This `impl` intentionally stays small and policy heavy:
/// - version selection and migration orchestration lives in [`FinalisedState::spawn`],
/// - the storage engine details are encapsulated behind [`FinalisedSource`] and the capability traits,
/// - higher-level query routing is provided by [`DbReader`].
impl<T: BlockchainSource> FinalisedState<T> {
    // ***** DB control *****

    /// Spawns a `FinalisedState` instance.
    ///
    /// This method:
    /// 1. Detects the on-disk database version (if any) using [`FinalisedState::try_find_current_db_version`].
    /// 2. Selects a target schema version from `cfg.db_version`.
    /// 3. Opens the existing database at the detected version, or creates a new database at the
    ///    target version.
    /// 4. If an existing database is older than the target (`current_version < target_version`),
    ///    runs migrations using `migrations::MigrationManager`.
    ///
    /// ## Version selection rules
    /// - `cfg.db_version == 0` targets `DbVersion { 0, 0, 0 }` (legacy layout).
    /// - `cfg.db_version == 1` targets the latest v1 DB version (`DB_VERSION_V1`)..
    /// - Any other value returns an error.
    ///
    /// ## Migrations
    /// Migrations are invoked only when a database already exists on disk and the opened database
    /// reports a lower version than the configured target.
    ///
    /// Migrations may require access to chain data to rebuild indices. For that reason, a
    /// [`BlockchainSource`] is provided here and passed into the migration manager.
    ///
    /// ## Errors
    /// Returns [`FinalisedStateError`] if:
    /// - the configured target version is unsupported,
    /// - the on-disk database version is unsupported,
    /// - opening or creating the database fails,
    /// - or any migration step fails.
    #[instrument(name = "FinalisedState::spawn", skip(cfg, source), fields(db_version = cfg.db_version))]
    pub(crate) async fn spawn(
        cfg: ChainIndexConfig,
        source: T,
    ) -> Result<Self, FinalisedStateError> {
        if cfg.ephemeral {
            return Ok(Self {
                db: Arc::new(Router::new(Arc::new(FinalisedSource::ephemeral(
                    source,
                    cfg.network.into(),
                    None,
                )))),
                cfg,
            });
        } else {
            let version_opt = Self::try_find_current_db_version(&cfg).await;

            let target_version = match cfg.db_version {
                0 => DbVersion {
                    major: 0,
                    minor: 0,
                    patch: 0,
                },
                1 => DB_VERSION_V1,
                x => {
                    return Err(FinalisedStateError::Custom(format!(
                        "unsupported database version: DbV{x}"
                    )));
                }
            };

            let backend = match version_opt {
                Some(version) => {
                    info!(version, "Opening FinalisedState from file");
                    match version {
                        0 => FinalisedSource::spawn_v0(&cfg).await?,
                        1 => FinalisedSource::spawn_v1(&cfg).await?,
                        _ => {
                            return Err(FinalisedStateError::Custom(format!(
                                "unsupported database version: DbV{version}"
                            )));
                        }
                    }
                }
                None => {
                    info!(version = %target_version, "Creating new FinalisedState");
                    match target_version.major() {
                        0 => FinalisedSource::spawn_v0(&cfg).await?,
                        1 => FinalisedSource::spawn_v1(&cfg).await?,
                        _ => {
                            return Err(FinalisedStateError::Custom(format!(
                                "unsupported database version: DbV{target_version}"
                            )));
                        }
                    }
                }
            };
            let current_version = backend.get_metadata().await?.version();

            let router = Arc::new(Router::new(Arc::new(backend)));

            if version_opt.is_some() && current_version < target_version {
                info!(
                    from_version = %current_version,
                    to_version = %target_version,
                    "Starting FinalisedState migration in background"
                );

                let migration_router = Arc::clone(&router);
                let migration_cfg = cfg.clone();
                let migration_source = source.clone();

                // Register the migration in the foreground, before spawning, so `wait_until_synced`
                // blocks until the background migration completes (or fails). The guard is moved into
                // the task and drops when it finishes, on either path.
                let op_guard = router.begin_background_op();

                tokio::spawn(async move {
                    let _op_guard = op_guard;

                    let mut migration_manager = MigrationManager {
                        router: migration_router.clone(),
                        cfg: migration_cfg,
                        current_version,
                        target_version,
                        source: migration_source,
                    };

                    if let Err(error) = migration_manager.migrate().await {
                        tracing::error!("FinalisedState migration failed: {error}");

                        migration_router.store_primary_status(StatusType::CriticalError);
                    }
                });
            }

            Ok(Self { db: router, cfg })
        }
    }

    /// Gracefully shuts down the running database backend(s).
    ///
    /// This delegates to the router, which shuts down:
    /// - the primary backend, and
    /// - any shadow backend currently present (during migrations).
    ///
    /// After this call returns `Ok(())`, database files may still remain on disk; shutdown does not
    /// delete data. (Deletion of old versions is handled by migrations when applicable.)
    pub(crate) async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.db.shutdown().await
    }

    /// Returns the runtime status of the serving database.
    ///
    /// This status is provided by the backend implementing `capability::DbCore::status`. During
    /// migrations, the router determines which backend serves `READ_CORE`, and the status reflects
    /// that routing decision.
    pub(crate) fn status(&self) -> StatusType {
        self.db.status()
    }

    /// Waits until the database reports [`StatusType::Ready`].
    ///
    /// This polls the router at a fixed interval (100ms) using a Tokio timer. The polling loop uses
    /// `MissedTickBehavior::Delay` to avoid catch-up bursts under load or when the runtime is
    /// stalled.
    ///
    /// Call this after [`FinalisedState::spawn`] if downstream services require the database to be fully
    /// initialised before handling requests.
    pub(crate) async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if self.db.status() == StatusType::Ready {
                break;
            }
        }
    }

    /// Waits until all in-progress background sync/migration work has finished.
    ///
    /// Unlike `FinalisedState::wait_until_ready`, which reflects serving-readiness (the database serves
    /// reads from the source while it syncs/migrates in the background), this waits for the
    /// persistent database to actually reach its sync/migration target. It returns once no background
    /// operation is in progress *and* the database has settled into a terminal serving state.
    ///
    /// Breaking on `StatusType::CriticalError` (as well as [`StatusType::Ready`]) ensures this does
    /// not hang if a background migration fails.
    ///
    /// This polls the router at a fixed interval (100ms) using the same `MissedTickBehavior::Delay`
    /// timer as `FinalisedState::wait_until_ready`.
    pub(crate) async fn wait_until_synced(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !self.db.has_background_ops()
                && matches!(
                    self.db.status(),
                    StatusType::Ready | StatusType::CriticalError
                )
            {
                break;
            }
        }
    }

    /// Creates a read-only view onto the running database.
    ///
    /// All chain fetches should be performed through [`DbReader`] rather than calling read methods
    /// directly on `FinalisedState`.
    pub(crate) fn to_reader(self: &Arc<Self>) -> DbReader<T> {
        DbReader {
            inner: Arc::clone(self),
        }
    }

    /// Attempts to detect the current on-disk database version from the filesystem layout.
    ///
    /// The detection is intentionally conservative: it returns the **oldest** detected version,
    /// because the process may have been terminated mid-migration, leaving both an older primary
    /// and a newer shadow directory on disk.
    ///
    /// ## Recognised layouts
    ///
    /// - **Legacy v0 layout**
    ///   - Network directories: `live/`, `test/`, `local/`
    ///   - Presence check: both `data.mdb` and `lock.mdb` exist
    ///   - Reported version: `Some(0)`
    ///
    /// - **Versioned v1+ layout**
    ///   - Network directories: `mainnet/`, `testnet/`, `regtest/`
    ///   - Version subdirectories: enumerated by `finalised_source::VERSION_DIRS` (e.g. `"v1"`)
    ///   - Presence check: both `data.mdb` and `lock.mdb` exist within a version directory
    ///   - Reported version: `Some(i + 1)` where `i` is the index in `VERSION_DIRS`
    ///
    /// Returns:
    /// - `Some(version)` if a compatible database directory is found,
    /// - `None` if no database is detected (fresh DB creation case).
    async fn try_find_current_db_version(cfg: &ChainIndexConfig) -> Option<u32> {
        let legacy_dir = match cfg.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "live",
            NetworkKind::Testnet => "test",
            NetworkKind::Regtest => "local",
        };
        let legacy_path = cfg.storage.database.path.join(legacy_dir);
        if legacy_path.join("data.mdb").exists() && legacy_path.join("lock.mdb").exists() {
            return Some(0);
        }

        let net_dir = match cfg.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "mainnet",
            NetworkKind::Testnet => "testnet",
            NetworkKind::Regtest => "regtest",
        };
        let net_path = cfg.storage.database.path.join(net_dir);
        if net_path.exists() && net_path.is_dir() {
            for (i, version_dir) in VERSION_DIRS.iter().enumerate() {
                let db_path = net_path.join(version_dir);
                let data_file = db_path.join("data.mdb");
                let lock_file = db_path.join("lock.mdb");
                if data_file.exists() && lock_file.exists() {
                    let version = (i + 1) as u32;
                    return Some(version);
                }
            }
        }

        None
    }

    /// Returns the database backend that should serve the requested capability.
    ///
    /// This is used by [`DbReader`] to route calls to the correct database during major migrations.
    /// The router may return either the primary or shadow backend depending on the current routing
    /// masks.
    ///
    /// ## Errors
    /// Returns [`FinalisedStateError::FeatureUnavailable`] if neither backend currently serves the
    /// requested capability.
    #[inline]
    pub(crate) fn backend_for_cap(
        &self,
        cap: CapabilityRequest,
    ) -> Result<Arc<FinalisedSource<T>>, FinalisedStateError> {
        self.db.backend(cap)
    }

    // ***** Db Core Write *****

    /// Syncs the persistent database up to and including `height`.
    ///
    /// Sync is skipped when:
    /// - the primary backend is ephemeral, meaning there is no persistent database to sync, or
    /// - a full-mode ephemeral reference is active, meaning migration/maintenance currently owns the
    ///   persistent database path, or
    /// - the database is non-empty and already at or above `height`.
    ///
    /// An *empty* database is never treated as already holding genesis: it must always sync so the
    /// origin block is written.
    ///
    /// If the requested sync range is more than `LONG_RUNNING_SYNC_THRESHOLD` blocks ahead of the
    /// current persistent database height, the sync runs in the **background**: read-only ephemeral
    /// routing is installed for its duration (keeping finalised-state reads served by the source while
    /// normal routed writes continue appending to primary), and this method returns immediately.
    /// Completion can be awaited via `FinalisedState::wait_until_synced`.
    ///
    /// If the requested sync range is within `LONG_RUNNING_SYNC_THRESHOLD`, the sync runs **inline**
    /// and this method only returns once every block has been written, so callers that read straight
    /// back (e.g. ChainIndex NFS initialisation) observe the data.
    pub(crate) async fn sync_to_height(
        &self,
        height: Height,
        source: &T,
    ) -> Result<(), FinalisedStateError>
    where
        T: Send + Sync + 'static,
    {
        if self.db.primary_is_ephemeral() {
            return Ok(());
        }

        if self.db.has_full_ephemeral_reference() {
            return Ok(());
        }

        let primary = self.db.primary_backend();
        let db_height_opt = primary.db_height().await?;

        // Short-circuit only when the DB already holds blocks at/above target; an empty DB
        // (`db_height_opt == None`) must still sync so the origin block is written.
        if let Some(existing) = db_height_opt {
            if height <= existing {
                return Ok(());
            }
        }

        let db_height = db_height_opt.unwrap_or(GENESIS_HEIGHT);
        let sync_is_long_running =
            height.0.saturating_sub(db_height.0) > LONG_RUNNING_SYNC_THRESHOLD;

        let router = Arc::clone(&self.db);
        let cfg = self.cfg.clone();
        let source = source.clone();

        if sync_is_long_running {
            // Register the background sync in the foreground, before spawning, so `wait_until_synced`
            // cannot observe a "no work in progress" state between this method returning and the
            // spawned task starting. The guard is moved into the task and drops when it completes.
            let op_guard = router.begin_background_op();

            let ephemeral_reference = router
                .init_or_take_ephemeral(
                    source.clone(),
                    cfg.network.to_zebra_network(),
                    EphemeralMode::ReadOnly,
                    db_height_opt,
                )
                .await?;

            tokio::spawn(async move {
                let _op_guard = op_guard;
                let _ephemeral_reference = ephemeral_reference;

                // Retry transient failures so a background sync does not fail silently; surface a
                // recoverable status between attempts and escalate to a terminal status once the
                // retry budget is exhausted.
                let mut attempt: u32 = 0;
                loop {
                    if router.has_full_ephemeral_reference() {
                        return;
                    }

                    match Self::sync_to_height_background(
                        router.clone(),
                        cfg.clone(),
                        height,
                        source.clone(),
                    )
                    .await
                    {
                        Ok(()) => return,
                        Err(error) => {
                            attempt += 1;
                            if attempt >= MAX_BACKGROUND_SYNC_RETRIES {
                                tracing::error!(
                                    "FinalisedState background sync_to_height failed after {attempt} \
                                     attempts, giving up: {error}"
                                );
                                router.store_primary_status(StatusType::CriticalError);
                                return;
                            }
                            tracing::warn!(
                                "FinalisedState background sync_to_height failed (attempt \
                                 {attempt}/{MAX_BACKGROUND_SYNC_RETRIES}), retrying: {error}"
                            );
                            router.store_primary_status(StatusType::RecoverableError);
                            tokio::time::sleep(BACKGROUND_SYNC_RETRY_BACKOFF).await;
                        }
                    }
                }
            });

            Ok(())
        } else {
            // Short sync: run to completion inline so the written blocks are visible to callers that
            // read straight back. Errors propagate to the caller rather than being swallowed.
            Self::sync_to_height_background(router, cfg, height, source).await
        }
    }

    async fn sync_to_height_background(
        router: Arc<Router<T>>,
        _cfg: ChainIndexConfig,
        height: Height,
        source: T,
    ) -> Result<(), FinalisedStateError>
    where
        T: Send + Sync + 'static,
    {
        if router.primary_is_ephemeral() {
            return Ok(());
        }

        if router.has_full_ephemeral_reference() {
            return Ok(());
        }

        // Ingest the tip->height range via the backend's batched loop (fetch -> build -> write,
        // deferring secondary-index maintenance) rather than a per-block loop here; progress is
        // logged from within that loop. The batched path is what keeps large catch-up syncs off
        // the random-fault cliff (see the `zaino-state` changelog).
        let result = router.write_blocks_to_height(height, &source).await;

        if result.is_ok() {
            // Keep the ephemeral passthrough's reported finalised height in step with the primary
            // once the batch lands, so reads routed through a ReadOnly ephemeral reference observe
            // catch-up progress.
            router.update_ephemeral_db_height(Some(height))?;

            // The env is opened with `NO_SYNC`, so the blocks written above are committed but may
            // not be on disk yet. Force a durability checkpoint so a `sync_to_height` that returns
            // `Ok` is guaranteed durable; a later crash can only roll back to this height.
            let env = router.backend(CapabilityRequest::WriteCore)?.env()?;
            tokio::task::block_in_place(|| env.sync(true))
                .map_err(FinalisedStateError::LmdbError)?;
        }

        result
    }

    /// Appends a single fully constructed [`IndexedBlock`] to the database.
    ///
    /// This **must** be the next block after the current database tip (`db_tip_height + 1`).
    /// Database implementations may assume append-only semantics to maintain secondary index
    /// consistency.
    ///
    /// For reorg handling, callers should delete tip blocks using [`FinalisedState::delete_block_at_height`]
    /// or [`FinalisedState::delete_block`] before re-appending.
    pub(crate) async fn write_block(&self, b: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.db.write_block(b).await
    }

    /// Deletes the block at height `h` from the database.
    ///
    /// This **must** be the current database tip. Deleting non-tip blocks is not supported because
    /// it would require re-writing dependent indices for all higher blocks.
    ///
    /// This method delegates to the backend’s `delete_block_at_height` implementation. If that
    /// deletion cannot be completed correctly (for example, if the backend cannot reconstruct all
    /// derived index entries needed for deletion), callers must fall back to [`FinalisedState::delete_block`]
    /// using an [`IndexedBlock`] fetched from the validator/source to ensure a complete wipe.
    pub(crate) async fn delete_block_at_height(
        &self,
        h: Height,
    ) -> Result<(), FinalisedStateError> {
        self.db.delete_block_at_height(h).await
    }

    /// Deletes the provided block from the database.
    ///
    /// This **must** be the current database tip. The provided [`IndexedBlock`] is used to ensure
    /// all derived indices created by that block can be removed deterministically.
    ///
    /// Prefer [`FinalisedState::delete_block_at_height`] when possible; use this method when the backend
    /// requires full block contents to correctly reverse all indices.
    pub(crate) async fn delete_block(&self, b: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.db.delete_block(b).await
    }

    // ***** DB Core Read *****

    /// Returns the highest block height stored in the finalised database.
    ///
    /// Returns:
    /// - `Ok(Some(height))` if at least one block is present,
    /// - `Ok(None)` if the database is empty.
    pub(crate) async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.db.db_height().await
    }

    /// Returns the main-chain height for `hash` if the block is present in the finalised database.
    ///
    /// Returns:
    /// - `Ok(Some(height))` if the hash is indexed,
    /// - `Ok(None)` if the hash is not present (not an error).
    pub(crate) async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        self.db.get_block_height(hash).await
    }

    /// Returns the main-chain block hash for `height` if the block is present in the finalised database.
    ///
    /// Returns:
    /// - `Ok(Some(hash))` if the height is indexed,
    /// - `Ok(None)` if the height is not present (not an error).
    pub(crate) async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        self.db.get_block_hash(height).await
    }

    /// Returns the persisted database metadata.
    ///
    /// See `capability::DbMetadata` for the precise fields and on-disk encoding.
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.db.get_metadata().await
    }
}

#[cfg(test)]
impl<T: BlockchainSource> FinalisedState<T> {
    /// Returns the internal router.
    ///
    /// This is a test-only escape hatch for unit and integration tests that need direct access to
    /// the routed backend, usually to inspect metadata, validate migration results, or exercise
    /// backend-specific capability methods after a test database has been constructed.
    ///
    /// Production code should use the public `FinalisedState` API instead of depending on the router
    /// directly.
    pub(crate) fn router(&self) -> &Router<T> {
        &self.db
    }

    /// Opens an existing test database and migrates it to `target_version`.
    ///
    /// This helper is intended to be called after a historical fixture database has already been
    /// created on disk, for example by [`FinalisedState::build_clean_v1_0_0`]. It does not create a new
    /// database if none exists. A missing database is treated as a test setup error.
    ///
    /// The method:
    /// - rejects target versions newer than the current compiled [`DB_VERSION_V1`],
    /// - discovers the existing on-disk major database version,
    /// - opens the matching backend implementation,
    /// - reads the precise metadata version stored on disk,
    /// - runs migrations when the stored version is older than `target_version`, and
    /// - verifies that the final metadata version exactly matches `target_version`.
    ///
    /// This is useful when a test needs to start from a known old database version and assert that
    /// migrations stop at a specific target version rather than always migrating to the latest
    /// supported version.
    pub(crate) async fn spawn_with_target_version(
        cfg: ChainIndexConfig,
        source: T,
        target_version: DbVersion,
    ) -> Result<Self, FinalisedStateError> {
        if target_version.major() > DB_VERSION_V1.major() {
            return Err(FinalisedStateError::Custom(format!(
                "unsupported database version: {target_version}"
            )));
        }
        if target_version.major() == DB_VERSION_V1.major() && target_version > DB_VERSION_V1 {
            return Err(FinalisedStateError::Custom(format!(
                "unsupported database version: {target_version}"
            )));
        }

        let version_opt = Self::try_find_current_db_version(&cfg).await;

        let backend = match version_opt {
            Some(version) => {
                info!(version, "Opening FinalisedState from file");
                match version {
                    0 => FinalisedSource::spawn_v0(&cfg).await?,
                    1 => FinalisedSource::spawn_v1(&cfg).await?,
                    _ => {
                        return Err(FinalisedStateError::Custom(format!(
                            "unsupported database version: DbV{version}"
                        )));
                    }
                }
            }
            None => {
                return Err(FinalisedStateError::Custom(
                    "expected existing v1.0.0 migration-test database, found no database"
                        .to_string(),
                ));
            }
        };
        let current_version = backend.get_metadata().await?.version();

        let router = Arc::new(Router::new(Arc::new(backend)));

        if current_version < target_version {
            info!(
                from_version = %current_version,
                to_version = %target_version,
                "Starting FinalisedState migration"
            );
            let mut migration_manager = MigrationManager {
                router: Arc::clone(&router),
                cfg: cfg.clone(),
                current_version,
                target_version,
                source,
            };
            migration_manager.migrate().await?;
        }

        let metadata = router.get_metadata().await?;
        if metadata.version() != target_version {
            return Err(FinalisedStateError::Custom(format!(
                "database version mismatch after test spawn: expected {}, found {}",
                target_version,
                metadata.version()
            )));
        }

        Ok(Self { db: router, cfg })
    }

    /// Builds a clean v1.0.0 database fixture from `source`.
    ///
    /// This helper creates a test-only v1 backend initialized with v1.0.0 metadata, fetches every
    /// block from genesis through the source's best height, converts each block into an
    /// [`IndexedBlock`], and writes it using the v1.0.0 block writer.
    ///
    /// The resulting database is intended to represent a pre-migration v1.0.0 database. Tests should
    /// usually shut it down and reopen it through [`FinalisedState::spawn_with_target_version`] or
    /// [`FinalisedState::build_db_to_version`] to exercise migration behavior.
    ///
    /// The supplied source must provide:
    /// - a best block height,
    /// - every block from genesis through that height, and
    /// - Sapling and Orchard commitment tree roots for each block.
    pub(crate) async fn build_clean_v1_0_0(
        cfg: &ChainIndexConfig,
        source: T,
    ) -> Result<FinalisedSource<T>, FinalisedStateError> {
        let db = FinalisedSource::spawn_v1_0_0(cfg).await?;
        db.wait_until_ready().await;

        let tip = source.get_best_block_height().await?.ok_or_else(|| {
            FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                "source has no best block height".to_string(),
            ))
        })?;
        let tip = Height::from(tip);

        let mut parent_chainwork = ChainWork::from_u256(0.into());

        for height in crate::chain_index::types::GENESIS_HEIGHT.0..=tip.0 {
            let block = source
                .get_block(zebra_state::HashOrHeight::Height(
                    zebra_chain::block::Height(height),
                ))
                .await?
                .ok_or_else(|| {
                    FinalisedStateError::BlockchainSourceError(
                        BlockchainSourceError::Unrecoverable(format!(
                            "source missing block at height {height}"
                        )),
                    )
                })?;

            let block_hash = BlockHash::from(block.hash().0);
            let (sapling_opt, orchard_opt) = source.get_commitment_tree_roots(block_hash).await?;
            let (sapling_root, sapling_size) = sapling_opt.ok_or_else(|| {
                FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                    format!("missing Sapling commitment tree root for block {block_hash}"),
                ))
            })?;
            let (orchard_root, orchard_size) = orchard_opt.ok_or_else(|| {
                FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                    format!("missing Orchard commitment tree root for block {block_hash}"),
                ))
            })?;

            let metadata = BlockMetadata::new(
                sapling_root,
                sapling_size as u32,
                orchard_root,
                orchard_size as u32,
                parent_chainwork,
                cfg.network.to_zebra_network(),
            );
            let block_with_metadata = BlockWithMetadata::new(block.as_ref(), metadata);
            let chain_block = IndexedBlock::try_from(block_with_metadata).map_err(|_| {
                FinalisedStateError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                    format!("error building block data at height {height}"),
                ))
            })?;
            parent_chainwork = chain_block.context.chainwork;

            db.write_block_v1_0_0(chain_block).await?;
        }

        Ok(db)
    }

    /// Builds a v1.0.0 fixture database and migrates it to `target_version`.
    ///
    /// This is the high-level migration-test constructor. It first creates a clean v1.0.0 database
    /// using [`FinalisedState::build_clean_v1_0_0`], shuts that backend down so all LMDB state is flushed
    /// and released, then reopens the same database through [`FinalisedState::spawn_with_target_version`].
    ///
    /// During the reopen step, the stored v1.0.0 metadata is used as the migration starting point
    /// and `target_version` is used as the explicit migration target.
    ///
    /// Use this helper when a test wants a fully initialized [`FinalisedState`] at a specific version after
    /// exercising the migration path from v1.0.0. The target version must be at least v1.0.0 and no
    /// newer than the current compiled [`DB_VERSION_V1`].
    pub(crate) async fn build_db_to_version(
        cfg: ChainIndexConfig,
        source: T,
        target_version: DbVersion,
    ) -> Result<Self, FinalisedStateError> {
        let v1_0_0 = DbVersion::new(1, 0, 0);
        if target_version < v1_0_0 {
            return Err(FinalisedStateError::Custom(format!(
                "target version {} is older than v1.0.0",
                target_version
            )));
        }

        let db = Self::build_clean_v1_0_0(&cfg, source.clone()).await?;
        db.shutdown().await?;
        drop(db);

        Self::spawn_with_target_version(cfg, source, target_version).await
    }
}
