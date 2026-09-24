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
//! - opening or creating the correct backing source (persistent or ephemeral), and rebuilding a
//!   persistent database whose stored metadata does not match this build,
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
//!   - Defines versioned metadata (`DbMetadata`, `DbVersion`) persisted on disk.
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
//! # Ephemeral mode and background sync
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
//!
//! Readiness has two distinct waits: `FinalisedState::wait_until_ready` reflects *serving*
//! readiness (returns once reads can be served, including from a passthrough), whereas
//! `FinalisedState::wait_until_synced` waits for an in-progress background sync to actually
//! finish (the persistent database reaching its target, or a terminal error).
//!
//! Caveat during a large background sync: blocks served by the ephemeral passthrough carry
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
//! Database implementations additionally use the integrity wrappers in `entry` to store values
//! with a BLAKE2b-256 checksum bound to the encoded key (`key || encoded_value`), providing early
//! detection of corruption or key/value mismatches.
//!
//! # On-disk layout and schema identity
//!
//! The database lives in `<path>/<network>/v1/`. Its `metadata` record holds the schema version
//! and schema hash of the build that created it. There are no migrations: when the stored
//! metadata differs from this build's, `spawn` deletes the database directory and resyncs from
//! the validator, whether the stored schema is older or newer.
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
//! - **Change an on-disk encoding:** treat it as a schema change. Bump the schema version, and
//!   every existing database rebuilds on its next start.
//!

// TODO / FIX - REMOVE THIS ONCE CHAININDEX LANDS!
#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod finalised_source;
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
use finalised_source::FinalisedSource;
use reader::*;
use router::Router;
use tracing::{info, instrument};
use zebra_chain::parameters::NetworkKind;

use crate::adapter::domain_block_ref;
use crate::store::router::EphemeralMode;
use crate::types::{AbsoluteChainWork, BlockHash, Height, IndexedBlock, GENESIS_HEIGHT};
use zaino_chain_store::ChainStoreConfig;

use crate::config::{StoreSettings, ZainoDbConfig};
use crate::error::StoreError;
use zaino_status::StatusType;

use std::{sync::Arc, time::Duration};
use tokio::time::{interval, MissedTickBehavior};

/// The activation heights of the three shielded pools whose data [`build_indexed_block_from_source`] assembles, with `None` for a pool the network never activates.
struct PoolActivationHeights {
    sapling: Option<zebra_chain::block::Height>,
    nu5: Option<zebra_chain::block::Height>,
    nu6_3: Option<zebra_chain::block::Height>,
}

