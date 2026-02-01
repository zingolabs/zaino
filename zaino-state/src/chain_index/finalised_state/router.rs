//! Capability-based database router (primary + shadow)
//!
//! This file implements [`Router`], which allows `ZainoDB` to selectively route operations to one of
//! two database backends:
//! - a **primary** (active) DB, and
//! - an optional **shadow** DB used during major migrations.
//!
//! The router is designed to support incremental and low-downtime migrations by splitting the DB
//! feature set into capability groups. Each capability group can be served by either backend,
//! controlled by atomic bitmasks.
//!
//! # Why a router exists
//!
//! Major schema upgrades are often most safely implemented as a rebuild into a new DB rather than an
//! in-place rewrite. The router enables that by allowing the system to:
//! - keep serving requests from the old DB while building the new one,
//! - optionally move specific read capabilities to the shadow DB once they are correct there,
//! - then atomically promote the shadow DB to primary at the end.
//!
//! # Concurrency and atomicity model
//!
//! The router uses `ArcSwap` / `ArcSwapOption` for lock-free backend swapping and `AtomicU32` masks
//! for capability routing.
//!
//! - Backend selection (`backend(...)`) is wait-free and based on the current masks.
//! - Promotion (`promote_shadow`) swaps the primary Arc atomically; existing in-flight operations
//!   remain valid because they hold an `Arc<DbBackend>`.
//!
//! Memory ordering is explicit (`Acquire`/`Release`/`AcqRel`) to ensure mask updates are observed
//! consistently relative to backend pointer updates.
//!
//! # Capability routing semantics
//!
//! `Router::backend(req)` resolves as:
//! 1. If `shadow_mask` contains the requested bit and shadow exists → return shadow.
//! 2. Else if `primary_mask` contains the requested bit → return primary.
//! 3. Else → return `FinalisedStateError::FeatureUnavailable`.
//!
//! # Shadow lifecycle (migration-only API)
//!
//! The following methods are intended to be called **only** by the migration manager:
//! - `set_shadow(...)`
//! - `extend_shadow_caps(...)`
//! - `promote_shadow()`
//!
//! Promotion performs:
//! - shadow → primary swap,
//! - resets shadow and shadow mask,
//! - updates the primary mask from the promoted backend’s declared capabilities,
//! - returns the old primary backend so the migration can shut it down and delete its files safely.
//!
//! # Trait impls
//!
//! `Router` implements the core DB traits (`DbCore`, `DbRead`, `DbWrite`) by routing READ_CORE/WRITE_CORE
//! to whichever backend currently serves those capabilities.
//!
//! # Development notes
//!
//! - If you introduce a new capability bit, ensure it is:
//!   - added to `CapabilityRequest`,
//!   - implemented by the relevant DB version(s),
//!   - and considered in migration routing policy (whether it can move to shadow incrementally).
//!
//! - When implementing incremental migrations (moving caps before final promotion), ensure the shadow
//!   backend is kept consistent with the primary for those capabilities (or restrict such caps to
//!   read-only queries that can tolerate lag with explicit semantics).

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

/// Capability-based database router.
///
/// `Router` is the internal dispatch layer used by `ZainoDB` to route operations to either:
/// - a **primary** database backend (the active DB), or
/// - an optional **shadow** backend used during major version migrations.
///
/// Routing is driven by per-backend **capability bitmasks**:
/// - If a requested capability bit is set in the shadow mask and a shadow backend exists, the call
///   is routed to shadow.
/// - Otherwise, if the bit is set in the primary mask, the call is routed to primary.
/// - Otherwise, the feature is reported as unavailable.
///
/// ## Concurrency model
/// - Backend pointers are stored using `ArcSwap` / `ArcSwapOption` to allow atomic, lock-free swaps.
/// - Capability masks are stored in `AtomicU32` and read using `Acquire` ordering in the hot path.
/// - Promoting shadow to primary is atomic and safe for in-flight calls because callers hold
///   `Arc<DbBackend>` clones.
///
/// ## Intended usage
/// The shadow-related APIs (`set_shadow`, `extend_shadow_caps`, `promote_shadow`) are intended to be
/// used only by the migration manager to support low-downtime rebuild-style migrations.
pub(crate) struct Router {
    /// Primary active database backend.
    ///
    /// This is the default backend used for any capability bit that is not explicitly routed to the
    /// shadow backend via [`Router::shadow_mask`].
    ///
    /// Stored behind [`ArcSwap`] so it can be replaced atomically during promotion without locking.
    primary: ArcSwap<DbBackend>,

