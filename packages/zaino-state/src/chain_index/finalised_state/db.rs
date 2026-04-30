//! Versioned database backends (DbBackend) and major-version dispatch
//!
//! This file defines the major-version split for the on-disk finalised database and provides
//! [`DbBackend`], a version-erased enum used throughout the finalised-state subsystem.
//!
//! Concrete database implementations live in:
//! - [`v0`]: legacy schema (compact-block streamer)
//! - [`v1`]: current schema (expanded indices and query surface)
//!
//! `DbBackend` delegates the core DB traits (`DbCore`, `DbRead`, `DbWrite`) and all extension traits
//! to the appropriate concrete implementation.
//!
//! # Capability model integration
//!
//! Each `DbBackend` instance declares its supported [`Capability`] set via `DbBackend::capability()`.
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
//! 1. Create `db::v2` and implement `DbV2::spawn(cfg)`.
//! 2. Add `V2(DbV2)` variant to [`DbBackend`].
//! 3. Add `spawn_v2` constructor.
//! 4. Append `"v2"` to [`VERSION_DIRS`].
//! 5. Extend all trait delegation `match` arms in this file.
//! 6. Update `DbBackend::capability()` and `DbVersion::capability()` for the new version.
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

pub(crate) mod v0;
pub(crate) mod v1;

use v0::DbV0;
use v1::DbV1;
use zaino_proto::proto::utils::PoolTypeFilter;

use crate::{
    chain_index::{
        finalised_state::capability::{
            BlockCoreExt, BlockShieldedExt, BlockTransparentExt, CompactBlockExt, DbCore,
            DbMetadata, DbRead, DbWrite, IndexedBlockExt,
        },
        types::TransactionHash,
    },
    config::BlockCacheConfig,
    error::FinalisedStateError,
    BlockHash, BlockHeaderData, CommitmentTreeData, CompactBlockStream, Height, IndexedBlock,
    NamedAtomicStatus, OrchardCompactTx, OrchardTxList, SaplingCompactTx, SaplingTxList,
    StatusType, TransparentCompactTx, TransparentTxList, TxLocation, TxidList,
};

#[cfg(feature = "transparent_address_history_experimental")]
use crate::{chain_index::finalised_state::capability::TransparentHistExt, AddrScript, Outpoint};

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
/// Capability reporting is provided by [`DbBackend::capability`] and must match the methods that
/// successfully dispatch in the extension trait implementations below.
pub(crate) enum DbBackend {
    /// Legacy schema backend.
    V0(DbV0),

    /// Current schema backend.
    V1(DbV1),
}

// ***** Core database functionality *****

impl DbBackend {
    /// Spawn a v0 database backend.
    ///
    /// This constructs and initializes the legacy schema implementation and returns it wrapped in
    /// [`DbBackend::V0`].
    pub(crate) async fn spawn_v0(cfg: &BlockCacheConfig) -> Result<Self, FinalisedStateError> {
        Ok(Self::V0(DbV0::spawn(cfg).await?))
    }

    /// Spawn a v1 database backend.
    ///
    /// This constructs and initializes the current schema implementation and returns it wrapped in
    /// [`DbBackend::V1`].
    pub(crate) async fn spawn_v1(cfg: &BlockCacheConfig) -> Result<Self, FinalisedStateError> {
        Ok(Self::V1(DbV1::spawn(cfg).await?))
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
        }
    }
}

impl From<DbV0> for DbBackend {
    /// Wrap an already-constructed v0 database backend.
    fn from(value: DbV0) -> Self {
        Self::V0(value)
    }
}

impl From<DbV1> for DbBackend {
    /// Wrap an already-constructed v1 database backend.
    fn from(value: DbV1) -> Self {
        Self::V1(value)
    }
}

#[async_trait]
impl DbCore for DbBackend {
    /// Return the current status of the backend.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    fn status(&self) -> StatusType {
        match self {
            Self::V0(db) => DbCore::status(db),
            Self::V1(db) => DbCore::status(db),
        }
    }

    /// Shut down the backend and release associated resources.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => DbCore::shutdown(db).await,
            Self::V1(db) => DbCore::shutdown(db).await,
        }
    }
}

