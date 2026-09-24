//! Finalised ChainIndex state (FinalisedState)
//!
//! This module provides `FinalisedState`, the *finalised* portion of the chain index.
//!
//! “Finalised” in this context means: All but the top `OPERATIONAL_NFS_DEPTH` blocks in the blockchain. This
//! follows Zebra's model where a reorg deeper than `MAX_BLOCK_REORG_HEIGHT` would require a complete network restart.
//!
//! `FinalisedState` is a facade over [`finalised_source::v1::DbV1`], the LMDB-backed database.
//! It is responsible for:
//! - opening or creating the database, and rebuilding one whose stored schema hash does not match
//!   this build,
//! - syncing the database up to a target height, in the **background** for large ranges,
//! - exposing a small set of core read/write operations to the rest of `chain_index`,
//! - and providing a read-only handle (`DbReader`) that should be used for all chain fetches.
//!
//! # Code layout (submodules)
//!
//! - `capability` defines the core traits (`DbRead`, `DbWrite`, `DbCore`), the extension traits
//!   (`BlockCoreExt`, `TransparentHistExt`, etc.), and the metadata record (`DbMetadata`).
//! - `finalised_source` houses the database (`finalised_source::v1`) and its shared LMDB
//!   lifecycle.
//! - `reader` defines `reader::DbReader`, the read-only view.
//!
//! # Background sync
//!
//! `sync_to_height` runs **inline** for ranges within
//! `ChainStoreConfig::background_build_threshold` (so a caller that reads straight back sees the
//! data), and **spawns** for larger ranges. The spawned task retries transient failures and
//! escalates to `StatusType::CriticalError` after `ChainStoreConfig::max_consecutive_failures`
//! attempts. Reads during a sync see only the blocks written so far; nothing is served in their
//! place.
//!
//! Readiness has two distinct waits: `FinalisedState::wait_until_ready` returns once the database
//! is open, whereas `FinalisedState::wait_until_synced` waits for an in-progress background sync
//! to finish (the database reaching its target, or a terminal error).
//!
//! # Database types and serialization strategy
//!
//! The finalised database stores **only** types that are explicitly designed for persistence.
//! Concretely, values written into LMDB are composed from the database-serializable types in
//! [`crate::types::db`] (re-exported via [`crate::types`]).
//!
//! All persisted types implement [`crate::codec::DbCodec`], which writes each record's fields
//! with no version tag (little-endian unless stated otherwise).
//!
//! # On-disk layout and schema identity
//!
//! The database lives in `<path>/<network>/v1/`. Its `metadata` record holds the schema hash of
//! the build that created it, computed from the canonical encodings, the tables, and the enabled
//! index features. There are no migrations: when the stored hash differs from this build's,
//! `spawn` deletes the database directory and resyncs from the validator.
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
//!   via an extension trait in `capability` and a method on `reader`. Gate an optional index with
//!   a cargo feature.
//!
//! - **Change an on-disk encoding:** treat it as a schema change. The computed schema hash
//!   changes, the schema hash golden fails until it is updated, and every existing database
//!   rebuilds on its next start.
//!

// TODO / FIX - REMOVE THIS ONCE CHAININDEX LANDS!
#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod finalised_source;
pub mod reader;

use capability::*;
use finalised_source::v1::DbV1;
use reader::*;
use tracing::{info, instrument};
use zebra_chain::parameters::NetworkKind;

use crate::adapter::domain_block_ref;
use crate::types::{AbsoluteChainWork, BlockHash, Height, IndexedBlock, GENESIS_HEIGHT};
use zaino_chain_store::{ChainStoreConfig, Provenance, StoreWatermark};

use crate::config::{StoreSettings, ZainoDbConfig};
use crate::error::StoreError;
use zaino_status::StatusType;

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::watch;
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

    /// The LMDB-backed database.
    db: Arc<DbV1>,

    /// The finalised watermark, as last published.
    watermark: Arc<watch::Sender<StoreWatermark>>,

    /// The number of background syncs in progress, which [`FinalisedState::wait_until_synced`] waits on.
    background_ops: Arc<AtomicUsize>,

    /// Immutable configuration snapshot used for sync and metadata construction.
    cfg: StoreSettings,
}

