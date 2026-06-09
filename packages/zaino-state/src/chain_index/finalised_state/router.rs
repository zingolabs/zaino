//! Capability-based database router with optional stateless service routing.
//!
//! This module implements [`Router`], the internal dispatch layer used by `ZainoDB` to route
//! finalised-state operations to:
//! - the **primary** database backend, which owns the persistent finalised-state database, or
//! - an optional **stateless** backend, which serves requests from a backing [`BlockchainSource`]
//!   while the persistent database is syncing or migrating.
//!
//! The router is designed to separate **service routing** from **maintenance writes**:
//! - normal `ZainoDB` reads and writes are routed through [`Router::backend`],
//! - long-running sync uses [`StatelessMode::ReadOnly`] so reads are served by stateless while
//!   `WRITE_CORE` remains available on the primary backend,
//! - migrations use [`StatelessMode::Full`] so all routed service capabilities, including
//!   `WRITE_CORE`, move to stateless while migration code mutates the primary or replacement
//!   database through explicit maintenance paths such as [`Router::primary_backend`].
//!
//! This prevents normal `write_block` / `sync_to_height` calls from writing to the persistent
//! primary database while a migration is active, while still allowing migration code to update the
//! database deliberately and safely.
//!
//! # Why the router exists
//!
//! The finalised-state database can be unavailable, incomplete, or unsafe to mutate through the
//! normal service path during:
//! - long-running initial or catch-up sync,
//! - in-place minor migrations,
//! - major rebuild migrations,
//! - background maintenance that must freeze normal writes.
//!
//! A stateless backend allows ZainoDB to keep serving finalised-state requests from the backing
//! validator/source while persistent database work continues in the background.
//!
//! # Routing model
//!
//! Routing is controlled by atomic capability masks:
//! - `stateless_mask` controls which capabilities are served by the stateless backend,
//! - `primary_mask` controls which capabilities are served by the primary backend.
//!
//! [`Router::backend`] resolves requests in this order:
//! 1. If `stateless_mask` contains the requested capability and a stateless backend is active,
//!    return stateless.
//! 2. Otherwise, if `primary_mask` contains the requested capability, return primary.
//! 3. Otherwise, return [`FinalisedStateError::FeatureUnavailable`].
//!
//! # Stateless modes
//!
//! [`StatelessMode::ReadOnly`] is intended for long-running sync:
//! - read/query capabilities route to stateless,
//! - `WRITE_CORE` remains routed to primary,
//! - normal `sync_to_height` can still write through the router unless a full-mode holder is active.
//!
//! [`StatelessMode::Full`] is intended for migrations:
//! - all stateless-supported capabilities route to stateless,
//! - `WRITE_CORE` routes to stateless instead of primary,
//! - normal routed writes are prevented from mutating the persistent database,
//! - migration code must use explicit maintenance accessors such as [`Router::primary_backend`].
//!
//! # Stateless lifetime
//!
//! Stateless routing is controlled by [`StatelessReference`].
//!
//! Calling [`Router::init_or_take_stateless`] installs or reuses the stateless backend and returns a
//! [`StatelessReference`]. The caller holds that reference for the entire period during which it
//! needs stateless routing to remain active. When the reference is dropped, routing is automatically
//! downgraded or restored.
//!
//! This makes stateless routing scope-based:
//!
//! ```text
//! let _stateless_reference = router.init_or_take_stateless(...).await;
//! // stateless routing active
//! // work runs here
//! // stateless routing released when `_stateless_reference` is dropped
//! ```
//!
//! # Concurrency and atomicity model
//!
//! The router uses:
//! - [`ArcSwap`] for lock-free replacement of the primary backend,
//! - [`ArcSwapOption`] for lock-free publication/removal of the stateless backend,
//! - [`AtomicU32`] capability masks for fast capability routing,
//! - a small lifecycle mutex to serialise stateless init/release transitions.
//!
//! Backend selection is wait-free and safe for concurrent readers. In-flight operations remain valid
//! because callers receive an [`Arc<DbBackend>`] before invoking backend methods.
//!
//! Capability mask updates use explicit memory ordering so routing changes are observed consistently
//! relative to backend pointer publication/removal.
//!
//! # Maintenance access
//!
//! [`Router::primary_backend`] intentionally bypasses service routing. It must only be used by
//! database maintenance code that is allowed to mutate or inspect the persistent backend while
//! stateless is serving normal traffic.
//!
//! Normal service code should use routed trait methods (`DbRead`, `DbWrite`, and capability
//! extension routing) rather than calling [`Router::primary_backend`].
//!
//! # Development notes
//!
//! - If a new capability bit is introduced, ensure it is:
//!   - added to `CapabilityRequest`,
//!   - implemented by the relevant [`DbBackend`] variants,
//!   - included or excluded deliberately in stateless routing policy.
//!
//! - Migrations should use [`StatelessMode::Full`] and perform persistent database writes through
//!   explicit maintenance accessors.
//!
//! - Long-running sync should use [`StatelessMode::ReadOnly`] and continue writing through routed
//!   `ZainoDB::write_block` / `Router::write_block`, so a concurrent full-mode migration can freeze
//!   those writes safely.
//!
//! - The current simple drop-based full-mode downgrade assumes there is at most one active
//!   full-mode maintenance operation at a time. If multiple concurrent full-mode operations become
//!   possible, replace the mode handling with explicit full/read-only counters.