#[async_trait]
impl DbRead for DbBackend {
    /// Return the highest stored height in the database, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        match self {
            Self::V0(db) => db.db_height().await,
            Self::V1(db) => db.db_height().await,
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
        }
    }
}

#[async_trait]
impl DbWrite for DbBackend {
    /// Write a fully-indexed block into the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn write_block(&self, block: IndexedBlock) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.write_block(block).await,
            Self::V1(db) => db.write_block(block).await,
        }
    }

    /// Delete the block at a given height, if present.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block_at_height(&self, height: Height) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.delete_block_at_height(height).await,
            Self::V1(db) => db.delete_block_at_height(height).await,
        }
    }

    /// Delete a specific indexed block from the database.
    ///
    /// This is a thin delegation wrapper over the concrete implementation.
    async fn delete_block(&self, block: &IndexedBlock) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.delete_block(block).await,
            Self::V1(db) => db.delete_block(block).await,
        }
    }

    /// Update the database metadata record.
    ///
    /// This is used by migrations and schema management logic.
    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        match self {
            Self::V0(db) => db.update_metadata(metadata).await,
            Self::V1(db) => db.update_metadata(metadata).await,
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
impl BlockCoreExt for DbBackend {
    async fn get_block_header(
        &self,
        height: Height,
    ) -> Result<BlockHeaderData, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_header(height).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_block_txids(&self, height: Height) -> Result<TxidList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_txids(height).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_txid(
        &self,
        tx_location: TxLocation,
    ) -> Result<TransactionHash, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_txid(tx_location).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }

    async fn get_tx_location(
        &self,
        txid: &TransactionHash,
    ) -> Result<Option<TxLocation>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_tx_location(txid).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_core")),
        }
    }
}

#[async_trait]
impl BlockTransparentExt for DbBackend {
    async fn get_transparent(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<TransparentCompactTx>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_transparent(tx_location).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_transparent")),
        }
    }

    async fn get_block_transparent(
        &self,
        height: Height,
    ) -> Result<TransparentTxList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_transparent(height).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("block_transparent")),
        }
    }
}

#[async_trait]
impl BlockShieldedExt for DbBackend {
    async fn get_sapling(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<SaplingCompactTx>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_sapling(tx_location).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_sapling(&self, h: Height) -> Result<SaplingTxList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_sapling(h).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_orchard(
        &self,
        tx_location: TxLocation,
    ) -> Result<Option<OrchardCompactTx>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_orchard(tx_location).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_orchard(&self, h: Height) -> Result<OrchardTxList, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_orchard(h).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }

    async fn get_block_commitment_tree_data(
        &self,
        height: Height,
    ) -> Result<CommitmentTreeData, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_block_commitment_tree_data(height).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("block_shielded")),
        }
    }
}

#[async_trait]
impl CompactBlockExt for DbBackend {
    async fn get_compact_block(
        &self,
        height: Height,
        pool_types: PoolTypeFilter,
    ) -> Result<zaino_proto::proto::compact_formats::CompactBlock, FinalisedStateError> {
        #[allow(unreachable_patterns)]
        match self {
            Self::V0(db) => db.get_compact_block(height, pool_types).await,
            Self::V1(db) => db.get_compact_block(height, pool_types).await,
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
            _ => Err(FinalisedStateError::FeatureUnavailable("compact_block")),
        }
    }
}

#[async_trait]
impl IndexedBlockExt for DbBackend {
    async fn get_chain_block(
        &self,
        height: Height,
    ) -> Result<Option<IndexedBlock>, FinalisedStateError> {
        match self {
            Self::V1(db) => db.get_chain_block(height).await,
            _ => Err(FinalisedStateError::FeatureUnavailable("chain_block")),
        }
    }
}

#[cfg(feature = "transparent_address_history_experimental")]
#[async_trait]
impl TransparentHistExt for DbBackend {
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