/// Cloned by hand because a derived `Clone` would demand `T: Clone`, and every clone shares one database, watermark, and background-sync counter.
impl<T: ChainStoreSource> Clone for FinalisedState<T> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            db: Arc::clone(&self.db),
            watermark: Arc::clone(&self.watermark),
            background_ops: Arc::clone(&self.background_ops),
            cfg: self.cfg.clone(),
        }
    }
}

/// Scope guard that counts one background sync from before its task is spawned until the task ends.
struct BackgroundOpGuard {
    /// The counter this guard holds a claim on.
    background_ops: Arc<AtomicUsize>,
}

impl BackgroundOpGuard {
    /// Counts a new background sync, in the caller's task, before the sync is spawned.
    fn begin(background_ops: &Arc<AtomicUsize>) -> Self {
        background_ops.fetch_add(1, Ordering::AcqRel);
        Self {
            background_ops: Arc::clone(background_ops),
        }
    }
}

impl Drop for BackgroundOpGuard {
    fn drop(&mut self) {
        self.background_ops.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Re-reads the database tip and publishes it as the watermark, leaving the previous watermark standing when either read fails.
async fn refresh_watermark(db: &DbV1, watermark: &watch::Sender<StoreWatermark>) {
    let tip = match db.db_height().await {
        Ok(Some(height)) => match db.get_block_hash(height).await {
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

    let refreshed = StoreWatermark {
        tip,
        provenance: Provenance::Durable,
    };
    watermark.send_if_modified(|current| {
        if *current == refreshed {
            false
        } else {
            *current = refreshed;
            true
        }
    });
}

/// Lifecycle and core read/write API for the finalised database.
///
/// This `impl` intentionally stays small and policy heavy:
/// - opening, and rebuilding on a schema mismatch, lives in [`FinalisedState::spawn`],
/// - the storage engine details are encapsulated behind [`DbV1`] and the capability traits,
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
        info!(
            path = %cfg.store.path().display(),
            "opening the finalised state"
        );
        let db = Arc::new(DbV1::spawn(&cfg).await?);
        db.start_maintenance();

        let state = Self {
            source,
            db,
            watermark: Arc::new(watch::Sender::new(StoreWatermark::empty())),
            background_ops: Arc::new(AtomicUsize::new(0)),
            cfg,
        };
        state.refresh_watermark().await;
        Ok(state)
    }

    /// Gracefully shuts down the database, leaving its files on disk.
    pub async fn shutdown(&self) -> Result<(), StoreError> {
        self.db.shutdown().await
    }

    /// Returns the runtime status of the database.
    pub fn status(&self) -> StatusType {
        self.db.status()
    }

    /// Waits until the database reports [`StatusType::Ready`], polling every 100ms.
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

    /// Returns whether a background sync is in progress.
    pub fn is_building(&self) -> bool {
        self.background_ops.load(Ordering::Acquire) != 0
    }

    /// Waits until no background sync is running and the database is `Ready` or `CriticalError`.
    pub async fn wait_until_synced(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !self.is_building()
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
    pub(crate) fn watermark(&self) -> StoreWatermark {
        *self.watermark.borrow()
    }

    /// Watches the finalised watermark.
    pub(crate) fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.watermark.subscribe()
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
    /// the clone it wraps shares the same database as every other, so two
    /// readers made this way see one store. Exists because the domain's
    /// `reader(&self)` takes a plain reference where [`Self::to_reader`] needs
    /// an `Arc<Self>`.
    pub(crate) fn reader(&self) -> DbReader<T> {
        DbReader {
            inner: Arc::new(self.clone()),
        }
    }

    /// Returns the database that serves every read and write.
    pub(crate) fn backend(&self) -> &DbV1 {
        &self.db
    }

    // ***** Db Core Write *****

    /// Syncs the database up to and including `height`, inline within `background_build_threshold` blocks of the tip and in a background task beyond it.
    pub async fn sync_to_height(&self, height: Height, source: &Arc<T>) -> Result<(), StoreError>
    where
        T: Send + Sync + 'static,
    {
        // Single-flight: if a background sync is already in progress, this poll is a
        // no-op. The indexer worker calls this method on every poll, so without this guard a
        // long-running background sync would be re-spawned on each iteration, piling up concurrent
        // `write_blocks_to_height` runs that contend on the single LMDB writer and multiply memory
        // until the process is OOM-killed before any batch commits durably — leaving restarts to
        // resume from the snapshot baseline rather than the last synced height (see issue #1261).
        // `has_background_ops` observes an in-flight sync. The running task syncs to the height it was spawned with; if the chain has
        // advanced past it, the next poll after it completes spawns a fresh sync to the new target.
        if self.is_building() {
            return Ok(());
        }

        let db_height_opt = self.db.db_height().await?;

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
        let db = Arc::clone(&self.db);
        let watermark = Arc::clone(&self.watermark);
        let source = Arc::clone(source);

        if sync_is_long_running {
            // Register the background sync in the foreground, before spawning, so `wait_until_synced`
            // cannot observe a "no work in progress" state between this method returning and the
            // spawned task starting. The guard is moved into the task and drops when it completes.
            let op_guard = BackgroundOpGuard::begin(&self.background_ops);

            tokio::spawn(async move {
                let _op_guard = op_guard;

                // Retry transient failures so a background sync does not fail silently; surface a
                // recoverable status between attempts and escalate to a terminal status once the
                // retry budget is exhausted.
                let mut attempt: u32 = 0;
                loop {
                    match Self::sync_to_height_background(&db, &watermark, height, source.as_ref())
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
                                db.store_status(StatusType::CriticalError);
                                return;
                            }
                            tracing::warn!(
                                "FinalisedState background sync_to_height failed (attempt \
                                 {attempt}/{max_attempts}), retrying: {error}"
                            );
                            db.store_status(StatusType::RecoverableError);
                            tokio::time::sleep(retry_backoff).await;
                        }
                    }
                }
            });

            Ok(())
        } else {
            // Short sync: run to completion inline so the written blocks are visible to callers that
            // read straight back. Errors propagate to the caller rather than being swallowed.
            Self::sync_to_height_background(&db, &watermark, height, source.as_ref()).await
        }
    }