use super::{
    capability::{DbCore, DbMetadata, DbRead, DbWrite},
    db::DbBackend,
};

use crate::{
    chain_index::finalised_state::capability::{Capability, CapabilityRequest},
    error::FinalisedStateError,
    BlockHash, BlockchainSource, Height, IndexedBlock, StatusType,
};

use arc_swap::{ArcSwap, ArcSwapOption};
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicU32, AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::runtime::Handle;

/// Stateless routing policy used when installing or reusing the stateless backend.
///
/// The selected mode determines which capability bits are routed to stateless and which remain on
/// the primary backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StatelessMode {
    /// Route read/query capabilities through stateless while keeping `WRITE_CORE` on primary.
    ///
    /// This mode is intended for long-running sync. It allows ZainoDB to serve reads from the
    /// backing source while normal routed writes continue to append to the persistent database,
    /// unless a concurrent [`StatelessMode::Full`] holder upgrades routing and freezes writes.
    ReadOnly,

    /// Route all stateless-supported capabilities through stateless, including `WRITE_CORE`.
    ///
    /// This mode is intended for migrations and maintenance operations that must prevent normal
    /// routed writes from touching the persistent primary database. Migration code must use
    /// explicit maintenance accessors, such as [`Router::primary_backend`], when it needs to mutate
    /// the real database.
    Full,
}

/// Scope guard for active stateless routing.
///
/// A `StatelessReference` is returned by [`Router::init_or_take_stateless`]. Holding this value keeps
/// the stateless backend installed and keeps the requested [`StatelessMode`] in effect. Dropping the
/// value automatically releases the caller's stateless routing claim.
///
/// The contained backend reference is retained so the router can use ordinary [`Arc`] reference
/// counting to determine whether stateless is still in use by other holders.
#[derive(Debug)]
pub(crate) struct StatelessReference<T>
where
    T: BlockchainSource + Send + Sync + 'static,
{
    /// Router that owns the stateless backend and routing masks.
    router: Arc<Router<T>>,

    /// Reference to the active stateless backend.
    ///
    /// This is wrapped in `Option` so [`Drop`] can take and release it exactly once.
    stateless: Option<Arc<DbBackend<T>>>,

    /// Routing mode requested by this reference.
    mode: StatelessMode,
}

impl<T> StatelessReference<T>
where
    T: BlockchainSource + Send + Sync + 'static,
{
    fn new(router: Arc<Router<T>>, stateless: Arc<DbBackend<T>>, mode: StatelessMode) -> Self {
        Self {
            router,
            stateless: Some(stateless),
            mode,
        }
    }

    pub(crate) fn backend(&self) -> &Arc<DbBackend<T>> {
        self.stateless
            .as_ref()
            .expect("stateless reference missing backend")
    }

    pub(crate) fn mode(&self) -> StatelessMode {
        self.mode
    }
}

