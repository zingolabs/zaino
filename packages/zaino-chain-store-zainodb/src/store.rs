//! Finalised ChainIndex state (FinalisedState)
//!
//! This module provides `FinalisedState`, the *finalised* portion of the chain index.
//!
//! “Finalised” in this context means: All but the top `OPERATIONAL_NFS_DEPTH` blocks in the blockchain. This
//! follows Zebra's model where a reorg deeper than `MAX_BLOCK_REORG_HEIGHT` would require a complete network restart.
//!
//! `FinalisedState` is a facade over a `FinalisedSource` — the
//! backing implementation that actually serves finalised data. That backing is **not necessarily a
//! database**: it is one of
//! - a versioned, LMDB-backed persistent database (`V1`), or
//! - an **ephemeral** passthrough that serves finalised reads directly from the upstream
//!   [`ChainStoreSource`](zaino_chain_store::ChainStoreSource) and persists nothing
//!   (selected by `StoreSettings::ephemeral`).
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
//! passthrough); it is a distinct, lower layer from the upstream `ChainStoreSource` (the validator /
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
//!     (`finalised_source::v1`), the ephemeral passthrough
//!     (`finalised_source::ephemeral`), and the version-and-mode-erased facade enum
//!     `finalised_source::FinalisedSource` that implements the capability traits.
//!
//! - `router`
//!   - Implements `router::Router`, a capability router that can direct calls to the primary backing
//!     source, or an ephemeral passthrough during
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
//!   └─ Router (capability routing; primary + optional ephemeral passthrough)
//!       └─ FinalisedSource (enum; V1 / Ephemeral; implements core + extension traits)
//!           ├─ finalised_source::v1::DbV1 (current persistent schema; full indices incl. transparent history)
//!           └─ finalised_source::ephemeral::EphemeralFinalisedState (passthrough to the ChainStoreSource)
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
//! - **Ephemeral mode** (`StoreSettings::ephemeral == true`): no persistent database is opened;
//!   the primary backing source is `Ephemeral`, which answers finalised reads straight from the
//!   `ChainStoreSource`. `sync_to_height` is a no-op and `db_height` reports `0`.
//! - **Background sync**: `sync_to_height` runs **inline** for ranges within
//!   `ChainStoreConfig::background_build_threshold` (so a caller that reads straight back
//!   sees the data), and
//!   **spawns** for larger ranges. While a large sync runs, read-only ephemeral routing is installed
//!   so reads are served from the source; the spawned task retries transient failures and escalates
//!   to `StatusType::CriticalError` after `ChainStoreConfig::max_consecutive_failures`
//!   attempts.
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
//! database catches up. The chain head is independent of this: it derives its
//! own window from the chain tip and never reads the finalised state
//! (`MAX_NFS_DEPTH`).
//!
//! # Database types and serialization strategy
//!
//! The finalised database stores **only** types that are explicitly designed for persistence.
//! Concretely, values written into LMDB are composed from the database-serializable types in
//! [`crate::types::db`] (re-exported via [`crate::types`]).
//!
//! All persisted types implement [`zaino_encoding::ZainoVersionedSerde`], which
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
//!   `data.mdb` + `lock.mdb`. This layout is still *detected* so that `spawn` can return a clear
//!   error — v0 is no longer supported, so an on-disk v0 database is rejected rather than opened or
//!   migrated. The operator must remove the directory and resync a v1 database.
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
//! - the migration runs in place on the one database,
//! - the router installs the ephemeral passthrough so reads keep being answered
//!   from the validator while it does,
//! - and the passthrough is released once the work is committed.
//!
//! There is no second database built in parallel and nothing is promoted; see
//! [`migrations`] for why that description used to be here.
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
//!   - `FinalisedState::sync_to_height`: convenience sync loop that fetches blocks from a `ChainStoreSource`
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
//! let db = Arc::new(crate::store::FinalisedState::spawn(cfg, source).await?);
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
//!   bump the DB major version and rebuild.
//!

// TODO / FIX - REMOVE THIS ONCE CHAININDEX LANDS!
#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod finalised_source;
pub(crate) mod migrations;
pub mod reader;
pub(crate) mod router;

/// Which backend is currently answering finalised-state reads.
///
/// Re-exported from the router because it is the one piece of routing state a
/// consumer legitimately needs: an ephemeral passthrough reports
/// [`StatusType::Ready`] exactly as a synced database does, so status alone
/// cannot tell an operator whether the finalised state being queried is the
/// real on-disk index.
pub use router::FinalisedStateMode;

use capability::*;
use finalised_source::{FinalisedSource, VERSION_DIRS};
use migrations::MigrationManager;
use reader::*;
use router::Router;
use tracing::{info, instrument};
use zebra_chain::parameters::NetworkKind;

#[cfg(feature = "prometheus")]
use crate::metric_names::*;

use crate::adapter::domain_block_ref;
use crate::store::{finalised_source::v1::DB_VERSION_V1, router::EphemeralMode};
use crate::types::{BlockHash, ChainWork, Height, IndexedBlock, GENESIS_HEIGHT};
use zaino_chain_store::ChainStoreConfig;

