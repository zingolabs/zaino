//! Finalised-state backing sources (FinalisedSource) and version/mode dispatch
//!
//! This file defines the version-and-mode split for the finalised state and provides
//! [`FinalisedSource`], a kind-erased enum used throughout the finalised-state subsystem.
//!
//! Concrete backing implementations live in:
//! - [`v0`]: legacy persistent schema (compact-block streamer)
//! - [`v1`]: current persistent schema (expanded indices and query surface)
//! - [`ephemeral`]: ephemeral passthrough that serves finalised reads directly from the
//!   [`BlockchainSource`](crate::chain_index::source::BlockchainSource) and persists nothing
//!   (used for ephemeral mode and as the passthrough during background sync/migration)
//!
//! `FinalisedSource` delegates the core traits (`DbCore`, `DbRead`, `DbWrite`) and all extension traits
//! to the appropriate concrete implementation. It is the finalised-state *backing*, distinct from the
//! upstream `BlockchainSource` the `Ephemeral` variant passes through to.
//!
//! # Capability model integration
//!
//! Each `FinalisedSource` instance declares its supported [`Capability`] set via `FinalisedSource::capability()`.
//! This must remain consistent with:
//! - [`capability::DbVersion::capability()`] (schema version → capability mapping), and
//! - the extension trait impls in this file (unsupported methods must return `FeatureUnavailable`).
//!
//! In particular:
//! - v0 supports READ/WRITE core + `CompactBlockExt`.
//! - v1 supports the full current capability set (`Capability::LATEST`), including:
//!   - block header/txid/location indexing,
//!   - transparent + shielded compact tx access,
//!   - indexed block retrieval,
//!   - transparent address history indices.
//!
//! # On-disk directory layout (v1+)
//!
//! [`VERSION_DIRS`] enumerates the version subdirectory names used for versioned layouts under the
//! per-network directory (`mainnet/`, `testnet/`, `regtest/`).
//!
//! **Important:** new versions must be appended to `VERSION_DIRS` in order, with no gaps, because
//! discovery code assumes index+1 corresponds to the version number.
//!
//! # Adding a new major version (v2) — checklist
//!
//! 1. Create `finalised_source::v2` and implement `DbV2::spawn(cfg)`.
//! 2. Add `V2(DbV2)` variant to [`FinalisedSource`].
//! 3. Add `spawn_v2` constructor.
//! 4. Append `"v2"` to [`VERSION_DIRS`].
//! 5. Extend all trait delegation `match` arms in this file.
//! 6. Update `FinalisedSource::capability()` and `DbVersion::capability()` for the new version.
//! 7. Add a migration step in `migrations.rs` and register it with `MigrationManager`.
//!
//! # Development: adding new indices/queries
//!
//! Prefer implementing new indices in the latest DB version first (e.g. `v1`) and exposing them via:
//! - a capability bit + extension trait in `capability.rs`,
//! - routing via `DbReader` and `Router`,
//! - and a migration/rebuild plan if the index requires historical backfill.
//!
//! Keep unsupported methods explicit: if a DB version does not provide a feature, return
//! `FinalisedStateError::FeatureUnavailable(...)` rather than silently degrading semantics.

pub(crate) mod ephemeral;
pub(crate) mod v0;
pub(crate) mod v1;

use v0::DbV0;
use v1::DbV1;
use zaino_proto::proto::utils::PoolTypeFilter;