impl<T> Drop for StatelessReference<T>
where
    T: BlockchainSource + Send + Sync + 'static,
{
    fn drop(&mut self) {
        let Some(stateless) = self.stateless.take() else {
            return;
        };

        self.router
            .release_stateless_reference(stateless, self.mode);
    }
}

/// Scope guard for an in-progress background operation (sync or migration).
///
/// Returned by [`Router::begin_background_op`], which increments the router's `background_ops`
/// counter in the foreground. The guard is moved into the spawned background task; dropping it
/// (when that task finishes, on any path) decrements the counter. Holding the guard for the whole
/// lifetime of the task is what lets [`ZainoDB::wait_until_synced`] observe that the operation is
/// still running.
pub(super) struct BackgroundOpGuard<T: BlockchainSource> {
    /// Router whose `background_ops` counter this guard holds a claim on.
    router: Arc<Router<T>>,
}

impl<T: BlockchainSource> Drop for BackgroundOpGuard<T> {
    fn drop(&mut self) {
        self.router.background_ops.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Capability-based database router.
///
/// `Router` is the internal dispatch layer used by `ZainoDB`. It routes operations to either:
/// - the **primary** database backend, which owns persistent finalised-state storage, or
/// - an optional **stateless** backend, which serves requests from a backing source while the
///   persistent database is syncing or migrating.
///
/// Routing is controlled by capability masks. Stateless is checked first, then primary. This allows
/// stateless to temporarily take over selected capabilities without replacing the primary backend.
///
/// ## Modes
///
/// Long-running sync uses [`StatelessMode::ReadOnly`]:
/// - finalised-state reads are served by stateless,
/// - `WRITE_CORE` remains on primary,
/// - routed sync writes can continue unless a full-mode migration is active.
///
/// Migrations use [`StatelessMode::Full`]:
/// - all service capabilities route to stateless,
/// - `WRITE_CORE` is removed from primary routing,
/// - normal routed writes cannot mutate the persistent database,
/// - migration code writes through explicit maintenance accessors.
///
/// ## Concurrency model
///
/// Backend pointers are stored using [`ArcSwap`] / [`ArcSwapOption`]. Capability masks are stored in
/// atomics and checked on every routed lookup. Each routed call receives an [`Arc<DbBackend<T>>`],
/// so in-flight calls remain valid even if routing changes immediately afterwards.
#[derive(Debug)]
pub(crate) struct Router<T: BlockchainSource> {
    /// Primary active database backend.
    ///
    /// This backend owns the persistent finalised-state database. In steady state, all capabilities
    /// route to this backend.
    ///
    /// During [`StatelessMode::ReadOnly`], primary keeps `WRITE_CORE` while stateless serves reads.
    /// During [`StatelessMode::Full`], primary is removed from routed service capability so
    /// migrations can work on it through explicit maintenance accessors without normal routed writes
    /// interfering.
    primary: ArcSwap<DbBackend<T>>,

    /// Optional stateless finalised-state backend.
    ///
    /// This backend is installed while long-running sync or migration work is active. It serves
    /// finalised-state requests from the backing source according to the active [`StatelessMode`].
    stateless: ArcSwapOption<DbBackend<T>>,

    /// Serialises stateless init/release transitions.
    ///
    /// Routing lookups do not take this lock. The lock only protects lifecycle transitions where the
    /// stateless backend is created, removed, or has its capability policy changed.
    stateless_lifecycle_lock: Mutex<()>,

    /// Number of active read-only stateless routing references.
    ///
    /// This counts only [`StatelessReference`] holders. It must not be derived from
    /// [`Arc`] strong counts, because normal routed backend calls also clone backend
    /// [`Arc`] handles while operations are in flight.
    stateless_read_only_reference_count: AtomicU32,

    /// Number of active full stateless routing references.
    ///
    /// While this count is non-zero, routed service capability stays in
    /// [`StatelessMode::Full`], meaning normal routed writes cannot mutate primary.
    stateless_full_reference_count: AtomicU32,

    /// Capability mask for the primary backend.
    ///
    /// A bit being set means the corresponding capability may be served by primary. This mask is
    /// modified when stateless routing is active:
    /// - full primary capability in steady state,
    /// - `WRITE_CORE` only during read-only stateless routing,
    /// - empty during full stateless routing.
    primary_mask: AtomicU32,

    /// Capability mask for the stateless backend.
    ///
    /// A bit being set means the corresponding capability should be served by stateless if the
    /// stateless backend is currently installed.
    ///
    /// This mask is empty in steady state, read-only during [`StatelessMode::ReadOnly`], and full
    /// stateless capability during [`StatelessMode::Full`].
    stateless_mask: AtomicU32,

    /// Number of in-progress background operations (sync and migration).
    ///
    /// Incremented synchronously in the foreground by [`Router::begin_background_op`] before a
    /// background task is spawned, and decremented when the returned [`BackgroundOpGuard`] is
    /// dropped (i.e. when the spawned task completes). This is the source of truth for
    /// [`ZainoDB::wait_until_synced`], which waits for finalised-state sync/migration to finish
    /// without conflating it with serving-readiness ([`StatusType::Ready`]).
    background_ops: AtomicUsize,
}

/// Database capability router.
///
/// `Router` owns the active primary backend and optionally owns a stateless backend used for
/// temporary service routing during sync and migration. Normal callers should access backends
/// through [`Router::backend`] or the `DbRead` / `DbWrite` trait implementations. Maintenance code
/// that intentionally bypasses service routing may use [`Router::primary_backend`].
impl<T: BlockchainSource> Router<T> {
    // ***** Router creation *****

    /// Creates a new [`Router`] with `primary` installed as the active backend.
    ///
    /// The primary capability mask is initialized from `primary.capability()`. Stateless routing is
    /// initially inactive.
    ///
    /// ## Notes
    ///
    /// The router assumes `primary.capability()` accurately describes the capabilities the backend
    /// can serve. Capability routing policy is enforced by mask changes during stateless routing.
    pub(crate) fn new(primary: Arc<DbBackend<T>>) -> Self {
        let cap = primary.capability();
        Self {
            primary: ArcSwap::from(primary),
            stateless: ArcSwapOption::empty(),
            stateless_lifecycle_lock: Mutex::new(()),
            stateless_read_only_reference_count: AtomicU32::new(0),
            stateless_full_reference_count: AtomicU32::new(0),
            primary_mask: AtomicU32::new(cap.bits()),
            stateless_mask: AtomicU32::new(0),
            background_ops: AtomicUsize::new(0),
        }
    }

    // ***** Capability router *****

    /// Returns the backend that should serve `cap` under the current routing policy.
    ///
    /// Routing order:
    /// 1. If the stateless mask contains the requested capability and stateless is active, return
    ///    stateless.
    /// 2. Otherwise, if the primary mask contains the requested capability, return primary.
    /// 3. Otherwise, return [`FinalisedStateError::FeatureUnavailable`].
    ///
    /// ## Correctness contract
    ///
    /// The masks are the source of truth for service routing. During full stateless routing,
    /// `WRITE_CORE` is intentionally routed away from primary so normal writes cannot interfere with
    /// migrations. Migration code that must mutate persistent state must use explicit maintenance
    /// accessors instead of routed writes.
    #[inline]
    pub(crate) fn backend(
        &self,
        cap: CapabilityRequest,
    ) -> Result<Arc<DbBackend<T>>, FinalisedStateError> {
        let bit = cap.as_capability().bits();

        if self.stateless_mask.load(Ordering::Acquire) & bit != 0 {
            if let Some(stateless) = self.stateless.load().as_ref() {
                return Ok(Arc::clone(stateless));
            }
        }
        if self.primary_mask.load(Ordering::Acquire) & bit != 0 {
            return Ok(self.primary.load_full());
        }

        Err(FinalisedStateError::FeatureUnavailable(cap.name()))
    }

    // ***** Stateless finalised state control *****
    //
    // These methods should only ever be used by the migration manager.

    /// Installs or reuses stateless routing and returns a scope guard for the active stateless mode.
    ///
    /// The returned [`StatelessReference`] must be held for the entire period during which the caller
    /// requires stateless routing. When the reference is dropped, routing is automatically released or
    /// downgraded.
    ///
    /// The `db_height` argument is the current persistent on-disk database height that should be
    /// reported by the stateless backend while it is serving normal routed reads. This value is
    /// independent of the backing source height.
    ///
    /// ## [`StatelessMode::ReadOnly`]
    ///
    /// If stateless is inactive:
    /// - creates a stateless backend,
    /// - initializes its reported persistent database height from `db_height`,
    /// - routes read/query capabilities to stateless,
    /// - keeps `WRITE_CORE` routed to primary.
    ///
    /// If stateless is already active:
    /// - updates the active stateless backend's reported persistent database height,
    /// - returns another reference to the active stateless backend,
    /// - does not upgrade write routing unless a full-mode reference is already active.
    ///
    /// This mode is used by long-running sync.
    ///
    /// ## [`StatelessMode::Full`]
    ///
    /// If stateless is inactive:
    /// - creates a stateless backend,
    /// - initializes its reported persistent database height from `db_height`,
    /// - routes all stateless-supported capabilities to stateless,
    /// - removes primary from routed service capability.
    ///
    /// If stateless is already active:
    /// - updates the active stateless backend's reported persistent database height,
    /// - ensures full stateless routing is active,
    /// - returns another reference to the active stateless backend.
    ///
    /// This mode is used by migrations.
    pub(crate) async fn init_or_take_stateless(
        self: &Arc<Self>,
        source: T,
        network: zebra_chain::parameters::Network,
        mode: StatelessMode,
        db_height: Option<Height>,
    ) -> Result<StatelessReference<T>, FinalisedStateError>
    where
        T: Send + Sync + 'static,
    {
        let _stateless_lifecycle_guard = self
            .stateless_lifecycle_lock
            .lock()
            .expect("stateless lifecycle mutex poisoned");

        match mode {
            StatelessMode::ReadOnly => {
                self.stateless_read_only_reference_count
                    .fetch_add(1, Ordering::AcqRel);
            }
            StatelessMode::Full => {
                self.stateless_full_reference_count
                    .fetch_add(1, Ordering::AcqRel);
            }
        }

        let stateless = match self.stateless.load_full() {
            Some(stateless) => {
                match stateless.as_ref() {
                    DbBackend::Stateless(stateless_backend) => {
                        stateless_backend.update_db_height(db_height)?;
                    }
                    DbBackend::V0(_) | DbBackend::V1(_) => {
                        self.decrement_stateless_reference_count(mode);

                        return Err(FinalisedStateError::Custom(
                            "router stateless slot contained a persistent database backend"
                                .to_string(),
                        ));
                    }
                }

                stateless
            }
            None => {
                let stateless = Arc::new(DbBackend::stateless(source, network, db_height));
                self.stateless.store(Some(Arc::clone(&stateless)));
                stateless
            }
        };

        let active_mode = self.active_stateless_mode().ok_or_else(|| {
            FinalisedStateError::Custom(
                "stateless routing mode missing after incrementing reference count".to_string(),
            )
        })?;

        self.apply_stateless_mode(stateless.as_ref(), active_mode);

        Ok(StatelessReference::new(Arc::clone(self), stateless, mode))
    }

    /// Releases one stateless reference.
    ///
    /// This is called automatically from [`StatelessReference::drop`]. Callers should not call this
    /// directly.
    ///
    /// If the dropped reference was the final stateless reference:
    /// - stateless routing is disabled,
    /// - full primary capability is restored,
    /// - the stateless backend is removed and shut down asynchronously when possible.
    ///
    /// If other stateless references remain:
    /// - routing is recalculated from the remaining read-only and full reference counts,
    /// - full mode remains active while at least one full-mode reference exists,
    /// - read-only mode remains active while no full-mode references exist and at least one read-only
    ///   reference exists.
    ///
    /// Full mode takes precedence over read-only mode. Multiple full-mode references are supported.
    fn release_stateless_reference(
        &self,
        stateless_reference: Arc<DbBackend<T>>,
        mode: StatelessMode,
    ) where
        T: Send + Sync + 'static,
    {
        let stateless_to_shutdown = {
            let _stateless_lifecycle_guard = self
                .stateless_lifecycle_lock
                .lock()
                .expect("stateless lifecycle mutex poisoned");

            self.decrement_stateless_reference_count(mode);

            let stateless_guard = self.stateless.load();

            let Some(active_stateless) = stateless_guard.as_ref() else {
                return;
            };

            if !Arc::ptr_eq(&stateless_reference, active_stateless) {
                return;
            }

            match self.active_stateless_mode() {
                Some(active_mode) => {
                    self.apply_stateless_mode(active_stateless.as_ref(), active_mode);
                    return;
                }
                None => {
                    self.restore_primary_capability();
                    self.stateless.swap(None)
                }
            }
        };

        drop(stateless_reference);

        if let Some(stateless) = stateless_to_shutdown {
            match Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        if let Err(error) = stateless.shutdown().await {
                            tracing::warn!("stateless shutdown failed during release: {error}");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(
                    "stateless backend removed from routing but could not be shut down asynchronously: {error}"
                );
                }
            }
        }
    }

    /// Updates the persistent database height reported by the active stateless backend.
    ///
    /// This updates only the optional stateless backend currently held by the router. It never touches
    /// the primary backend and does not use capability routing.
    ///
    /// This method is intended for sync and migration progress reporting while stateless is serving
    /// normal finalised-state reads. The reported height should reflect the actual persistent on-disk
    /// database height, not the backing source height.
    ///
    /// If no stateless backend is active, this method is a no-op. This allows normal sync code to call it
    /// after successful routed writes without needing to know whether stateless routing is currently
    /// enabled.
    pub(crate) fn update_stateless_db_height(
        &self,
        db_height: Option<Height>,
    ) -> Result<(), FinalisedStateError> {
        let Some(stateless) = self.stateless.load_full() else {
            return Ok(());
        };

        match stateless.as_ref() {
            DbBackend::Stateless(stateless) => stateless.update_db_height(db_height),
            DbBackend::V0(_) | DbBackend::V1(_) => Err(FinalisedStateError::Custom(
                "router stateless slot contained a persistent database backend".to_string(),
            )),
        }
    }

    /// Returns `true` if the primary backend is stateless.
    ///
    /// This is used by callers that need to avoid starting persistent database work when the router is
    /// running in ephemeral/stateless mode.
    pub(crate) fn primary_is_stateless(&self) -> bool {
        matches!(self.primary.load().as_ref(), DbBackend::Stateless(_))
    }

    /// Returns `true` if at least one full-mode stateless reference is active.
    ///
    /// While this is true, normal routed writes must not attempt to sync the persistent primary database.
    pub(crate) fn has_full_stateless_reference(&self) -> bool {
        self.stateless_full_reference_count.load(Ordering::Acquire) > 0
    }

    /// Returns the currently active stateless routing mode from stateless reference counters.
    ///
    /// Full mode takes precedence over read-only mode. This means any active full-mode
    /// reference keeps normal routed writes frozen until all full-mode references are dropped.
    fn active_stateless_mode(&self) -> Option<StatelessMode> {
        if self.stateless_full_reference_count.load(Ordering::Acquire) > 0 {
            Some(StatelessMode::Full)
        } else if self
            .stateless_read_only_reference_count
            .load(Ordering::Acquire)
            > 0
        {
            Some(StatelessMode::ReadOnly)
        } else {
            None
        }
    }

    /// Returns the primary backend's declared capability set.
    fn primary_capability(&self) -> Capability {
        self.primary.load_full().capability()
    }

    /// Returns the stateless capability set used for read-only routing.
    ///
    /// This is the stateless backend capability set with `WRITE_CORE` removed.
    fn read_only_stateless_capability(stateless: &DbBackend<T>) -> Capability {
        stateless.capability() & !Capability::WRITE_CORE
    }

    /// Returns the primary capability set used while read-only stateless routing is active.
    ///
    /// This is normally only `WRITE_CORE`.
    fn primary_write_capability(&self) -> Capability {
        self.primary_capability() & Capability::WRITE_CORE
    }

    /// Applies the routing masks required by `mode`.
    ///
    /// [`StatelessMode::ReadOnly`] routes read/query capabilities to stateless and keeps
    /// `WRITE_CORE` on primary.
    ///
    /// [`StatelessMode::Full`] routes all stateless-supported capabilities to stateless and removes
    /// primary from routed service capability.
    fn apply_stateless_mode(&self, stateless: &DbBackend<T>, mode: StatelessMode) {
        match mode {
            StatelessMode::ReadOnly => {
                self.stateless_mask.store(
                    Self::read_only_stateless_capability(stateless).bits(),
                    Ordering::Release,
                );
                self.primary_mask
                    .store(self.primary_write_capability().bits(), Ordering::Release);
            }

            StatelessMode::Full => {
                self.stateless_mask
                    .store(stateless.capability().bits(), Ordering::Release);
                self.primary_mask.store(0, Ordering::Release);
            }
        }
    }

    /// Restores steady-state routing to the primary backend and disables stateless routing.
    fn restore_primary_capability(&self) {
        self.primary_mask
            .store(self.primary_capability().bits(), Ordering::Release);
        self.stateless_mask.store(0, Ordering::Release);
    }

    /// Decrements the stateless reference count for `mode`.
    ///
    /// This is used when stateless initialization fails after the reference count has already been
    /// incremented. Normal stateless reference release is handled by [`Router::release_stateless_reference`].
    fn decrement_stateless_reference_count(&self, mode: StatelessMode) {
        match mode {
            StatelessMode::ReadOnly => {
                let previous_reference_count = self
                    .stateless_read_only_reference_count
                    .fetch_sub(1, Ordering::AcqRel);

                if previous_reference_count == 0 {
                    self.stateless_read_only_reference_count
                        .store(0, Ordering::Release);

                    tracing::warn!("stateless read-only reference count underflow");
                }
            }
            StatelessMode::Full => {
                let previous_reference_count = self
                    .stateless_full_reference_count
                    .fetch_sub(1, Ordering::AcqRel);

                if previous_reference_count == 0 {
                    self.stateless_full_reference_count
                        .store(0, Ordering::Release);

                    tracing::warn!("stateless full reference count underflow");
                }
            }
        }
    }

    // ***** Primary routing *****

    /// Returns the current primary backend, bypassing stateless service routing.
    ///
    /// This is a maintenance accessor. It is intended for migrations and database maintenance code
    /// that must intentionally inspect or mutate the persistent backend while normal service traffic
    /// is routed elsewhere.
    ///
    /// Normal read/write service paths should not use this method.
    pub(crate) fn primary_backend(&self) -> Arc<DbBackend<T>> {
        self.primary.load_full()
    }

    /// Replaces the primary backend and returns the old primary backend.
    ///
    /// This is a maintenance operation used by rebuild-style migrations after a replacement backend
    /// has been fully built and validated.
    ///
    /// The primary capability mask is updated to the new backend's declared capability set before the
    /// pointer swap. Existing in-flight operations remain valid because they hold [`Arc`] clones of
    /// the old backend.
    pub(crate) fn replace_primary(&self, new_primary: Arc<DbBackend<T>>) -> Arc<DbBackend<T>> {
        let _stateless_lifecycle_guard = self
            .stateless_lifecycle_lock
            .lock()
            .expect("stateless lifecycle mutex poisoned");

        let old_primary = self.primary.swap(new_primary);

        match self.stateless.load().as_ref() {
            Some(stateless) => match self.active_stateless_mode() {
                Some(active_mode) => {
                    self.apply_stateless_mode(stateless.as_ref(), active_mode);
                }
                None => {
                    self.restore_primary_capability();
                }
            },
            None => {
                self.restore_primary_capability();
            }
        }

        old_primary
    }

    /// Stores a runtime status in the current primary backend.
    ///
    /// This is a maintenance/status hook. It intentionally bypasses service capability routing and
    /// updates only the primary backend's existing status field.
    ///
    /// It is used to report background maintenance failures, such as an asynchronous migration failure,
    /// after `ZainoDB::spawn` has already returned.
    pub(crate) fn store_primary_status(&self, status: StatusType) {
        self.primary.load_full().store_status(status);
    }

    /// Registers the start of a background operation (sync or migration).
    ///
    /// This increments the `background_ops` counter immediately, in the caller's (foreground) task,
    /// and returns a [`BackgroundOpGuard`] that decrements it on drop. Callers must create the guard
    /// *before* spawning the background task and move it into the spawned future, so the counter is
    /// non-zero from before the spawning method returns until the task completes. That ordering is
    /// what makes [`ZainoDB::wait_until_synced`] race-free.
    pub(super) fn begin_background_op(self: &Arc<Self>) -> BackgroundOpGuard<T> {
        self.background_ops.fetch_add(1, Ordering::AcqRel);
        BackgroundOpGuard {
            router: Arc::clone(self),
        }
    }

    /// Returns `true` while at least one background operation (sync or migration) is in progress.
    pub(super) fn has_background_ops(&self) -> bool {
        self.background_ops.load(Ordering::Acquire) != 0
    }
}

// ***** Core DB functionality *****

/// Core database façade implementation for the router.
///
/// `DbCore` methods are routed via capability selection:
/// - `status()` consults the backend that currently serves `READ_CORE`.
/// - `shutdown()` attempts to shut down both primary and shadow backends (if present).
#[async_trait]
impl<T: BlockchainSource> DbCore for Router<T> {
    /// Returns the runtime status of the database system.
    ///
    /// This is derived from whichever backend currently serves `READ_CORE`. If `READ_CORE` is not
    /// available (misconfiguration or partial migration state), this returns [`StatusType::Busy`]
    /// as a conservative fallback.
    fn status(&self) -> StatusType {
        let primary_status = self.primary.load_full().status();

        if primary_status == StatusType::CriticalError {
            return primary_status;
        }

        match self.backend(CapabilityRequest::ReadCore) {
            Ok(backend) => backend.status(),
            Err(_) => StatusType::Busy,
        }
    }