use crate::config::{StoreSettings, ZainoDbConfig};
use crate::error::StoreError;
use zaino_status::StatusType;

use std::{sync::Arc, time::Duration};
use tokio::time::{interval, MissedTickBehavior};

/// The activation heights of the three shielded pools whose data
/// [`build_indexed_block_from_source`] assembles, resolved once per run.
///
/// Both the ingestion loop ([`capability::DbWrite::write_blocks_to_height`]) and the v1.2.1 →
/// v1.3.0 migration backfill need exactly this set of heights; resolving them in one place keeps
/// the two call sites from drifting apart. A `None` height means the pool's network upgrade has no
/// activation height on the given network.
struct PoolActivationHeights {
    sapling: Option<zebra_chain::block::Height>,
    nu5: Option<zebra_chain::block::Height>,
    nu6_3: Option<zebra_chain::block::Height>,
}

impl PoolActivationHeights {
    /// Resolves the Sapling, NU5 (Orchard), and NU6.3 (Ironwood) activation heights on
    /// `zebra_network`.
    fn resolve(zebra_network: &zebra_chain::parameters::Network) -> Self {
        let activation_height = |pool: crate::pool::ShieldedPool| {
            pool.activation_upgrade().activation_height(zebra_network)
        };
        Self {
            sapling: activation_height(crate::pool::ShieldedPool::Sapling),
            nu5: activation_height(crate::pool::ShieldedPool::Orchard),
            nu6_3: activation_height(crate::pool::ShieldedPool::Ironwood),
        }
    }
}

/// Fetches the block at `height_int` from `source` and builds its [`IndexedBlock`], threading
/// `parent_chainwork` into the block's context.
///
/// Shared by every backend's [`capability::DbWrite::write_blocks_to_height`] ingestion loop so the
/// fetch + commitment-tree-root + assembly lives in one place regardless of which backend
/// owns the loop.
///
/// No network parameter. The old path took one to recompute the header's block-commitments
/// field per network upgrade; the domain block carries that field as it was mined, and the two
/// agree for every block that parses. See [`crate::conversion`].
pub(crate) async fn build_indexed_block_from_source<S: ChainStoreSource + ?Sized>(
    source: &S,
    sapling_activation_height: zebra_chain::block::Height,
    nu5_activation_height: Option<zebra_chain::block::Height>,
    nu6_3_activation_height: Option<zebra_chain::block::Height>,
    height_int: u32,
    parent_chainwork: Option<ChainWork>,
) -> Result<IndexedBlock, StoreError> {
    let fetched = fetch_block_for_indexing(source, height_int).await?;
    assemble_indexed_block(
        fetched,
        sapling_activation_height,
        nu5_activation_height,
        nu6_3_activation_height,
        height_int,
        parent_chainwork,
    )
}

/// The two source reads behind one indexed block, kept together.
///
/// Split out of [`build_indexed_block_from_source`] because this half does not depend on
/// `parent_chainwork` and so does not have to run in block order — which is what lets a bulk sync
/// issue many of them at once. It is also where a sync spends nearly all of its CPU: deserialising
/// a block decompresses two Jubjub points per Sapling output (`from_bytes_not_small_order` — a
/// modular square root plus a cofactor multiplication), work Zaino discards, since it keeps only
/// the compact representation. On sandblast-era blocks that dwarfs everything else the sync does.
pub(crate) struct FetchedBlock {
    block: zaino_primitives::types::Block,
    tree_roots: zaino_primitives::types::TreeRoots,
}

impl FetchedBlock {
    /// This block's own proof-of-work contribution.
    ///
    /// Lets a caller fold the cumulative chainwork over a run of already-fetched blocks before
    /// assembling any of them — the fold is the only ordering constraint in block building, and it
    /// is pure integer arithmetic, so it must not hold the expensive conversion in block order.
    pub(crate) fn block_work(&self) -> Result<crate::types::BlockWork, StoreError> {
        let hash = crate::types::BlockHash(self.block.header.hash.into());
        crate::conversion::block_work(self.block.header.bits, hash)
            .map_err(|error| inconsistent(error.to_string()))
    }
}

/// Reads one block and its commitment-tree roots from the source.
pub(crate) async fn fetch_block_for_indexing<S: ChainStoreSource + ?Sized>(
    source: &S,
    height_int: u32,
) -> Result<FetchedBlock, StoreError> {
    let block = fetch_block(source, height_int).await?;
    let tree_roots = fetch_tree_roots(source, &block).await?;
    Ok(FetchedBlock { block, tree_roots })
}