use crate::{
    chain_index::{
        finalised_state::{
            capability::{
                BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbCore,
                DbMetadata, DbRead, DbWrite, IndexedBlockExt, TransparentHistExt,
            },
            finalised_source::ephemeral::EphemeralFinalisedState,
        },
        types::{db::metadata::FinalisedTxOutSetInfoAccumulator, TransactionHash},
    },
    config::ChainIndexConfig,
    error::FinalisedStateError,
    BlockHash, BlockHeaderData, BlockchainSource, CommitmentTreeData, CompactBlockStream, Height,
    IndexedBlock, NamedAtomicStatus, OrchardCompactTx, OrchardTxList, Outpoint, SaplingCompactTx,
    SaplingTxList, StatusType, TransparentCompactTx, TransparentTxList, TxLocation, TxOutCompact,
    TxidList,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::AddrScript;

use async_trait::async_trait;
use lmdb::{Database, DatabaseFlags, Environment};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    task::JoinHandle,
    time::{interval, sleep, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::capability::Capability;

/// Lifecycle scaffolding shared by every `DbVx` finalised-state backend.
///
/// Implementors expose the four shared struct fields via required getters;
/// provided methods cover the duplicated `status()`, `wait_until_ready()`,
/// `shutdown()`, `clean_trailing()`, and the background task's per-iteration
/// `zaino_db_handler_sleep()`.
///
/// Note: This trait ties any DB version that uses it to Lmdb.
/// In the future we may want to support alternative DB backends.
/// When this happens, we will have to lean away from this trait to some extent.
#[async_trait]
pub(super) trait LmdbLifecycle: Sync {
    fn env(&self) -> &Arc<Environment>;
    fn db_handler_slot(&self) -> &Mutex<Option<JoinHandle<()>>>;
    fn cancel_token(&self) -> &CancellationToken;
    fn status_atomic(&self) -> &NamedAtomicStatus;

    fn status(&self) -> StatusType {
        self.status_atomic().load()
    }

    async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if self.status_atomic().load() == StatusType::Ready {
                break;
            }
        }
    }

    async fn clean_trailing(&self) -> Result<(), FinalisedStateError> {
        let txn = self.env().begin_ro_txn()?;
        drop(txn);
        Ok(())
    }

    async fn zaino_db_handler_sleep(&self, maintenance: &mut tokio::time::Interval) {
        tokio::select! {
            _ = sleep(Duration::from_secs(5)) => {},
            _ = maintenance.tick() => {
                if let Err(e) = self.clean_trailing().await {
                    warn!("clean_trailing failed: {}", e);
                }
            }
            _ = self.cancel_token().cancelled() => {},
        }
    }

    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.status_atomic().store(StatusType::Closing);
        self.cancel_token().cancel();

        let taken = self
            .db_handler_slot()
            .lock()
            .expect("db_handler mutex poisoned")
            .take();
        if let Some(mut handle) = taken {
            let timeout = sleep(Duration::from_secs(5));
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
                    warn!("background task didn't exit in time – aborting");
                    handle.abort();
                }
            }
        }

        let _ = self.clean_trailing().await;
        if let Err(e) = self.env().sync(true) {
            warn!("LMDB fsync before close failed: {e}");
        }
        Ok(())
    }
}

