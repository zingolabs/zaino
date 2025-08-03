//! Holds the Finalised portion of the chain index on disk.

// TODO / FIX - ROMOVE THIS ONCE CHAININDEX LANDS!
#![allow(dead_code)]

pub(crate) mod capability;
pub(crate) mod db;
pub(crate) mod entry;
pub(crate) mod migrations;
pub(crate) mod reader;

use capability::*;
use db::v1::*;
use entry::*;
use reader::*;
use tokio::time::{interval, MissedTickBehavior};

use crate::{
    config::BlockCacheConfig, error::FinalisedStateError, ChainBlock, Hash, Height, StatusType,
    ZainoVersionedSerialise as _,
};

use lmdb::{Environment, EnvironmentFlags, Transaction};
use std::{path::Path, sync::Arc, time::Duration};

/// ZainoDB: Versioned database holding the finalised portion of the blockchain.
pub(crate) struct ZainoDB {
    db: Arc<dyn DbCore + Send + Sync>,
    caps: Capability,
    cfg: BlockCacheConfig,
}

impl ZainoDB {
    // ***** Db Control *****

    /// Spawns a ZainoDB, opens a database if a path is given in the config else  creates a new db.
    pub(crate) async fn spawn(cfg: BlockCacheConfig) -> Result<Self, FinalisedStateError> {
        let meta_opt = Self::peek_metadata(&cfg.database.path).await?;

        let (backend, caps): (Arc<dyn DbCore + Send + Sync>, Capability) = match meta_opt {
            Some(meta) => {
                let caps = meta.version.capability();
                let db: Arc<dyn DbCore + Send + Sync> = match meta.version.major() {
                    1 => Arc::new(DbV1::spawn(&cfg).await?) as _,
                    _ => {
                        return Err(FinalisedStateError::Custom(format!(
                            "unsupported version {}",
                            meta.version
                        )))
                    }
                };
                (db, caps)
            }
            None => {
                let db = Arc::new(DbV1::spawn(&cfg).await?) as _;
                (db, Capability::LATEST)
            }
        };

        Ok(Self {
            db: backend,
            caps,
            cfg,
        })
    }