/// Turns a [`FetchedBlock`] into an [`IndexedBlock`], given the chainwork of its parent.
///
/// The order-dependent half: `parent_chainwork` chains each block to the one before it, so this
/// runs in block order even when the fetches above did not. Cheap next to the fetch — no curve
/// arithmetic, just the treestate check, the metadata assembly and the compact-form conversion.
pub(crate) fn assemble_indexed_block(
    fetched: FetchedBlock,
    sapling_activation_height: zebra_chain::block::Height,
    nu5_activation_height: Option<zebra_chain::block::Height>,
    nu6_3_activation_height: Option<zebra_chain::block::Height>,
    height_int: u32,
    parent_chainwork: Option<ChainWork>,
) -> Result<IndexedBlock, StoreError> {
    let FetchedBlock { block, tree_roots } = fetched;

    require_pool_roots(
        &tree_roots,
        PoolActivation {
            sapling: height_int >= sapling_activation_height.0,
            orchard: nu5_activation_height.is_some_and(|activation| height_int >= activation.0),
            ironwood: nu6_3_activation_height.is_some_and(|activation| height_int >= activation.0),
        },
        block.header.hash,
    )?;

    indexed_block_from_parts(&block, &tree_roots, parent_chainwork)
}

/// Which pools are expected to have a commitment tree at a block.
#[derive(Debug, Clone, Copy)]
struct PoolActivation {
    sapling: bool,
    orchard: bool,
    ironwood: bool,
}

/// Rejects a treestate that is missing a root the block's height requires.
///
/// From a pool's activation onward its root is not optional: a source that
/// omits one has answered about a chain this store cannot index, and defaulting
/// the root would write a wrong treestate that no later read could detect.
/// Below activation the pool has no tree yet, so absence is the correct answer.
fn require_pool_roots(
    roots: &zaino_primitives::types::TreeRoots,
    active: PoolActivation,
    hash: zaino_primitives::types::BlockHash,
) -> Result<(), StoreError> {
    let require = |present: bool, is_active: bool, pool: &str| -> Result<(), StoreError> {
        if is_active && !present {
            return Err(inconsistent(format!(
                "missing {pool} commitment tree root for block {hash}"
            )));
        }
        Ok(())
    };

    require(roots.sapling.is_some(), active.sapling, "sapling")?;
    require(roots.orchard.is_some(), active.orchard, "orchard")?;
    require(roots.ironwood.is_some(), active.ironwood, "ironwood")
}

/// The block at `height`, or a source error naming what was asked for.
async fn fetch_block<S: ChainStoreSource + ?Sized>(
    source: &S,
    height: u32,
) -> Result<zaino_primitives::types::Block, StoreError> {
    let height = zaino_primitives::types::Height::try_from(height)
        .map_err(|_| inconsistent(format!("height {height} is above the protocol maximum")))?;
    source
        .get_block(height)
        .await
        .map_err(|error| StoreError::Source(source_error(error)))
}

/// The commitment tree roots after `block`.
///
/// Asked for rather than derived: they are cumulative over the chain, so one
/// block does not determine them.
async fn fetch_tree_roots<S: ChainStoreSource + ?Sized>(
    source: &S,
    block: &zaino_primitives::types::Block,
) -> Result<zaino_primitives::types::TreeRoots, StoreError> {
    source
        .get_commitment_tree_roots(block.header.hash)
        .await
        .map_err(|error| StoreError::Source(source_error(error)))
}

/// Accumulates the block's work onto its parent's and builds the stored shape.
pub(crate) fn indexed_block_from_parts(
    block: &zaino_primitives::types::Block,
    tree_roots: &zaino_primitives::types::TreeRoots,
    parent_chainwork: Option<ChainWork>,
) -> Result<IndexedBlock, StoreError> {
    let hash = crate::types::BlockHash(block.header.hash.into());
    let chainwork =
        crate::conversion::chainwork_from_parent(block.header.bits, hash, parent_chainwork)
            .map_err(|error| inconsistent(error.to_string()))?;
    crate::conversion::indexed_block(block, tree_roots, chainwork)
        .map_err(|error| inconsistent(error.to_string()))
}

use zaino_chain_store::ChainStoreSource;

use crate::error::{inconsistent, source_error};

// The build-behaviour knobs — how wide a sync runs in the background, how many
// attempts it makes, and how long it waits between them — were constants here.
// They are `ChainStoreConfig` fields now, because they are the same question for
// any store rather than anything about LMDB, and because a deployment that wants
// to change one should not have to rebuild. The defaults are the values these
// constants held, so nothing moves by adopting them.

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
/// between a primary database and, during migrations, the ephemeral passthrough.
///
/// Query paths should not call `FinalisedState` methods directly. Instead, construct a [`DbReader`] using
/// [`FinalisedState::to_reader`] and perform all reads via that read-only API. This ensures capability-
/// correct routing (especially during migrations).
///
/// ## Configuration
/// `FinalisedState` stores the [`StoreSettings`] used to:
/// - determine network-specific on-disk paths,
/// - select a target database version (`cfg.db_version`),
/// - and compute per-block metadata (e.g., network selection for `BlockMetadata`).
pub struct FinalisedState<T: ChainStoreSource> {
    /// The validator this store builds itself from.
    ///
    /// Owned rather than passed per call, so a consumer driving the store
    /// cannot point it at a different chain part-way through a build. This is
    /// what lets [`zaino_chain_store::ChainStoreIngest::build_to`] take a
    /// target height and nothing else.
    source: Arc<T>,

    /// Capability router for the active database backend(s).
    ///
    /// - In steady state, all requests route to the primary backend.
    /// - During a migration or a long sync, some or all capabilities route to
    ///   the ephemeral passthrough so reads keep being answered.
    db: Arc<Router<T>>,

