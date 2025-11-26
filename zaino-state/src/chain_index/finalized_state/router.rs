//! Implements the ZainoDB Router, used to selectively route database capabilities during major migrations.
//!
//! The Router allows incremental database migrations by splitting read and write capability groups between primary and shadow databases.
//! This design enables partial migrations without duplicating the entire chain database,
//! greatly reducing disk usage and ensuring minimal downtime.

use super::{
    capability::{Capability, DbCore, DbMetadata, DbRead, DbWrite},
    db::DbBackend,
};

use crate::{
    chain_index::finalised_state::capability::CapabilityRequest, error::FinalisedStateError,
    BlockHash, Height, IndexedBlock, StatusType,
};

use arc_swap::{ArcSwap, ArcSwapOption};
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

pub(crate) struct Router {
    /// Primary active database.
    primary: ArcSwap<DbBackend>,
    /// Shadow database, new version to be built during major migration.
    shadow: ArcSwapOption<DbBackend>,
    /// Capability mask for primary database.
    primary_mask: AtomicU32,
    /// Capability mask dictating what database capalility (if any) should be served by the shadow.
    shadow_mask: AtomicU32,
}

/// Database version router.
///
/// Routes database capability to the correct database during major migrations.
impl Router {
    // ***** Router creation *****

    /// Creatues a new database router, setting primary the given database.
    ///
    /// Shadow is spawned as none and should only be set to some during major database migrations.
    pub(crate) fn new(primary: Arc<DbBackend>) -> Self {
        let cap = primary.capability();
        Self {
            primary: ArcSwap::from(primary),
            shadow: ArcSwapOption::empty(),
            primary_mask: AtomicU32::new(cap.bits()),
            shadow_mask: AtomicU32::new(0),
        }
    }

    // ***** Capability router *****

    /// Return the database backend for a given capability, or an error if none is available.
    #[inline]
    pub(crate) fn backend(
        &self,
        cap: CapabilityRequest,
    ) -> Result<Arc<DbBackend>, FinalisedStateError> {
        let bit = cap.as_capability().bits();

        if self.shadow_mask.load(Ordering::Acquire) & bit != 0 {
            if let Some(shadow_db) = self.shadow.load().as_ref() {
                return Ok(Arc::clone(shadow_db));
            }
        }
        if self.primary_mask.load(Ordering::Acquire) & bit != 0 {
            return Ok(self.primary.load_full());
        }

        Err(FinalisedStateError::FeatureUnavailable(cap.name()))
    }

    // ***** Shadow database control *****
    //
    // These methods should only ever be used by the migration manager.

    /// Sets the shadow to the given database.
    pub(crate) fn set_shadow(&self, shadow: Arc<DbBackend>, caps: Capability) {
        self.shadow.store(Some(shadow));
        self.shadow_mask.store(caps.bits(), Ordering::Release);
    }

    /// Move additional capability bits to the *current* shadow.
    pub(crate) fn extend_shadow_caps(&self, caps: Capability) {
        self.shadow_mask.fetch_or(caps.bits(), Ordering::AcqRel);
    }

    /// Promotes the shadow database to primary, resets shadow,
    /// and updates the primary capability mask from the new backend.
    ///
    /// Used at the end of major migrations to move the active database to the new version.
    ///
    /// Returns the initial primary value.
    ///
    /// # Error
    ///
    /// Returns a critical error if the shadow is not found.
    pub(crate) fn promote_shadow(&self) -> Result<Arc<DbBackend>, FinalisedStateError> {
        let Some(new_primary) = self.shadow.swap(None) else {
            return Err(FinalisedStateError::Critical(
                "shadow not found!".to_string(),
            ));
        };

        self.primary_mask
            .store(new_primary.capability().bits(), Ordering::Release);
        self.shadow_mask.store(0, Ordering::Release);

        Ok(self.primary.swap(new_primary))
    }

    // ***** Primary database capability control *****

    /// Disables specific capabilities on the primary backend.
    pub(crate) fn limit_primary_caps(&self, caps: Capability) {
        self.primary_mask.fetch_and(!caps.bits(), Ordering::AcqRel);
    }

    /// Enables specific capabilities on the primary backend.
    pub(crate) fn extend_primary_caps(&self, caps: Capability) {
        self.primary_mask.fetch_or(caps.bits(), Ordering::AcqRel);
    }

    /// Overwrites the entire primary capability mask.
    pub(crate) fn set_primary_mask(&self, new_mask: Capability) {
        self.primary_mask.store(new_mask.bits(), Ordering::Release);
    }
}

// ***** Core DB functionality *****

#[async_trait]
impl DbCore for Router {
    fn status(&self) -> StatusType {
        match self.backend(CapabilityRequest::ReadCore) {
            Ok(backend) => backend.status(),
            Err(_) => StatusType::Busy,
        }
    }

    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        let primary_shutdown_result = self.primary.load_full().shutdown().await;

        let shadow_option = self.shadow.load();
        let shadow_shutdown_result = match shadow_option.as_ref() {
            Some(shadow_database) => shadow_database.shutdown().await,
            None => Ok(()),
        };

        primary_shutdown_result?;
        shadow_shutdown_result
    }
}

#[async_trait]
impl DbWrite for Router {
    async fn write_block(&self, blk: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .write_block(blk)
            .await
    }

    async fn delete_block_at_height(&self, h: Height) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .delete_block_at_height(h)
            .await
    }

    async fn delete_block(&self, blk: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .delete_block(blk)
            .await
    }

    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .update_metadata(metadata)
            .await
    }
}

#[async_trait]
impl DbRead for Router {
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?.db_height().await
    }

    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?
            .get_block_height(hash)
            .await
    }

    async fn get_block_hash(&self, h: Height) -> Result<Option<BlockHash>, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?
            .get_block_hash(h)
            .await
    }

    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?
            .get_metadata()
            .await
    }
}