/// Open an LMDB database if present, otherwise create it.
pub(super) async fn open_or_create_db(
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

/// Version subdirectory names for versioned on-disk layouts.
///
/// This list defines the supported major-version directory names under a per-network directory.
/// For example, a v1 database is stored under `<network>/v1/`.
///
/// Invariants:
/// - New versions must be appended to this list in order.
/// - There must be no missing versions between entries.
/// - Discovery code assumes `VERSION_DIRS[index]` corresponds to major version `index + 1`.
pub(super) const VERSION_DIRS: [&str; 1] = ["v1"];

#[derive(Debug)]
/// All concrete database implementations.
/// Version-erased database backend.
///
/// This enum is the central dispatch point for the finalised-state database:
/// - It is constructed by spawning a concrete backend (for example, v0 or v1).
/// - It implements the core database traits (`DbCore`, `DbRead`, `DbWrite`).
/// - It implements capability extension traits by delegating to the concrete implementation, or by
///   returning [`FinalisedStateError::FeatureUnavailable`] when unsupported.
///
/// Capability reporting is provided by [`FinalisedSource::capability`] and must match the methods that
/// successfully dispatch in the extension trait implementations below.
pub(crate) enum FinalisedSource<T: BlockchainSource> {
    /// Legacy schema backend.
    V0(DbV0),

    /// Current schema backend.
    V1(DbV1),

    /// Ephemeral finalised state, DB disabled.
    Ephemeral(EphemeralFinalisedState<T>),
}

// ***** Core database functionality *****

impl<T: BlockchainSource> FinalisedSource<T> {
    /// Spawn a v0 database backend.
    ///
    /// This constructs and initializes the legacy schema implementation and returns it wrapped in
    /// [`FinalisedSource::V0`].
    pub(crate) async fn spawn_v0(cfg: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        Ok(Self::V0(DbV0::spawn(cfg).await?))
    }

    /// Spawn a v1 database backend.
    ///
    /// This constructs and initializes the current schema implementation and returns it wrapped in
    /// [`FinalisedSource::V1`].
    pub(crate) async fn spawn_v1(cfg: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        Ok(Self::V1(DbV1::spawn(cfg).await?))
    }

    /// Spawns a "ephemeral" finalised state.
    pub(crate) fn ephemeral(
        source: T,
        network: zebra_chain::parameters::Network,
        db_height: Option<Height>,
    ) -> Self {
        Self::Ephemeral(EphemeralFinalisedState::new(source, network, db_height))
    }

    /// Wait until the database backend reports [`StatusType::Ready`].
    ///
    /// This polls `DbCore::status()` on a fixed interval. It is intended for startup sequencing in
    /// components that require the database to be fully initialized before accepting requests.
    ///
    /// Notes:
    /// - This method does not return an error. If the database never becomes ready, it will loop.
    /// - The polling interval is intentionally small and uses `MissedTickBehavior::Delay` to avoid
    ///   burst catch-up behavior under load.
    pub(crate) async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if self.status() == StatusType::Ready {
                break;
            }
        }
    }

    /// Stores a new runtime status in the concrete backend.
    ///
    /// This is used by router-level background orchestration, for example to report an asynchronous
    /// migration failure after `FinalisedState::spawn` has already returned.
    pub(crate) fn store_status(&self, status: StatusType) {
        match self {
            Self::V0(database) => database.status_atomic().store(status),
            Self::V1(database) => database.status_atomic().store(status),
            Self::Ephemeral(ephemeral) => ephemeral.store_status(status),
        }
    }

    /// Return the capabilities supported by this database instance.
    ///
    /// This is the authoritative runtime capability set for this backend and must remain consistent
    /// with the dispatch behavior in the extension trait implementations below.
    pub(crate) fn capability(&self) -> Capability {
        match self {
            Self::V0(_) => {
                Capability::READ_CORE | Capability::WRITE_CORE | Capability::COMPACT_BLOCK_EXT
            }
            Self::V1(_) => Capability::LATEST,
            Self::Ephemeral(_) => {
                Capability::READ_CORE
                    | Capability::WRITE_CORE
                    | Capability::BLOCK_CORE_EXT
                    | Capability::BLOCK_TRANSPARENT_EXT
                    | Capability::BLOCK_SHIELDED_EXT
                    | Capability::COMPACT_BLOCK_EXT
                    | Capability::CHAIN_BLOCK_EXT
            }
        }
    }

    /// Return an arc clone of the underlying LMDB environment, used during some DB migrations.
    pub(crate) fn env(&self) -> Result<Arc<Environment>, FinalisedStateError> {
        match self {
            Self::V1(db) => Ok(Arc::clone(db.env())),
            Self::V0(db) => Ok(Arc::clone(db.env())),
            Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "no LMDB environment available",
            )),
        }
    }

    /// Provides access to the metadata DB table, enabling the migration manager
    /// to use this DB table to store temporary migration metadata.
    pub(crate) fn metadata_db(&self) -> Result<Database, FinalisedStateError> {
        match self {
            Self::V1(db) => Ok(db.metadata_db()),
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 metadata db not available",
            )),
        }
    }

    /// Provudes access to the spent DB table, required for Migration1_1_0To1_2_0.
    pub(crate) fn spent_db(&self) -> Result<Database, FinalisedStateError> {
        match self {
            Self::V1(db) => Ok(db.spent_db()),
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 spent db not available",
            )),
        }
    }

    /// Provides access to the reverse txid-index DB table, required for Migration1_1_0To1_2_0.
    pub(crate) fn txid_location_db(&self) -> Result<Database, FinalisedStateError> {
        match self {
            Self::V1(db) => Ok(db.txid_location_db()),
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 txid_location db",
            )),
        }
    }

    /// Provides access to the txids DB table, required for Migration1_1_0To1_2_0.
    pub(crate) fn txids_db(&self) -> Result<Database, FinalisedStateError> {
        match self {
            Self::V1(db) => Ok(db.txids_db()),
            Self::V0(_) | Self::Ephemeral(_) => {
                Err(FinalisedStateError::FeatureUnavailable("v1 txids db"))
            }
        }
    }

    /// Provides access to the transparent DB table, required for Migration1_1_0To1_2_0 Stage B to
    /// read block transparent data directly (bypassing per-height block re-validation).
    pub(crate) fn transparent_db(&self) -> Result<Database, FinalisedStateError> {
        match self {
            Self::V1(db) => Ok(db.transparent_db()),
            Self::V0(_) | Self::Ephemeral(_) => {
                Err(FinalisedStateError::FeatureUnavailable("v1 transparent db"))
            }
        }
    }

    /// Provides access to the finalised txout-set accumulator DB table.
    pub(crate) fn tx_out_set_info_accumulator_db(&self) -> Result<Database, FinalisedStateError> {
        match self {
            Self::V1(database) => Ok(database.tx_out_set_info_accumulator_db()),
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 tx_out_set_info_accumulator db not available",
            )),
        }
    }

    /// Bulk-rebuilds the finalised txout-set accumulator to the current tip and persists it (V1
    /// only).
    ///
    /// Recomputes the accumulator from the finalised `transparent` + `spent` tables via sequential
    /// scans and writes the singleton plus its freshness watermark. Replaces the per-block
    /// accumulator maintenance that dominated sync time at sandblast height; used by
    /// `sync_to_height` after a catch-up run and by the v1.2 migration's accumulator stage.
    pub(crate) async fn rebuild_tx_out_set_accumulator(&self) -> Result<(), FinalisedStateError> {
        match self {
            Self::V1(database) => database.rebuild_tx_out_set_accumulator().await,
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 txout-set accumulator builder",
            )),
        }
    }
}