    /// Immutable configuration snapshot used for sync and metadata construction.
    cfg: StoreSettings,
}

/// Cloned by hand rather than derived.
///
/// A derived `Clone` would demand `T: Clone`, which a validator is not. Every
/// field here is shared or cheap, and — the part that matters — every clone
/// routes through the *same* [`Router`], which is where all mutable state
/// lives. Two handles to one store, not two stores.
impl<T: ChainStoreSource> Clone for FinalisedState<T> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            db: Arc::clone(&self.db),
            cfg: self.cfg.clone(),
        }
    }
}

/// Re-reads the router's tip and publishes it as the watermark.
///
/// Free rather than a method because the build path reaches it from a static
/// context: `sync_to_height_background` holds the router and no
/// `FinalisedState`, and that is the path the sync worker drives. A method
/// would have left the one caller that most needs it unable to call it, which
/// is how it came to be missing.
///
/// **A failed read leaves the previous watermark standing.** That is the
/// conservative direction: a stale watermark under-claims coverage, where
/// clearing it would make a healthy store look empty and route every read away
/// from a database that holds the answer. Both reads are guarded, not just the
/// first — a tip whose hash cannot be read is a store that has a tip, and
/// saying otherwise would be the very outcome this avoids. Only an *empty*
/// database publishes an empty watermark, because that one is true.
async fn refresh_watermark<T: ChainStoreSource>(router: &Arc<Router<T>>) {
    let tip = match router.db_height().await {
        Ok(Some(height)) => match router.get_block_hash(height).await {
            Ok(Some(hash)) => domain_block_ref(height, hash),
            Ok(None) => {
                tracing::warn!(
                    height = height.0,
                    "finalised store has a tip height with no hash; leaving the watermark standing"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    height = height.0,
                    %error,
                    "finalised store could not read its tip hash; leaving the watermark standing"
                );
                return;
            }
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                %error,
                "finalised store could not read its tip height; leaving the watermark standing"
            );
            return;
        }
    };

    router.publish_watermark(zaino_chain_store::StoreWatermark {
        tip,
        provenance: router.watermark_provenance(),
    });
}