    /// Writes every block from the database tip up to `height`, forces the result to disk, and publishes the new watermark.
    async fn sync_to_height_background(
        db: &DbV1,
        watermark: &watch::Sender<StoreWatermark>,
        height: Height,
        source: &T,
    ) -> Result<(), StoreError> {
        // Ingest the tip->height range via the backend's batched loop (fetch -> build -> write,
        // deferring secondary-index maintenance) rather than a per-block loop here; progress is
        // logged from within that loop. The batched path is what keeps large catch-up syncs off
        // the random-fault cliff (see the `zaino-state` changelog).
        let result = db.write_blocks_to_height(height, source).await;

        if result.is_ok() {
            // The env is opened with `NO_SYNC`, so the blocks written above are committed but may
            // not be on disk yet. Force a durability checkpoint so a `sync_to_height` that returns
            // `Ok` is guaranteed durable; a later crash can only roll back to this height.
            let env = Arc::clone(finalised_source::LmdbLifecycle::env(db));
            tokio::task::block_in_place(|| env.sync(true)).map_err(StoreError::LmdbError)?;

            // Publish the height this run reached. Without it the watermark
            // would only ever be what `spawn` published, which on a store that
            // was empty at startup is nothing at all — so every read bounded by
            // the watermark would refuse forever while the database filled up
            // behind it. `write_block` and `rewind_to` publish for the same
            // reason; this is the path the sync worker actually drives, and it
            // was the one not doing it.
            refresh_watermark(db, watermark).await;
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
        refresh_watermark(&self.db, &self.watermark).await;
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