impl<T: BlockchainSource> From<DbV0> for FinalisedSource<T> {
    /// Wrap an already-constructed v0 database backend.
    fn from(value: DbV0) -> Self {
        Self::V0(value)
    }
}

impl<T: BlockchainSource> From<DbV1> for FinalisedSource<T> {
    /// Wrap an already-constructed v1 database backend.
    fn from(value: DbV1) -> Self {
        Self::V1(value)
    }
}

impl<T: BlockchainSource> From<EphemeralFinalisedState<T>> for FinalisedSource<T> {
    /// Wrap an already-constructed ephemeral finalised state backend.
    fn from(value: EphemeralFinalisedState<T>) -> Self {
        Self::Ephemeral(value)
    }
}

#[async_trait]
impl<T: BlockchainSource> DbCore for FinalisedSource<T> {
    /// Return the current status of the backend.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    fn status(&self) -> StatusType {
        match self {
            Self::V0(db) => DbCore::status(db),
            Self::V1(db) => DbCore::status(db),
            Self::Ephemeral(ephemeral) => DbCore::status(ephemeral),
        }
    }

    /// Shut down the backend and release associated resources.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => DbCore::shutdown(db).await,
            Self::V1(db) => DbCore::shutdown(db).await,
            Self::Ephemeral(ephemeral) => DbCore::shutdown(ephemeral).await,
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> DbRead for FinalisedSource<T> {
    /// Return the highest stored height in the database, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        match self {
            Self::V0(db) => db.db_height().await,
            Self::V1(db) => db.db_height().await,
            Self::Ephemeral(ephemeral) => ephemeral.db_height().await,
        }
    }

    /// Resolve a block hash to its stored height, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        match self {
            Self::V0(db) => db.get_block_height(hash).await,
            Self::V1(db) => db.get_block_height(hash).await,
            Self::Ephemeral(ephemeral) => ephemeral.get_block_height(hash).await,
        }
    }

    /// Resolve a block height to its stored block hash, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn get_block_hash(
        &self,
        height: Height,
    ) -> Result<Option<BlockHash>, FinalisedStateError> {
        match self {
            Self::V0(db) => db.get_block_hash(height).await,
            Self::V1(db) => db.get_block_hash(height).await,
            Self::Ephemeral(ephemeral) => ephemeral.get_block_hash(height).await,
        }
    }

    /// Read the database metadata record.
    ///
    /// This includes versioning and migration status and is used by the migration manager and
    /// compatibility checks.
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        match self {
            Self::V0(db) => db.get_metadata().await,
            Self::V1(db) => db.get_metadata().await,
            Self::Ephemeral(ephemeral) => ephemeral.get_metadata().await,
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> DbWrite for FinalisedSource<T> {
    /// Write a fully-indexed block into the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.write_block(block).await,
            Self::V1(db) => db.write_block(block).await,
            Self::Ephemeral(_ephemeral) => Ok(()),
        }
    }

    /// Bulk catch-up ingestion, delegated to the concrete backend's strategy.
    async fn write_blocks_to_height<S: crate::chain_index::source::BlockchainSource>(
        &self,
        height: Height,
        source: &S,
    ) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.write_blocks_to_height(height, source).await,
            Self::V1(db) => db.write_blocks_to_height(height, source).await,
            Self::Ephemeral(db) => db.write_blocks_to_height(height, source).await,
        }
    }

    /// Delete the block at a given height, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.delete_block_at_height(height).await,
            Self::V1(db) => db.delete_block_at_height(height).await,
            Self::Ephemeral(_ephemeral) => Ok(()),
        }
    }

    /// Delete a specific indexed block from the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.delete_block(block).await,
            Self::V1(db) => db.delete_block(block).await,
            Self::Ephemeral(_ephemeral) => Ok(()),
        }
    }

    /// Update the database metadata record.
    ///
    /// This is used by migrations and schema management logic.
    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.update_metadata(metadata).await,
            Self::V1(db) => db.update_metadata(metadata).await,
            Self::Ephemeral(_ephemeral) => Ok(()),
        }
    }
}