/// Lifecycle, migration control, and core read/write API for the finalised database.
///
/// This `impl` intentionally stays small and policy heavy:
/// - version selection and migration orchestration lives in [`FinalisedState::spawn`],
/// - the storage engine details are encapsulated behind [`FinalisedSource`] and the capability traits,
/// - higher-level query routing is provided by [`DbReader`].
impl<T: ChainStoreSource> FinalisedState<T> {
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
    /// - `cfg.db_version == 1` targets the latest v1 DB version (`DB_VERSION_V1`)..
    /// - Any other value (including the legacy `0`) returns an error.
    ///
    /// ## Migrations
    /// Migrations are invoked only when a database already exists on disk and the opened database
    /// reports a lower version than the configured target.
    ///
    /// Migrations may require access to chain data to rebuild indices. For that reason, a
    /// [`ChainStoreSource`] is provided here and passed into the migration manager.
    ///
    /// ## Errors
    /// Returns [`StoreError`] if:
    /// - the configured target version is unsupported,
    /// - the on-disk database version is unsupported,
    /// - opening or creating the database fails,
    /// - or any migration step fails.
    #[instrument(
        name = "FinalisedState::spawn",
        skip(store, db, source),
        fields(db_version = store.target_schema_major())
    )]
    pub async fn spawn(
        store: ChainStoreConfig,
        db: ZainoDbConfig,
        source: Arc<T>,
    ) -> Result<Self, StoreError> {
        let cfg = StoreSettings::new(store, db);

        // Passthrough is the absence of a path, not a flag beside one: the two
        // cannot contradict each other because there is only one field.
        let Some(db_root) = cfg.store.path().map(std::path::Path::to_path_buf) else {
            // WARN, not INFO: this branch previously returned silently, and a run that serves every
            // finalised-state read from the validator while a test suite believes it is exercising
            // the on-disk index is nearly always a misconfiguration worth surfacing loudly.
            tracing::warn!(
                mode = %FinalisedStateMode::EphemeralConfigured,
                "finalised state running in EPHEMERAL mode (ephemeral_finalised_state = true): no \
                 persistent database will be opened or written, and all finalised-state reads are \
                 served from the backing validator"
            );
            let ephemeral = Arc::new(FinalisedSource::ephemeral(
                Arc::clone(&source),
                cfg.db.network().clone(),
                None,
            ));
            let state = Self {
                source,
                db: Arc::new(Router::new(ephemeral)),
                cfg,
            };
            state.refresh_watermark().await;
            return Ok(state);
        };

        {
            info!(
                mode = %FinalisedStateMode::Persistent,
                path = %db_root.display(),
                "finalised state running in PERSISTENT mode"
            );
            let version_opt = Self::try_find_current_db_version(&cfg).await;

            let target_version = match cfg.store.target_schema_major() {
                1 => DB_VERSION_V1,
                x => {
                    return Err(StoreError::Custom(format!(
                        "unsupported database version: DbV{x}"
                    )));
                }
            };

            let backend = match version_opt {
                Some(version) => {
                    info!(version, "Opening FinalisedState from file");
                    match version {
                        0 => {
                            return Err(StoreError::Custom(format!(
                                "legacy v0 database detected at {}; v0 is no longer supported. \
                                 Remove the directory and restart to resync a v1 database from genesis.",
                                db_root.display()
                            )));
                        }
                        1 => FinalisedSource::spawn_v1(&cfg).await?,
                        _ => {
                            return Err(StoreError::Custom(format!(
                                "unsupported database version: DbV{version}"
                            )));
                        }
                    }
                }
                None => {
                    info!(version = %target_version, "Creating new FinalisedState");
                    match target_version.major() {
                        1 => FinalisedSource::spawn_v1(&cfg).await?,
                        _ => {
                            return Err(StoreError::Custom(format!(
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

                    match migration_manager.migrate().await {
                        Ok(()) => {
                            // Previously only the failure path logged, so a successful migration was
                            // indistinguishable from one still running.
                            info!(
                                from_version = %current_version,
                                to_version = %target_version,
                                "FinalisedState migration complete"
                            );
                            // Start the background validator only now that every migration has
                            // finished: its initial scan reads tables a migration populates (e.g.
                            // `commitment_tree_data_1_3_0`), so starting it earlier would race the
                            // migration and fail on a not-yet-written row.
                            migration_router.primary_backend().start_validator();
                        }
                        Err(error) => {
                            tracing::error!("FinalisedState migration failed: {error}");

                            migration_router.store_primary_status(StatusType::CriticalError);
                        }
                    }
                });
            } else {
                // No migration to run, so the on-disk tables the validator scans are already at the
                // current schema: start it immediately.
                router.primary_backend().start_validator();
            }

            let state = Self {
                source,
                db: router,
                cfg,
            };
            state.refresh_watermark().await;
            Ok(state)
        }
    }

    /// Gracefully shuts down the running database backend(s).
    ///
    /// This delegates to the router, which shuts down:
    /// - the primary backend, and
    /// - any ephemeral passthrough currently present (during migrations).
    ///
    /// After this call returns `Ok(())`, database files may still remain on disk; shutdown does not
    /// delete data. (Deletion of old versions is handled by migrations when applicable.)
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        self.db.shutdown().await
    }

    /// Returns the runtime status of the serving database.
    ///
    /// This status is provided by the backend implementing `capability::DbCore::status`. During
    /// migrations, the router determines which backend serves `READ_CORE`, and the status reflects
    /// that routing decision.
    pub fn status(&self) -> StatusType {
        let status = self.db.status();

        // The reliable production hook for the one-shot "online" announcement. The ephemeral-release
        // edge in `Router::release_ephemeral_reference` covers a first sync or a migration, but a
        // restart against an already-current database never installs a passthrough at all
        // (`sync_is_long_running` is false), so that edge never fires and nothing would mark the
        // finalised state as live. `Indexer::log_status` polls this every ~10s, and
        // `note_persistent_online` is latched, so the announcement lands exactly once either way.
        //
        // Deliberately not hooked to `wait_until_ready`: despite its name it has no production
        // caller — only tests and the `reader` wrapper use it.
        let mode = self.db.finalised_state_mode();

        #[cfg(feature = "prometheus")]
        metrics::gauge!(FINALISED_EPHEMERAL).set(if mode == FinalisedStateMode::Persistent {
            0.0
        } else {
            1.0
        });

        if status == StatusType::Ready && mode == FinalisedStateMode::Persistent {
            self.db.note_persistent_online();
        }

        status
    }

    /// Returns which backend is currently answering finalised-state reads.
    ///
    /// Distinct from [`FinalisedState::status`]: an ephemeral passthrough reports
    /// [`StatusType::Ready`] just like a synced persistent database, so `status` alone cannot tell a
    /// caller whether the finalised state it is querying is the real on-disk index.
    pub fn finalised_state_mode(&self) -> FinalisedStateMode {
        self.db.finalised_state_mode()
    }

    /// Waits until the database reports [`StatusType::Ready`].
    ///
    /// This polls the router at a fixed interval (100ms) using a Tokio timer. The polling loop uses
    /// `MissedTickBehavior::Delay` to avoid catch-up bursts under load or when the runtime is
    /// stalled.
    ///
    /// Call this after [`FinalisedState::spawn`] if downstream services require the database to be fully
    /// initialised before handling requests.
    pub async fn wait_until_ready(&self) {
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
    pub async fn wait_until_synced(&self) {
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

    /// The finalised watermark, as last published.
    pub(crate) fn watermark(&self) -> zaino_chain_store::StoreWatermark {
        self.db.watermark()
    }

    /// Watches the finalised watermark.
    pub(crate) fn subscribe_watermark(
        &self,
    ) -> tokio::sync::watch::Receiver<zaino_chain_store::StoreWatermark> {
        self.db.subscribe_watermark()
    }

    /// What the router will currently serve.
    ///
    /// The union of the primary and ephemeral masks rather than the primary
    /// backend's own set: during a migration some capabilities route to the
    /// passthrough backend, and what a consumer can ask for is what is routed,
    /// not what the primary happens to hold.
    pub(crate) fn capability(&self) -> crate::store::capability::Capability {
        self.db.service_capability()
    }

    /// Builds up to and including `target`, using the store's own validator.
    pub async fn build_to(&self, target: Height) -> Result<(), StoreError> {
        let source = Arc::clone(&self.source);
        self.sync_to_height(target, &source).await
    }

    /// Discards every block above `height`.
    ///
    /// Deletes from the tip downwards, one block at a time, because the writer
    /// is append-only and its secondary indexes can only be reversed in the
    /// order they were built. A repair path, not part of following the chain:
    /// a reorg deep enough to reach the finalised state is outside the window
    /// the chain head covers.
    pub async fn rewind_to(&self, height: Height) -> Result<(), StoreError> {
        while let Some(tip) = self.db.db_height().await? {
            if tip.0 <= height.0 {
                break;
            }
            self.db.delete_block_at_height(tip).await?;
        }
        self.refresh_watermark().await;
        Ok(())
    }

    /// Creates a read-only view onto the running database.
    ///
    /// All chain fetches should be performed through [`DbReader`] rather than calling read methods
    /// directly on `FinalisedState`.
    pub fn to_reader(self: &Arc<Self>) -> DbReader<T> {
        DbReader {
            inner: Arc::clone(self),
        }
    }

    /// A reader from a handle that is not already behind an [`Arc`].
    ///
    /// The allocation is per reader, not per read, and it buys nothing shared:
    /// the clone it wraps routes through the same [`Router`] as every other, so
    /// two readers made this way see one store. Exists because the domain's
    /// `reader(&self)` takes a plain reference where [`Self::to_reader`] needs
    /// an `Arc<Self>`.
    pub(crate) fn reader(&self) -> DbReader<T> {
        DbReader {
            inner: Arc::new(self.clone()),
        }
    }

    /// Attempts to detect the current on-disk database version from the filesystem layout.
    ///
    /// The detection is intentionally conservative: it returns the **oldest** detected version,
    /// because the process may have been terminated mid-migration, leaving both an older primary
    /// and a newer partially-migrated directory on disk.
    ///
    /// ## Recognised layouts
    ///
    /// - **Legacy v0 layout**
    ///   - Network directories: `live/`, `test/`, `local/`
    ///   - Presence check: both `data.mdb` and `lock.mdb` exist
    ///   - Reported version: `Some(0)`. v0 is no longer supported, so `spawn` rejects this with a
    ///     clear error rather than opening or migrating it; detection exists only to produce that
    ///     error.
    ///
    /// - **Versioned v1+ layout**
    ///   - Network directories: `mainnet/`, `testnet/`, `regtest/`
    ///   - Version subdirectories: enumerated by `finalised_source::VERSION_DIRS` (e.g. `"v1"`)
    ///   - Presence check: both `data.mdb` and `lock.mdb` exist within a version directory
    ///   - Reported version: `Some(i + 1)` where `i` is the index in `VERSION_DIRS`
    ///
    /// Returns:
    /// - `Some(version)` if a compatible database directory is found,
    /// - `None` if no database is detected (fresh DB creation case), and for a
    ///   store that holds nothing: there is no directory to look in, and so no
    ///   version to find.
    async fn try_find_current_db_version(cfg: &StoreSettings) -> Option<u32> {
        let db_root = cfg.store.path()?;
        let legacy_dir = match cfg.db.network().kind() {
            NetworkKind::Mainnet => "live",
            NetworkKind::Testnet => "test",
            NetworkKind::Regtest => "local",
        };
        let legacy_path = db_root.join(legacy_dir);
        if legacy_path.join("data.mdb").exists() && legacy_path.join("lock.mdb").exists() {
            return Some(0);
        }

        let net_dir = match cfg.db.network().kind() {
            NetworkKind::Mainnet => "mainnet",
            NetworkKind::Testnet => "testnet",
            NetworkKind::Regtest => "regtest",
        };
        let net_path = db_root.join(net_dir);
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
    /// The router may return either the primary or the ephemeral backend depending on the current routing
    /// masks.
    ///
    /// ## Errors
    /// Returns [`StoreError::FeatureUnavailable`] if neither backend currently serves the
    /// requested capability.
    #[inline]
    pub(crate) fn backend_for_cap(
        &self,
        cap: CapabilityRequest,
    ) -> Result<Arc<FinalisedSource<T>>, StoreError> {
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
    /// If the requested sync range is more than `background_build_threshold` blocks ahead of the
    /// current persistent database height, the sync runs in the **background**: read-only ephemeral
    /// routing is installed for its duration (keeping finalised-state reads served by the source while
    /// normal routed writes continue appending to primary), and this method returns immediately.
    /// Completion can be awaited via `FinalisedState::wait_until_synced`.
    ///
    /// If the requested sync range is within `background_build_threshold`, the sync runs **inline**
    /// and this method only returns once every block has been written, so callers that read straight
    /// back (e.g. ChainIndex NFS initialisation) observe the data.
    pub async fn sync_to_height(&self, height: Height, source: &Arc<T>) -> Result<(), StoreError>
    where
        T: Send + Sync + 'static,
    {
        if self.db.primary_is_ephemeral() {
            return Ok(());
        }

        if self.db.has_full_ephemeral_reference() {
            return Ok(());
        }

        // Single-flight: if a background sync (or migration) is already in progress, this poll is a
        // no-op. The indexer worker calls this method on every poll, so without this guard a
        // long-running background sync would be re-spawned on each iteration, piling up concurrent
        // `write_blocks_to_height` runs that contend on the single LMDB writer and multiply memory
        // until the process is OOM-killed before any batch commits durably — leaving restarts to
        // resume from the snapshot baseline rather than the last synced height (see issue #1261).
        // `has_background_ops` is the union of sync and migration; migrations are already excluded
        // above via `has_full_ephemeral_reference`, so the only thing this observes here is an
        // in-flight sync. The running task syncs to the height it was spawned with; if the chain has
        // advanced past it, the next poll after it completes spawns a fresh sync to the new target.
        if self.db.has_background_ops() {
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
            height.0.saturating_sub(db_height.0) > self.cfg.store.background_build_threshold();

        let max_attempts = self.cfg.store.max_consecutive_failures();
        let retry_backoff = self.cfg.store.retry_backoff();
        let router = Arc::clone(&self.db);
        let cfg = self.cfg.clone();
        let source = Arc::clone(source);

        if sync_is_long_running {
            // Register the background sync in the foreground, before spawning, so `wait_until_synced`
            // cannot observe a "no work in progress" state between this method returning and the
            // spawned task starting. The guard is moved into the task and drops when it completes.
            let op_guard = router.begin_background_op();

            let ephemeral_reference = router
                .init_or_take_ephemeral(
                    source.clone(),
                    cfg.db.network().clone(),
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
                            if attempt >= max_attempts {
                                tracing::error!(
                                    "FinalisedState background sync_to_height failed after {attempt} \
                                     attempts, giving up: {error}"
                                );
                                router.store_primary_status(StatusType::CriticalError);
                                return;
                            }
                            tracing::warn!(
                                "FinalisedState background sync_to_height failed (attempt \
                                 {attempt}/{max_attempts}), retrying: {error}"
                            );
                            router.store_primary_status(StatusType::RecoverableError);
                            tokio::time::sleep(retry_backoff).await;
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
        _cfg: StoreSettings,
        height: Height,
        source: Arc<T>,
    ) -> Result<(), StoreError>
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
        let result = router.write_blocks_to_height(height, source.as_ref()).await;

        if result.is_ok() {
            // Keep the ephemeral passthrough's reported finalised height in step with the primary
            // once the batch lands, so reads routed through a ReadOnly ephemeral reference observe
            // catch-up progress.
            router.update_ephemeral_db_height(Some(height))?;

            // The env is opened with `NO_SYNC`, so the blocks written above are committed but may
            // not be on disk yet. Force a durability checkpoint so a `sync_to_height` that returns
            // `Ok` is guaranteed durable; a later crash can only roll back to this height.
            let env = router.backend(CapabilityRequest::WriteCore)?.env()?;
            tokio::task::block_in_place(|| env.sync(true)).map_err(StoreError::LmdbError)?;

            // Publish the height this run reached. Without it the watermark
            // would only ever be what `spawn` published, which on a store that
            // was empty at startup is nothing at all — so every read bounded by
            // the watermark would refuse forever while the database filled up
            // behind it. `write_block` and `rewind_to` publish for the same
            // reason; this is the path the sync worker actually drives, and it
            // was the one not doing it.
            //
            // Once per completed run, not once per batch: a long catch-up
            // serves its reads through the ephemeral passthrough it holds a
            // reference to, so the watermark standing still for the duration is
            // the correct description of what the primary can answer.
            refresh_watermark(&router).await;
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
    pub async fn write_block(&self, b: IndexedBlock) -> Result<(), StoreError> {
        self.db.write_block(b).await?;
        self.refresh_watermark().await;
        Ok(())
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
    pub async fn delete_block_at_height(&self, h: Height) -> Result<(), StoreError> {
        self.db.delete_block_at_height(h).await?;
        self.refresh_watermark().await;
        Ok(())
    }

    /// Deletes the provided block from the database.
    ///
    /// This **must** be the current database tip. The provided [`IndexedBlock`] is used to ensure
    /// all derived indices created by that block can be removed deterministically.
    ///
    /// Prefer [`FinalisedState::delete_block_at_height`] when possible; use this method when the backend
    /// requires full block contents to correctly reverse all indices.
    pub(crate) async fn delete_block(&self, b: &IndexedBlock) -> Result<(), StoreError> {
        self.db.delete_block(b).await?;
        self.refresh_watermark().await;
        Ok(())
    }

    /// Re-reads the tip and publishes it as the watermark.
    ///
    /// Called after every operation that could move the tip, rather than having
    /// each of them compute the new value: the operations that change a height
    /// are spread across the writer, the migration path and the ephemeral
    /// lifecycle, and a publish site that has to be remembered at each one is a
    /// publish site that gets forgotten. Re-reading costs two indexed lookups
    /// and happens per batch, not per block.
    pub(crate) async fn refresh_watermark(&self) {
        refresh_watermark(&self.db).await;
    }

    // ***** DB Core Read *****

    /// Returns the highest block height stored in the finalised database.
    ///
    /// Returns:
    /// - `Ok(Some(height))` if at least one block is present,
    /// - `Ok(None)` if the database is empty.
    pub async fn db_height(&self) -> Result<Option<Height>, StoreError> {
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
    ) -> Result<Option<Height>, StoreError> {
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
    ) -> Result<Option<BlockHash>, StoreError> {
        self.db.get_block_hash(height).await
    }

    /// Returns the persisted database metadata.
    ///
    /// See `capability::DbMetadata` for the precise fields and on-disk encoding.
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, StoreError> {
        self.db.get_metadata().await
    }
}

#[cfg(test)]
impl<T: ChainStoreSource> FinalisedState<T> {
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

    /// Shared handle to the router, for tests that need to drive ephemeral routing transitions
    /// directly (`init_or_take_ephemeral` takes `&Arc<Router<T>>`).
    ///
    /// Exercising those transitions through `sync_to_height` instead would race the spawned
    /// background task, so the deterministic routing tests reach for this.
    #[cfg(test)]
    pub(crate) fn router_arc(&self) -> &Arc<Router<T>> {
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
        cfg: StoreSettings,
        source: Arc<T>,
        target_version: DbVersion,
    ) -> Result<Self, StoreError> {
        if target_version.major() > DB_VERSION_V1.major() {
            return Err(StoreError::Custom(format!(
                "unsupported database version: {target_version}"
            )));
        }
        if target_version.major() == DB_VERSION_V1.major() && target_version > DB_VERSION_V1 {
            return Err(StoreError::Custom(format!(
                "unsupported database version: {target_version}"
            )));
        }

        let version_opt = Self::try_find_current_db_version(&cfg).await;

        let backend = match version_opt {
            Some(version) => {
                info!(version, "Opening FinalisedState from file");
                match version {
                    1 => FinalisedSource::spawn_v1(&cfg).await?,
                    _ => {
                        return Err(StoreError::Custom(format!(
                            "unsupported database version: DbV{version}"
                        )));
                    }
                }
            }
            None => {
                return Err(StoreError::Custom(
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
                source: Arc::clone(&source),
            };
            migration_manager.migrate().await?;
        }

        // This test helper builds a fixture at an arbitrary (often intermediate) version for
        // inspection, so it deliberately does NOT start the validator: the validator only validates
        // against the current schema and would fail on an intermediate-version database. The
        // foreground migration is already complete, so mark the primary `Ready` directly (as
        // `spawn_v1_0_0` does) to give callers a settled backend. Validation is exercised through
        // the production `FinalisedState::spawn` path, which always targets the current schema.
        router.store_primary_status(StatusType::Ready);

        let metadata = router.get_metadata().await?;
        if metadata.version() != target_version {
            return Err(StoreError::Custom(format!(
                "database version mismatch after test spawn: expected {}, found {}",
                target_version,
                metadata.version()
            )));
        }

        let state = Self {
            source,
            db: router,
            cfg,
        };
        state.refresh_watermark().await;
        Ok(state)
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
        cfg: &StoreSettings,
        source: Arc<T>,
    ) -> Result<FinalisedSource<T>, StoreError> {
        let db = FinalisedSource::spawn_v1_0_0(cfg).await?;

        let tip = source
            .get_best_block_height()
            .await
            .map_err(|error| StoreError::Source(source_error(error)))?;
        let tip = Height(u32::from(tip));

        // Ironwood (NU6.3) commitment tree data is only expected from activation. Below activation
        // (or on a network with no NU6.3 activation height) the source has no ironwood root, so it
        // defaults — mirroring `build_indexed_block_from_source`.
        let nu6_3_activation_height = crate::pool::ShieldedPool::Ironwood
            .activation_upgrade()
            .activation_height(cfg.db.network());

        let mut parent_chainwork: Option<ChainWork> = None;

        for height in crate::types::GENESIS_HEIGHT.0..=tip.0 {
            let block = fetch_block(source.as_ref(), height).await?;
            let tree_roots = fetch_tree_roots(source.as_ref(), &block).await?;

            // Per this builder's contract, the fixture source provides Sapling and Orchard
            // roots for every block, so those pools are unconditionally required.
            require_pool_roots(
                &tree_roots,
                PoolActivation {
                    sapling: true,
                    orchard: true,
                    ironwood: nu6_3_activation_height
                        .is_some_and(|activation| height >= activation.0),
                },
                block.header.hash,
            )?;

            let chain_block = indexed_block_from_parts(&block, &tree_roots, parent_chainwork)?;
            parent_chainwork = Some(chain_block.context.chainwork);

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
        cfg: StoreSettings,
        source: Arc<T>,
        target_version: DbVersion,
    ) -> Result<Self, StoreError> {
        let v1_0_0 = DbVersion::new(1, 0, 0);
        if target_version < v1_0_0 {
            return Err(StoreError::Custom(format!(
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