    /// Shadow database backend (optional).
    ///
    /// During a major migration, a new-version backend is built and installed here. Individual
    /// capability groups can be routed to the shadow by setting bits in [`Router::shadow_mask`].
    ///
    /// Outside of migrations this should remain `None`.
    shadow: ArcSwapOption<DbBackend>,

    /// Capability mask for the primary backend.
    ///
    /// A bit being set means “this capability may be served by the primary backend”.
    ///
    /// The mask is initialized from `primary.capability()` and can be restricted/extended during
    /// migrations to ensure that requests are only routed to backends that can satisfy them.
    primary_mask: AtomicU32,

    /// Capability mask for the shadow backend.
    ///
    /// A bit being set means “this capability should be served by the shadow backend (if present)”.
    ///
    /// Routing precedence is:
    /// 1. shadow if the bit is set and shadow exists,
    /// 2. else primary if the bit is set,
    /// 3. else feature unavailable.
    shadow_mask: AtomicU32,
}

/// Database version router.
///
/// Routes database capabilities to either a primary backend or (during major migrations) an optional
/// shadow backend.
///
/// ## Routing guarantees
/// - The router only returns a backend if the corresponding capability bit is enabled in the
///   backend’s active mask.
/// - Backend selection is lock-free and safe for concurrent use.
/// - Promotion swaps the primary backend atomically; in-flight operations remain valid because they
///   hold their own `Arc<DbBackend>` clones.
impl Router {
    // ***** Router creation *****

    /// Creates a new [`Router`] with `primary` installed as the active backend.
    ///
    /// The primary capability mask is initialized from `primary.capability()`. The shadow backend is
    /// initially unset and must only be configured during major migrations.
    ///
    /// ## Notes
    /// - The router does not validate that `primary.capability()` matches the masks that may later be
    ///   set by migration code; migration orchestration must keep the masks conservative.
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

    /// Returns the database backend that should serve `cap`.
    ///
    /// Routing order:
    /// 1. If the shadow mask contains the requested bit *and* a shadow backend exists, return shadow.
    /// 2. Else if the primary mask contains the requested bit, return primary.
    /// 3. Otherwise return [`FinalisedStateError::FeatureUnavailable`].
    ///
    /// ## Correctness contract
    /// The masks are the source of truth for routing. If migration code enables a bit on the shadow
    /// backend before the corresponding data/index is correct there, callers may observe incorrect
    /// results. Therefore, migrations must only route a capability to shadow once it is complete and
    /// consistent for that capability’s semantics.
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

    /// Installs `shadow` as the current shadow backend and sets its routed capability mask to `caps`.
    ///
    /// This is the entry point for starting a major migration:
    /// - spawn/open the new-version backend,
    /// - call `set_shadow(new_backend, initial_caps)`,
    /// - optionally expand shadow routing incrementally with [`Router::extend_shadow_caps`].
    ///
    /// ## Ordering
    /// The shadow backend pointer is stored first, then the shadow mask is published with `Release`
    /// ordering. Readers use `Acquire` to observe both consistently.
    pub(crate) fn set_shadow(&self, shadow: Arc<DbBackend>, caps: Capability) {
        self.shadow.store(Some(shadow));
        self.shadow_mask.store(caps.bits(), Ordering::Release);
    }

    /// Adds additional capabilities to the shadow routing mask.
    ///
    /// This enables incremental migrations where certain read capabilities can move to the shadow
    /// backend once the corresponding indices are complete there.
    ///
    /// ## Notes
    /// - This only changes routing; it does not validate the shadow backend’s correctness.
    /// - Use conservative routing policies: prefer moving read-only capabilities first.
    pub(crate) fn extend_shadow_caps(&self, caps: Capability) {
        self.shadow_mask.fetch_or(caps.bits(), Ordering::AcqRel);
    }

    /// Promotes the current shadow backend to become the new primary backend.
    ///
    /// Promotion performs the following steps:
    /// - Removes the shadow backend (`shadow = None`).
    /// - Sets `primary_mask` to the promoted backend’s declared capabilities.
    /// - Clears `shadow_mask`.
    /// - Atomically swaps the `primary` backend pointer to the promoted backend.
    ///
    /// Returns the old primary backend so the caller (migration manager) can:
    /// - wait for all outstanding `Arc` clones to drop,
    /// - shut it down,
    /// - and finally remove the old on-disk directory safely.
    ///
    /// # Errors
    /// Returns [`FinalisedStateError::Critical`] if no shadow backend is currently installed.
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