// ***** Database capability extension traits *****
//
// Each extension trait corresponds to a distinct capability group. The dispatch rules are:
// - If the backend supports the capability, delegate to the concrete implementation.
// - If unsupported, return `FinalisedStateError::FeatureUnavailable("<capability_name>")`.
//
// These names must remain consistent with the capability wiring in `capability.rs`.

#[async_trait]
impl<T: BlockchainSource> BlockCoreExt for FinalisedSource<T> {
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_header(height).await,
            Self::Ephemeral(db) => db.get_block_header(height).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_range_headers(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<BlockHeaderData>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_headers(start, end).await,
            Self::Ephemeral(db) => db.get_block_range_headers(start, end).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_txids(height).await,
            Self::Ephemeral(db) => db.get_block_txids(height).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_range_txids(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TxidList>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_txids(start, end).await,
            Self::Ephemeral(db) => db.get_block_range_txids(start, end).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_txid(tx_location).await,
            Self::Ephemeral(db) => db.get_txid(tx_location).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_tx_location(txid).await,
            Self::Ephemeral(db) => db.get_tx_location(txid).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> BlockTransparentExt for FinalisedSource<T> {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_transparent(tx_location).await,
            Self::Ephemeral(db) => db.get_transparent(tx_location).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_transparent")),
        }
    }

    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_transparent(height).await,
            Self::Ephemeral(db) => db.get_block_transparent(height).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_transparent")),
        }
    }

    async fn get_block_range_transparent(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransparentTxList>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_transparent(start, end).await,
            Self::Ephemeral(db) => db.get_block_range_transparent(start, end).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_transparent")),
        }
    }

    async fn get_previous_output(
        &self,
        outpoint: Outpoint,
    ) -> Result<TxOutCompact, FinalisedStateError> {
        match self {
            Self::V1(db) => <DbV1 as BlockTransparentExt>::get_previous_output(db, outpoint).await,
            Self::Ephemeral(db) => {
                <EphemeralFinalisedState<T> as BlockTransparentExt>::get_previous_output(
                    db, outpoint,
                )
                .await
            }

            _ => Err(FinalisedStateError::FeatureUnavailable("block_transparent")),
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> BlockShieldedExt for FinalisedSource<T> {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_sapling(tx_location).await,
            Self::Ephemeral(db) => db.get_sapling(tx_location).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_sapling(&self, h: Height) -> Result<SaplingTxList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_sapling(h).await,
            Self::Ephemeral(db) => db.get_block_sapling(h).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_range_sapling(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<SaplingTxList>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_sapling(start, end).await,
            Self::Ephemeral(db) => db.get_block_range_sapling(start, end).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_orchard(tx_location).await,
            Self::Ephemeral(db) => db.get_orchard(tx_location).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_orchard(&self, h: Height) -> Result<OrchardTxList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_orchard(h).await,
            Self::Ephemeral(db) => db.get_block_orchard(h).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_range_orchard(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<OrchardTxList>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_orchard(start, end).await,
            Self::Ephemeral(db) => db.get_block_range_orchard(start, end).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_commitment_tree_data(height).await,
            Self::Ephemeral(db) => db.get_block_commitment_tree_data(height).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_range_commitment_tree_data(
        &self,
        start: Height,
        end: Height,
    ) -> Result<Vec<CommitmentTreeData>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_range_commitment_tree_data(start, end).await,
            Self::Ephemeral(db) => db.get_block_range_commitment_tree_data(start, end).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> CompactBlockExt for FinalisedSource<T> {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        #[allow(unreachable_patterns)]
        match self {
            Self::V0(db) => db.get_compact_block(height, pool_types).await,
            Self::V1(db) => db.get_compact_block(height, pool_types).await,
            Self::Ephemeral(db) => db.get_compact_block(height, pool_types).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("compact_block")),
        }
    }

    async fn get_compact_block_stream(
        &self,
        start_height: Height,
        end_height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<CompactBlockStream, FinalisedStateError> {
        #[allow(unreachable_patterns)]
        match self {
            Self::V0(db) => {
                db.get_compact_block_stream(start_height, end_height, pool_types)
                    .await
            }
            Self::V1(db) => {
                db.get_compact_block_stream(start_height, end_height, pool_types)
                    .await
            }
            Self::Ephemeral(db) => {
                db.get_compact_block_stream(start_height, end_height, pool_types)
                    .await
            }

            _ => Err(FinalisedStateError::FeatureUnavailable("compact_block")),
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> IndexedBlockExt for FinalisedSource<T> {
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_chain_block(height).await,
            Self::Ephemeral(db) => db.get_chain_block(height).await,

            _ => Err(FinalisedStateError::FeatureUnavailable("chain_block")),
        }
    }
}

#[async_trait]
impl<T: BlockchainSource> TransparentHistExt for FinalisedSource<T> {
    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_records(
        &self,
        script: AddrScript,
    ) -> Result<Option<Vec<crate::chain_index::types::AddrEventBytes>>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.addr_records(script).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_and_index_records(
        &self,
        script: AddrScript,
        tx_location: TxLocation,
    ) -> Result<Option<Vec<crate::chain_index::types::AddrEventBytes>>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.addr_and_index_records(script, tx_location).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_tx_locations_by_range(
        &self,
        script: AddrScript,
        start: Height,
        end: Height,
    ) -> Result<Option<Vec<TxLocation>>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.addr_tx_locations_by_range(script, start, end).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_utxos_by_range(
        &self,
        script: AddrScript,
        start: Height,
        end: Height,
    ) -> Result<Option<Vec<(TxLocation, u16, u64)>>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.addr_utxos_by_range(script, start, end).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    #[cfg(feature = "transparent_address_history_experimental")]
    async fn addr_balance_by_range(
        &self,
        script: AddrScript,
        start: Height,
        end: Height,
    ) -> Result<i64, FinalisedStateError> {
        match self {
            Self::V1(db) => db.addr_balance_by_range(script, start, end).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn get_outpoint_spender(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_outpoint_spender(outpoint).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn get_outpoint_spenders(
        &self,
        outpoints: Vec<Outpoint>,
    ) -> Result<Vec<Option<TxLocation>>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_outpoint_spenders(outpoints).await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }

    async fn get_tx_out_set_info_accumulator(
        &self,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        match self {
            Self::V1(database) => database.get_tx_out_set_info_accumulator().await,
            _ => Err(FinalisedStateError::FeatureUnavailable(
                "transparent_history",
            )),
        }
    }
}

#[cfg(test)]
impl<T: BlockchainSource> FinalisedSource<T> {
    /// Spawn a test-only v1 backend initialized as a v1.0.0 database.
    ///
    /// Used by migration tests to create a historical v1.0.0 database fixture before reopening it
    /// through the current startup / migration path.
    pub(crate) async fn spawn_v1_0_0(cfg: &ChainIndexConfig) -> Result<Self, FinalisedStateError> {
        Ok(Self::V1(DbV1::spawn_v1_0_0(cfg).await?))
    }

    /// Current contiguous validated-tip height (v1 only; 0 for v0). Test hook.
    pub(crate) fn validated_tip_height(&self) -> u32 {
        match self {
            Self::V1(db) => db.validated_tip_height(),
            Self::V0(_) | Self::Ephemeral(_) => 0,
        }
    }

    /// Reads the height the persisted txout-set accumulator currently reflects (V1 only).
    ///
    /// `None` means it has never been built. Test hook for asserting the incremental range-update
    /// path advances the watermark (and is therefore taken, rather than a silent rebuild fallback).
    pub(crate) async fn read_tx_out_set_accumulator_built_height(
        &self,
    ) -> Result<Option<Height>, FinalisedStateError> {
        match self {
            Self::V1(database) => database.read_tx_out_set_accumulator_built_height().await,
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 txout-set accumulator builder",
            )),
        }
    }

    /// Computes (without persisting) the bulk-built txout-set accumulator to `db_tip` (V1 only).
    ///
    /// Test hook for asserting the sequential bulk builder matches the incrementally-maintained
    /// accumulator across shard counts.
    pub(crate) fn build_tx_out_set_accumulator_blocking(
        &self,
        db_tip: Height,
        shards: u16,
    ) -> Result<FinalisedTxOutSetInfoAccumulator, FinalisedStateError> {
        match self {
            Self::V1(database) => database.build_tx_out_set_accumulator_blocking(db_tip, shards),
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::FeatureUnavailable(
                "v1 txout-set accumulator builder",
            )),
        }
    }

    /// Writes a block using the v1.0.0 format.
    ///
    /// This intentionally writes only the core v1 tables and uses v1 item encodings.
    ///
    /// This method does not perform safety checks and must not be used in production code.
    ///
    /// Used for migration tests.
    pub(crate) async fn write_block_v1_0_0(
        &self,
        block: IndexedBlock,
    ) -> Result<(), FinalisedStateError> {
        match self {
            Self::V1(db) => db.write_block_v1_0_0(block).await,
            Self::V0(_) | Self::Ephemeral(_) => Err(FinalisedStateError::Custom(
                "v1.0.0 test fixture writer requires a v1 backend".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod shutdown {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{sync::Barrier, time::timeout};

    struct FakeDb {
        env: Arc<Environment>,
        db_handler: Mutex<Option<JoinHandle<()>>>,
        cancel_token: CancellationToken,
        status: NamedAtomicStatus,
    }

    impl LmdbLifecycle for FakeDb {
        fn env(&self) -> &Arc<Environment> {
            &self.env
        }
        fn db_handler_slot(&self) -> &Mutex<Option<JoinHandle<()>>> {
            &self.db_handler
        }
        fn cancel_token(&self) -> &CancellationToken {
            &self.cancel_token
        }
        fn status_atomic(&self) -> &NamedAtomicStatus {
            &self.status
        }
    }

    /// Regression for #1033 — every task awaiting cancellation must observe shutdown,
    /// not just one. Originally written against the `Notify::notify_one` implementation
    /// (which strands N-1 waiters); now passes against `CancellationToken::cancel`,
    /// which wakes all current waiters and persists state for late subscribers.
    #[tokio::test]
    async fn wakes_every_shutdown_waiter() {
        let tmp = tempfile::tempdir().unwrap();
        let env = Arc::new(
            lmdb::Environment::new()
                .set_map_size(1 << 20)
                .open(tmp.path())
                .unwrap(),
        );
        let db = Arc::new(FakeDb {
            env,
            db_handler: Mutex::new(None),
            cancel_token: CancellationToken::new(),
            status: NamedAtomicStatus::new("test", StatusType::Ready),
        });

        const N: usize = 3;
        let woke = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(N + 1));

        let mut waiters = Vec::with_capacity(N);
        for _ in 0..N {
            let token = db.cancel_token.clone();
            let woke = Arc::clone(&woke);
            let barrier = Arc::clone(&barrier);
            waiters.push(tokio::spawn(async move {
                barrier.wait().await;
                token.cancelled().await;
                woke.fetch_add(1, Ordering::Relaxed);
            }));
        }
        barrier.wait().await;

        LmdbLifecycle::shutdown(db.as_ref()).await.unwrap();

        for (i, w) in waiters.into_iter().enumerate() {
            timeout(Duration::from_millis(200), w)
                .await
                .unwrap_or_else(|_| panic!("waiter {i} stranded: cancel_token woke only a subset"))
                .unwrap();
        }
        assert_eq!(woke.load(Ordering::Relaxed), N);
    }
}