    /// Gracefully shuts down the running ZainoDB, closing all child processes.
    pub(crate) async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.db.shutdown().await
    }

    /// Returns the status of the running ZainoDB.
    pub(crate) async fn status(&self) -> StatusType {
        self.db.status().await
    }

    /// Waits until the ZainoDB returns a Ready status.
    pub(crate) async fn wait_until_ready(&self) {
        let mut ticker = interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            ticker.tick().await;
            if self.db.status().await == StatusType::Ready {
                break;
            }
        }
    }

    /// Creates a read-only viewer onto the running ZainoDB.
    ///
    /// NOTE: **ALL** chain fetch should use DbReader instead of directly using ZainoDB.
    pub(crate) fn to_reader(self: &Arc<Self>) -> DbReader {
        DbReader {
            inner: Arc::clone(self),
        }
    }

    /// Try to read the metadata entry.
    ///
    /// * `Ok(Some(meta))` – DB exists, metadata decoded.
    /// * `Ok(None)`      – directory or key is missing ⇒ fresh DB.
    /// * `Err(e)`        – any other LMDB / decode failure.
    async fn peek_metadata(path: &Path) -> Result<Option<DbMetadata>, FinalisedStateError> {
        if !path.exists() {
            return Ok(None); // brand-new DB
        }

        let env = match Environment::new()
            .set_max_dbs(4)
            .set_flags(EnvironmentFlags::READ_ONLY)
            .open(path)
        {
            Ok(env) => env,
            Err(lmdb::Error::Other(2)) => return Ok(None),
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        };

        tokio::task::block_in_place(|| {
            let meta_dbi = env.open_db(Some("metadata"))?; // if DBI missing, lmdb::NotFound
            let txn = env.begin_ro_txn()?;
            match txn.get(meta_dbi, b"metadata") {
                Ok(raw) => {
                    let entry = StoredEntryFixed::<DbMetadata>::from_bytes(raw).map_err(|e| {
                        FinalisedStateError::Custom(format!("metadata decode error: {e}"))
                    })?;
                    Ok(Some(entry.item))
                }
                Err(lmdb::Error::NotFound) => Ok(None), // key missing
                Err(e) => Err(FinalisedStateError::LmdbError(e)),
            }
        })
    }

    // ***** Db Core Write *****

    /// Writes a block to the database.
    ///
    /// This **MUST** be the *next* block in the chain (db_tip_height + 1).
    pub(crate) async fn write_block(&self, block: ChainBlock) -> Result<(), FinalisedStateError> {
        self.db.write_block(block).await
    }

    /// Deletes a block from the database by height.
    ///
    /// This **MUST** be the *top* block in the db.
    ///
    /// Uses `delete_block` internally, fails if the block to be deleted cannot be correctly built.
    /// If this happens, the block to be deleted must be fetched from the validator and given to `delete_block`
    /// to ensure the block has been completely wiped from the database.
    pub(crate) async fn delete_block_at_height(
        &self,
        height: Height,
    ) -> Result<(), FinalisedStateError> {
        self.db.delete_block_at_height(height).await
    }

    /// Deletes a given block from the database.
    ///
    /// This **MUST** be the *top* block in the db.
    pub(crate) async fn delete_block(&self, block: &ChainBlock) -> Result<(), FinalisedStateError> {
        self.db.delete_block(block).await
    }

    // ***** DB Core Read *****

    /// Returns the highest block height held in the database.
    pub(crate) async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.db.db_height().await
    }

    /// Returns the block height for the given block hash *if* present in the finalised state.
    ///
    /// TODO: Should theis return `Result<Option<Height>, FinalisedStateError>`?
    pub(crate) async fn get_block_height(&self, hash: Hash) -> Result<Height, FinalisedStateError> {
        self.db.get_block_height(hash).await
    }

    /// Returns the block block hash for the given block height *if* present in the finlaised state.
    pub(crate) async fn get_block_hash(&self, height: Height) -> Result<Hash, FinalisedStateError> {
        self.db.get_block_hash(height).await
    }

    /// Returns metadata for the running ZainoDB.
    pub(crate) async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.db.get_metadata().await
    }

    // ***** DB Extension methods *****

    #[inline]
    fn downcast<T: 'static>(&self) -> Option<&T> {
        self.db.as_any().downcast_ref::<T>()
    }

    /// Returns the block core extension if present.
    pub(crate) fn block_core(&self) -> Option<&dyn BlockCoreExt> {
        if self.caps.has(Capability::BLOCK_CORE_EXT) {
            let backend = self.downcast::<DbV1>().unwrap();
            Some(backend as &dyn BlockCoreExt)
        } else {
            None
        }
    }

    /// Returns the block transparent extension if present.
    pub(crate) fn block_transparent(&self) -> Option<&dyn BlockTransparentExt> {
        if self.caps.has(Capability::BLOCK_TRANSPARENT_EXT) {
            let backend = self.downcast::<DbV1>().unwrap();
            Some(backend as &dyn BlockTransparentExt)
        } else {
            None
        }
    }

    /// Returns the block shielded extension if present.
    pub(crate) fn block_shielded(&self) -> Option<&dyn BlockShieldedExt> {
        if self.caps.has(Capability::BLOCK_SHIELDED_EXT) {
            let backend = self.downcast::<DbV1>().unwrap();
            Some(backend as &dyn BlockShieldedExt)
        } else {
            None
        }
    }

    /// Returns the compact block extension if present.
    pub(crate) fn compact_block(&self) -> Option<&dyn CompactBlockExt> {
        if self.caps.has(Capability::COMPACT_BLOCK_EXT) {
            let backend = self.downcast::<DbV1>().unwrap();
            Some(backend as &dyn CompactBlockExt)
        } else {
            None
        }
    }

    /// Returns the chain block extension if present.
    pub(crate) fn chain_block(&self) -> Option<&dyn ChainBlockExt> {
        if self.caps.has(Capability::CHAIN_BLOCK_EXT) {
            let backend = self.downcast::<DbV1>().unwrap();
            Some(backend as &dyn ChainBlockExt)
        } else {
            None
        }
    }

    /// Returns the transparent transaction history extension if present.
    pub(crate) fn transparent_hist(&self) -> Option<&dyn TransparentHistExt> {
        if self.caps.has(Capability::TRANSPARENT_HIST_EXT) {
            let backend = self.downcast::<DbV1>().unwrap();
            Some(backend as &dyn TransparentHistExt)
        } else {
            None
        }
    }
}