    /// Disables specific capabilities on the primary backend by clearing bits in `primary_mask`.
    ///
    /// This is primarily used during migrations to prevent routing particular operations to the old
    /// backend once the migration wants them served elsewhere.
    ///
    /// ## Safety
    /// This only affects routing. It does not stop in-flight operations already holding an
    /// `Arc<DbBackend>` clone.
    pub(crate) fn limit_primary_caps(&self, caps: Capability) {
        self.primary_mask.fetch_and(!caps.bits(), Ordering::AcqRel);
    }

    /// Enables specific capabilities on the primary backend by setting bits in `primary_mask`.
    ///
    /// This can be used to restore routing to the primary backend after temporarily restricting it.
    pub(crate) fn extend_primary_caps(&self, caps: Capability) {
        self.primary_mask.fetch_or(caps.bits(), Ordering::AcqRel);
    }

    /// Overwrites the entire primary capability mask.
    ///
    /// This is a sharp tool intended for migration orchestration. Prefer incremental helpers
    /// (`limit_primary_caps`, `extend_primary_caps`) unless a full reset is required.
    pub(crate) fn set_primary_mask(&self, new_mask: Capability) {
        self.primary_mask.store(new_mask.bits(), Ordering::Release);
    }
}

// ***** Core DB functionality *****

/// Core database façade implementation for the router.
///
/// `DbCore` methods are routed via capability selection:
/// - `status()` consults the backend that currently serves `READ_CORE`.
/// - `shutdown()` attempts to shut down both primary and shadow backends (if present).
#[async_trait]
impl DbCore for Router {
    /// Returns the runtime status of the database system.
    ///
    /// This is derived from whichever backend currently serves `READ_CORE`. If `READ_CORE` is not
    /// available (misconfiguration or partial migration state), this returns [`StatusType::Busy`]
    /// as a conservative fallback.
    fn status(&self) -> StatusType {
        match self.backend(CapabilityRequest::ReadCore) {
            Ok(backend) => backend.status(),
            Err(_) => StatusType::Busy,
        }
    }

    /// Shuts down both the primary and shadow backends (if any).
    ///
    /// Shutdown is attempted for the primary first, then the shadow. If primary shutdown fails, the
    /// error is returned immediately (the shadow shutdown result is not returned in that case).
    ///
    /// ## Migration note
    /// During major migrations, the old primary backend may need to stay alive until all outstanding
    /// handles are dropped. That waiting logic lives outside the router (typically in the migration
    /// manager).
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

/// Core write surface routed through `WRITE_CORE`.
///
/// All writes are delegated to the backend currently selected for [`CapabilityRequest::WriteCore`].
/// During migrations this allows writers to remain on the old backend until the new backend is ready
/// (or to be switched deliberately by migration orchestration).
#[async_trait]
impl DbWrite for Router {
    /// Writes a block via the backend currently serving `WRITE_CORE`.
    async fn write_block(&self, blk: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .write_block(blk)
            .await
    }

    /// Deletes the block at height `h` via the backend currently serving `WRITE_CORE`.
    async fn delete_block_at_height(&self, h: Height) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .delete_block_at_height(h)
            .await
    }

    /// Deletes the provided block via the backend currently serving `WRITE_CORE`.
    async fn delete_block(&self, blk: &IndexedBlock) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .delete_block(blk)
            .await
    }

    /// Updates the persisted metadata singleton via the backend currently serving `WRITE_CORE`.
    ///
    /// This is used by migrations to record progress and completion status.
    async fn update_metadata(&self, metadata: DbMetadata) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .update_metadata(metadata)
            .await
    }
}

/// Core read surface routed through `READ_CORE`.
///
/// All reads are delegated to the backend currently selected for [`CapabilityRequest::ReadCore`].
/// During migrations this allows reads to continue from the old backend unless/until explicitly
/// moved.
#[async_trait]
impl DbRead for Router {
    /// Returns the database tip height via the backend currently serving `READ_CORE`.
    async fn db_height(&self) -> Result<Option<Height>, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?.db_height().await
    }

    /// Returns the height for `hash` via the backend currently serving `READ_CORE`.
    async fn get_block_height(
        &self,
        hash: BlockHash,
    ) -> Result<Option<Height>, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?
            .get_block_height(hash)
            .await
    }

    /// Returns the hash for `h` via the backend currently serving `READ_CORE`.
    async fn get_block_hash(&self, h: Height) -> Result<Option<BlockHash>, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?
            .get_block_hash(h)
            .await
    }

    /// Returns database metadata via the backend currently serving `READ_CORE`.
    ///
    /// During migrations, callers should expect `DbMetadata::migration_status` to reflect the state
    /// of the active backend selected by routing.
    async fn get_metadata(&self) -> Result<DbMetadata, FinalisedStateError> {
        self.backend(CapabilityRequest::ReadCore)?
            .get_metadata()
            .await
    }
}