/// - Shared by the version probe and the backend that opens one; two copies drift
///   silently onto different networks
pub(super) fn network_dir(kind: NetworkKind) -> &'static str {
    match kind {
        NetworkKind::Mainnet => "mainnet",
        NetworkKind::Testnet => "testnet",
        NetworkKind::Regtest => "regtest",
    }
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
    parent_chainwork: Option<AbsoluteChainWork>,
) -> Result<IndexedBlock<AbsoluteChainWork>, StoreError> {
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
    pub(crate) fn block_work(&self) -> crate::types::SingleBlockWork {
        self.block.header.bits.to_work()
    }

    /// This block's hash, for naming it in an error.
    pub(crate) fn hash(&self) -> crate::types::BlockHash {
        crate::types::BlockHash(self.block.header.hash.into())
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
    parent_chainwork: Option<AbsoluteChainWork>,
) -> Result<IndexedBlock<AbsoluteChainWork>, StoreError> {
    let _assembling = crate::timer::Timer::start(metrics::histogram!(
        crate::metric_names::SYNC_BLOCK_ASSEMBLE_SECONDS
    ));

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
    let _timer = crate::timer::Timer::start(metrics::histogram!(
        crate::metric_names::SYNC_BLOCK_FETCH_SECONDS
    ));
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
    let _timer = crate::timer::Timer::start(metrics::histogram!(
        crate::metric_names::SYNC_TREESTATE_FETCH_SECONDS
    ));
    source
        .get_commitment_tree_roots(block.header.hash)
        .await
        .map_err(|error| StoreError::Source(source_error(error)))
}

/// Accumulates the block's work onto its parent's and builds the stored shape.
pub(crate) fn indexed_block_from_parts(
    block: &zaino_primitives::types::Block,
    tree_roots: &zaino_primitives::types::TreeRoots,
    parent_chainwork: Option<AbsoluteChainWork>,
) -> Result<IndexedBlock<AbsoluteChainWork>, StoreError> {
    let hash = crate::types::BlockHash(block.header.hash.into());
    let chainwork = crate::conversion::chainwork_from_parent(
        block.header.bits.to_work(),
        hash,
        Height(u32::from(block.header.height)),
        parent_chainwork,
    )
    .map_err(conversion_error)?;
    crate::conversion::indexed_block(block, tree_roots, chainwork).map_err(conversion_error)
}

use zaino_chain_store::ChainStoreSource;

use crate::error::{conversion_error, inconsistent, source_error};

// The build-behaviour knobs — how wide a sync runs in the background, how many
// attempts it makes, and how long it waits between them — were constants here.
// They are `ChainStoreConfig` fields now, because they are the same question for
// any store rather than anything about LMDB, and because a deployment that wants
// to change one should not have to rebuild. The defaults are the values these
// constants held, so nothing moves by adopting them.

#[derive(Debug)]
/// Owner-facing handle to the finalised portion of the chain index, whose reads go through [`FinalisedState::to_reader`].
pub struct FinalisedState<T: ChainStoreSource> {
    /// The validator this store builds itself from.
    ///
    /// Owned rather than passed per call, so a consumer driving the store
    /// cannot point it at a different chain part-way through a build. This is
    /// what lets [`zaino_chain_store::ChainStoreIngest::build_to`] take a
    /// target height and nothing else.
    source: Arc<T>,

    /// Capability router that sends requests to the primary backend, or to the ephemeral passthrough during a long sync.
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

/// Lifecycle and core read/write API for the finalised database.
///
/// This `impl` intentionally stays small and policy heavy:
/// - opening, and rebuilding on a schema mismatch, lives in [`FinalisedState::spawn`],
/// - the storage engine details are encapsulated behind [`FinalisedSource`] and the capability traits,
/// - higher-level query routing is provided by [`DbReader`].
impl<T: ChainStoreSource> FinalisedState<T> {
    // ***** DB control *****

    /// Opens the finalised state, creating the database or rebuilding it when its stored schema differs from this build's.
    #[instrument(name = "FinalisedState::spawn", skip(store, db, source))]
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
            let router = Arc::new(Router::new(Arc::new(
                FinalisedSource::spawn_v1(&cfg).await?,
            )));
            router.primary_backend().start_maintenance();

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
    /// - any ephemeral passthrough currently present.
    ///
    /// After this call returns `Ok(())`, database files remain on disk; shutdown does not
    /// delete data.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        self.db.shutdown().await
    }

    /// Returns the runtime status of whichever backend the router currently sends `READ_CORE` to.
    pub fn status(&self) -> StatusType {
        let status = self.db.status();

        // The reliable production hook for the one-shot "online" announcement. The ephemeral-release
        // edge in `Router::release_ephemeral_reference` covers a first sync, but a
        // restart against an already-current database never installs a passthrough at all
        // (`sync_is_long_running` is false), so that edge never fires and nothing would mark the
        // finalised state as live. `Indexer::log_status` polls this every ~10s, and
        // `note_persistent_online` is latched, so the announcement lands exactly once either way.
        //
        // Deliberately not hooked to `wait_until_ready`: despite its name it has no production
        // caller — only tests and the `reader` wrapper use it.
        let mode = self.db.finalised_state_mode();
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

    /// Waits until no background sync is running and the database is `Ready` or `CriticalError`.
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

    /// What the router will currently serve, as the union of the primary and ephemeral routing masks.
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

    /// Returns the backend that serves the requested capability, or [`StoreError::FeatureUnavailable`] when none does.
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
    /// - a full-mode ephemeral reference is active, meaning maintenance currently owns the
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

        // Single-flight: if a background sync is already in progress, this poll is a
        // no-op. The indexer worker calls this method on every poll, so without this guard a
        // long-running background sync would be re-spawned on each iteration, piling up concurrent
        // `write_blocks_to_height` runs that contend on the single LMDB writer and multiply memory
        // until the process is OOM-killed before any batch commits durably — leaving restarts to
        // resume from the snapshot baseline rather than the last synced height (see issue #1261).
        // `has_background_ops` observes an in-flight sync. The running task syncs to the height it was spawned with; if the chain has
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
    pub async fn write_block(&self, b: IndexedBlock<AbsoluteChainWork>) -> Result<(), StoreError> {
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

    /// Re-reads the tip and publishes it as the watermark, after any operation that could move the tip.
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
    /// Returns the internal router, so tests can reach backend-specific methods directly.
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
}