    /// Shuts down the router's active backends.
    ///
    /// This disables stateless routing, removes the stateless backend if present, restores primary
    /// capability routing, shuts down the primary backend, and then shuts down the removed stateless
    /// backend.
    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.stateless_mask.store(0, Ordering::Release);

        let stateless = self.stateless.swap(None);

        self.primary_mask.store(
            self.primary.load_full().capability().bits(),
            Ordering::Release,
        );

        let primary_shutdown_result = self.primary.load_full().shutdown().await;

        let stateless_shutdown_result = match stateless {
            Some(stateless) => stateless.shutdown().await,
            None => Ok(()),
        };

        primary_shutdown_result?;
        stateless_shutdown_result?;

        Ok(())
    }
}

/// Core write surface routed through `WRITE_CORE`.
///
/// These methods represent normal service writes. They must use routed backend selection so the
/// router can freeze normal writes during full-mode migrations.
///
/// Migration code that intentionally mutates the persistent database must not use these methods
/// while full stateless routing is active; it should use [`Router::primary_backend`] or a dedicated
/// replacement backend.
#[async_trait]
impl<T: BlockchainSource> DbWrite for Router<T> {
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
/// These methods represent normal service reads. During stateless routing they may be served by the
/// stateless backend rather than the persistent primary backend.
#[async_trait]
impl<T: BlockchainSource> DbRead for Router<T> {
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
