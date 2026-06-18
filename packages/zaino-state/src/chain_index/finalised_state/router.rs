//! Capability-based database router with optional ephemeral service routing.
//!
//! This module implements [`Router`], the internal dispatch layer used by `FinalisedState` to route
//! finalised-state operations to:
//! - the **primary** database backend, which owns the persistent finalised-state database, or
//! - an optional **ephemeral** backend, which serves requests from a backing [`BlockchainSource`]
//!   while the persistent database is syncing or migrating.
//!
//! The router is designed to separate **service routing** from **maintenance writes**:
//! - normal `FinalisedState` reads and writes are routed through [`Router::backend`],
//! - long-running sync uses [`EphemeralMode::ReadOnly`] so reads are served by ephemeral while
//!   `WRITE_CORE` remains available on the primary backend,
//! - migrations use [`EphemeralMode::Full`] so all routed service capabilities, including
//!   `WRITE_CORE`, move to ephemeral while migration code mutates the primary or replacement
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
//! A ephemeral backend allows FinalisedState to keep serving finalised-state requests from the backing
//! validator/source while persistent database work continues in the background.
//!
//! # Routing model
//!
//! Routing is controlled by atomic capability masks:
//! - `ephemeral_mask` controls which capabilities are served by the ephemeral backend,
//! - `primary_mask` controls which capabilities are served by the primary backend.
//!
//! [`Router::backend`] resolves requests in this order:
//! 1. If `ephemeral_mask` contains the requested capability and a ephemeral backend is active,
//!    return ephemeral.
//! 2. Otherwise, if `primary_mask` contains the requested capability, return primary.
//! 3. Otherwise, return [`FinalisedStateError::FeatureUnavailable`].
//!
//! # Ephemeral modes
//!
//! [`EphemeralMode::ReadOnly`] is intended for long-running sync:
//! - read/query capabilities route to ephemeral,
//! - `WRITE_CORE` remains routed to primary,
//! - normal `sync_to_height` can still write through the router unless a full-mode holder is active.
//!
//! [`EphemeralMode::Full`] is intended for migrations:
//! - all ephemeral-supported capabilities route to ephemeral,
//! - `WRITE_CORE` routes to ephemeral instead of primary,
//! - normal routed writes are prevented from mutating the persistent database,
//! - migration code must use explicit maintenance accessors such as [`Router::primary_backend`].
//!
//! # Ephemeral lifetime
//!
//! Ephemeral routing is controlled by [`EphemeralReference`].
//!
//! Calling [`Router::init_or_take_ephemeral`] installs or reuses the ephemeral backend and returns a
//! [`EphemeralReference`]. The caller holds that reference for the entire period during which it
//! needs ephemeral routing to remain active. When the reference is dropped, routing is automatically
//! downgraded or restored.
//!
//! This makes ephemeral routing scope-based:
//!
//! ```text
//! let _ephemeral_reference = router.init_or_take_ephemeral(...).await;
//! // ephemeral routing active
//! // work runs here
//! // ephemeral routing released when `_ephemeral_reference` is dropped
//! ```
//!
//! # Concurrency and atomicity model
//!
//! The router uses:
//! - [`ArcSwap`] for lock-free replacement of the primary backend,
//! - [`ArcSwapOption`] for lock-free publication/removal of the ephemeral backend,
//! - [`AtomicU32`] capability masks for fast capability routing,
//! - a small lifecycle mutex to serialise ephemeral init/release transitions.
//!
//! Backend selection is wait-free and safe for concurrent readers. In-flight operations remain valid
//! because callers receive an [`Arc<FinalisedSource>`] before invoking backend methods.
//!
//! Capability mask updates use explicit memory ordering so routing changes are observed consistently
//! relative to backend pointer publication/removal.
//!
//! # Maintenance access
//!
//! [`Router::primary_backend`] intentionally bypasses service routing. It must only be used by
//! database maintenance code that is allowed to mutate or inspect the persistent backend while
//! ephemeral is serving normal traffic.
//!
//! Normal service code should use routed trait methods (`DbRead`, `DbWrite`, and capability
//! extension routing) rather than calling [`Router::primary_backend`].
//!
//! # Development notes
//!
//! - If a new capability bit is introduced, ensure it is:
//!   - added to `CapabilityRequest`,
//!   - implemented by the relevant [`FinalisedSource`] variants,
//!   - included or excluded deliberately in ephemeral routing policy.
//!
//! - Migrations should use [`EphemeralMode::Full`] and perform persistent database writes through
//!   explicit maintenance accessors.
//!
//! - Long-running sync should use [`EphemeralMode::ReadOnly`] and continue writing through routed
//!   `FinalisedState::write_block` / `Router::write_block`, so a concurrent full-mode migration can freeze
//!   those writes safely.
//!
//! - The current simple drop-based full-mode downgrade assumes there is at most one active
//!   full-mode maintenance operation at a time. If multiple concurrent full-mode operations become
//!   possible, replace the mode handling with explicit full/read-only counters.

use super::{
    capability::{DbCore, DbMetadata, DbRead, DbWrite},
    finalised_source::FinalisedSource,
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

/// Ephemeral routing policy used when installing or reusing the ephemeral backend.
///
/// The selected mode determines which capability bits are routed to ephemeral and which remain on
/// the primary backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EphemeralMode {
    /// Route read/query capabilities through ephemeral while keeping `WRITE_CORE` on primary.
    ///
    /// This mode is intended for long-running sync. It allows FinalisedState to serve reads from the
    /// backing source while normal routed writes continue to append to the persistent database,
    /// unless a concurrent [`EphemeralMode::Full`] holder upgrades routing and freezes writes.
    ReadOnly,

    /// Route all ephemeral-supported capabilities through ephemeral, including `WRITE_CORE`.
    ///
    /// This mode is intended for migrations and maintenance operations that must prevent normal
    /// routed writes from touching the persistent primary database. Migration code must use
    /// explicit maintenance accessors, such as [`Router::primary_backend`], when it needs to mutate
    /// the real database.
    Full,
}

/// Scope guard for active ephemeral routing.
///
/// A `EphemeralReference` is returned by [`Router::init_or_take_ephemeral`]. Holding this value keeps
/// the ephemeral backend installed and keeps the requested [`EphemeralMode`] in effect. Dropping the
/// value automatically releases the caller's ephemeral routing claim.
///
/// The contained backend reference is retained so the router can use ordinary [`Arc`] reference
/// counting to determine whether ephemeral is still in use by other holders.
#[derive(Debug)]
pub(crate) struct EphemeralReference<T>
where
    T: BlockchainSource + Send + Sync + 'static,
{
    /// Router that owns the ephemeral backend and routing masks.
    router: Arc<Router<T>>,

    /// Reference to the active ephemeral backend.
    ///
    /// This is wrapped in `Option` so [`Drop`] can take and release it exactly once.
    ephemeral: Option<Arc<FinalisedSource<T>>>,

    /// Routing mode requested by this reference.
    mode: EphemeralMode,
}

impl<T> EphemeralReference<T>
where
    T: BlockchainSource + Send + Sync + 'static,
{
    fn new(
        router: Arc<Router<T>>,
        ephemeral: Arc<FinalisedSource<T>>,
        mode: EphemeralMode,
    ) -> Self {
        Self {
            router,
            ephemeral: Some(ephemeral),
            mode,
        }
    }

    pub(crate) fn backend(&self) -> &Arc<FinalisedSource<T>> {
        self.ephemeral
            .as_ref()
            .expect("ephemeral reference missing backend")
    }

    pub(crate) fn mode(&self) -> EphemeralMode {
        self.mode
    }
}

impl<T> Drop for EphemeralReference<T>
where
    T: BlockchainSource + Send + Sync + 'static,
{
    fn drop(&mut self) {
        let Some(ephemeral) = self.ephemeral.take() else {
            return;
        };

        self.router
            .release_ephemeral_reference(ephemeral, self.mode);
    }
}

/// Scope guard for an in-progress background operation (sync or migration).
///
/// Returned by [`Router::begin_background_op`], which increments the router's `background_ops`
/// counter in the foreground. The guard is moved into the spawned background task; dropping it
/// (when that task finishes, on any path) decrements the counter. Holding the guard for the whole
/// lifetime of the task is what lets [`FinalisedState::wait_until_synced`] observe that the operation is
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
/// `Router` is the internal dispatch layer used by `FinalisedState`. It routes operations to either:
/// - the **primary** database backend, which owns persistent finalised-state storage, or
/// - an optional **ephemeral** backend, which serves requests from a backing source while the
///   persistent database is syncing or migrating.
///
/// Routing is controlled by capability masks. Ephemeral is checked first, then primary. This allows
/// ephemeral to temporarily take over selected capabilities without replacing the primary backend.
///
/// ## Modes
///
/// Long-running sync uses [`EphemeralMode::ReadOnly`]:
/// - finalised-state reads are served by ephemeral,
/// - `WRITE_CORE` remains on primary,
/// - routed sync writes can continue unless a full-mode migration is active.
///
/// Migrations use [`EphemeralMode::Full`]:
/// - all service capabilities route to ephemeral,
/// - `WRITE_CORE` is removed from primary routing,
/// - normal routed writes cannot mutate the persistent database,
/// - migration code writes through explicit maintenance accessors.
///
/// ## Concurrency model
///
/// Backend pointers are stored using [`ArcSwap`] / [`ArcSwapOption`]. Capability masks are stored in
/// atomics and checked on every routed lookup. Each routed call receives an [`Arc<FinalisedSource<T>>`],
/// so in-flight calls remain valid even if routing changes immediately afterwards.
#[derive(Debug)]
pub(crate) struct Router<T: BlockchainSource> {
    /// Primary active database backend.
    ///
    /// This backend owns the persistent finalised-state database. In steady state, all capabilities
    /// route to this backend.
    ///
    /// During [`EphemeralMode::ReadOnly`], primary keeps `WRITE_CORE` while ephemeral serves reads.
    /// During [`EphemeralMode::Full`], primary is removed from routed service capability so
    /// migrations can work on it through explicit maintenance accessors without normal routed writes
    /// interfering.
    primary: ArcSwap<FinalisedSource<T>>,

    /// Optional ephemeral finalised-state backend.
    ///
    /// This backend is installed while long-running sync or migration work is active. It serves
    /// finalised-state requests from the backing source according to the active [`EphemeralMode`].
    ephemeral: ArcSwapOption<FinalisedSource<T>>,

    /// Serialises ephemeral init/release transitions.
    ///
    /// Routing lookups do not take this lock. The lock only protects lifecycle transitions where the
    /// ephemeral backend is created, removed, or has its capability policy changed.
    ephemeral_lifecycle_lock: Mutex<()>,

    /// Number of active read-only ephemeral routing references.
    ///
    /// This counts only [`EphemeralReference`] holders. It must not be derived from
    /// [`Arc`] strong counts, because normal routed backend calls also clone backend
    /// [`Arc`] handles while operations are in flight.
    ephemeral_read_only_reference_count: AtomicU32,

    /// Number of active full ephemeral routing references.
    ///
    /// While this count is non-zero, routed service capability stays in
    /// [`EphemeralMode::Full`], meaning normal routed writes cannot mutate primary.
    ephemeral_full_reference_count: AtomicU32,

    /// Capability mask for the primary backend.
    ///
    /// A bit being set means the corresponding capability may be served by primary. This mask is
    /// modified when ephemeral routing is active:
    /// - full primary capability in steady state,
    /// - `WRITE_CORE` only during read-only ephemeral routing,
    /// - empty during full ephemeral routing.
    primary_mask: AtomicU32,

    /// Capability mask for the ephemeral backend.
    ///
    /// A bit being set means the corresponding capability should be served by ephemeral if the
    /// ephemeral backend is currently installed.
    ///
    /// This mask is empty in steady state, read-only during [`EphemeralMode::ReadOnly`], and full
    /// ephemeral capability during [`EphemeralMode::Full`].
    ephemeral_mask: AtomicU32,

    /// Number of in-progress background operations (sync and migration).
    ///
    /// Incremented synchronously in the foreground by [`Router::begin_background_op`] before a
    /// background task is spawned, and decremented when the returned [`BackgroundOpGuard`] is
    /// dropped (i.e. when the spawned task completes). This is the source of truth for
    /// [`FinalisedState::wait_until_synced`], which waits for finalised-state sync/migration to finish
    /// without conflating it with serving-readiness ([`StatusType::Ready`]).
    background_ops: AtomicUsize,
}

/// Database capability router.
///
/// `Router` owns the active primary backend and optionally owns a ephemeral backend used for
/// temporary service routing during sync and migration. Normal callers should access backends
/// through [`Router::backend`] or the `DbRead` / `DbWrite` trait implementations. Maintenance code
/// that intentionally bypasses service routing may use [`Router::primary_backend`].
impl<T: BlockchainSource> Router<T> {
    // ***** Router creation *****

    /// Creates a new [`Router`] with `primary` installed as the active backend.
    ///
    /// The primary capability mask is initialized from `primary.capability()`. Ephemeral routing is
    /// initially inactive.
    ///
    /// ## Notes
    ///
    /// The router assumes `primary.capability()` accurately describes the capabilities the backend
    /// can serve. Capability routing policy is enforced by mask changes during ephemeral routing.
    pub(crate) fn new(primary: Arc<FinalisedSource<T>>) -> Self {
        let cap = primary.capability();
        Self {
            primary: ArcSwap::from(primary),
            ephemeral: ArcSwapOption::empty(),
            ephemeral_lifecycle_lock: Mutex::new(()),
            ephemeral_read_only_reference_count: AtomicU32::new(0),
            ephemeral_full_reference_count: AtomicU32::new(0),
            primary_mask: AtomicU32::new(cap.bits()),
            ephemeral_mask: AtomicU32::new(0),
            background_ops: AtomicUsize::new(0),
        }
    }

    // ***** Capability router *****

    /// Returns the backend that should serve `cap` under the current routing policy.
    ///
    /// Routing order:
    /// 1. If the ephemeral mask contains the requested capability and ephemeral is active, return
    ///    ephemeral.
    /// 2. Otherwise, if the primary mask contains the requested capability, return primary.
    /// 3. Otherwise, return [`FinalisedStateError::FeatureUnavailable`].
    ///
    /// ## Correctness contract
    ///
    /// The masks are the source of truth for service routing. During full ephemeral routing,
    /// `WRITE_CORE` is intentionally routed away from primary so normal writes cannot interfere with
    /// migrations. Migration code that must mutate persistent state must use explicit maintenance
    /// accessors instead of routed writes.
    #[inline]
    pub(crate) fn backend(
        &self,
        cap: CapabilityRequest,
    ) -> Result<Arc<FinalisedSource<T>>, FinalisedStateError> {
        let bit = cap.as_capability().bits();

        if self.ephemeral_mask.load(Ordering::Acquire) & bit != 0 {
            if let Some(ephemeral) = self.ephemeral.load().as_ref() {
                return Ok(Arc::clone(ephemeral));
            }
        }
        if self.primary_mask.load(Ordering::Acquire) & bit != 0 {
            return Ok(self.primary.load_full());
        }

        Err(FinalisedStateError::FeatureUnavailable(cap.name()))
    }

    // ***** Ephemeral finalised state control *****
    //
    // These methods should only ever be used by the migration manager.

    /// Installs or reuses ephemeral routing and returns a scope guard for the active ephemeral mode.
    ///
    /// The returned [`EphemeralReference`] must be held for the entire period during which the caller
    /// requires ephemeral routing. When the reference is dropped, routing is automatically released or
    /// downgraded.
    ///
    /// The `db_height` argument is the current persistent on-disk database height that should be
    /// reported by the ephemeral backend while it is serving normal routed reads. This value is
    /// independent of the backing source height.
    ///
    /// ## [`EphemeralMode::ReadOnly`]
    ///
    /// If ephemeral is inactive:
    /// - creates a ephemeral backend,
    /// - initializes its reported persistent database height from `db_height`,
    /// - routes read/query capabilities to ephemeral,
    /// - keeps `WRITE_CORE` routed to primary.
    ///
    /// If ephemeral is already active:
    /// - updates the active ephemeral backend's reported persistent database height,
    /// - returns another reference to the active ephemeral backend,
    /// - does not upgrade write routing unless a full-mode reference is already active.
    ///
    /// This mode is used by long-running sync.
    ///
    /// ## [`EphemeralMode::Full`]
    ///
    /// If ephemeral is inactive:
    /// - creates a ephemeral backend,
    /// - initializes its reported persistent database height from `db_height`,
    /// - routes all ephemeral-supported capabilities to ephemeral,
    /// - removes primary from routed service capability.
    ///
    /// If ephemeral is already active:
    /// - updates the active ephemeral backend's reported persistent database height,
    /// - ensures full ephemeral routing is active,
    /// - returns another reference to the active ephemeral backend.
    ///
    /// This mode is used by migrations.
    pub(crate) async fn init_or_take_ephemeral(
        self: &Arc<Self>,
        source: T,
        network: zebra_chain::parameters::Network,
        mode: EphemeralMode,
        db_height: Option<Height>,
    ) -> Result<EphemeralReference<T>, FinalisedStateError>
    where
        T: Send + Sync + 'static,
    {
        let _ephemeral_lifecycle_guard = self
            .ephemeral_lifecycle_lock
            .lock()
            .expect("ephemeral lifecycle mutex poisoned");

        match mode {
            EphemeralMode::ReadOnly => {
                self.ephemeral_read_only_reference_count
                    .fetch_add(1, Ordering::AcqRel);
            }
            EphemeralMode::Full => {
                self.ephemeral_full_reference_count
                    .fetch_add(1, Ordering::AcqRel);
            }
        }

        let ephemeral = match self.ephemeral.load_full() {
            Some(ephemeral) => {
                match ephemeral.as_ref() {
                    FinalisedSource::Ephemeral(ephemeral_backend) => {
                        ephemeral_backend.update_db_height(db_height)?;
                    }
                    FinalisedSource::V0(_) | FinalisedSource::V1(_) => {
                        self.decrement_ephemeral_reference_count(mode);

                        return Err(FinalisedStateError::Custom(
                            "router ephemeral slot contained a persistent database backend"
                                .to_string(),
                        ));
                    }
                }

                ephemeral
            }
            None => {
                let ephemeral = Arc::new(FinalisedSource::ephemeral(source, network, db_height));
                self.ephemeral.store(Some(Arc::clone(&ephemeral)));
                ephemeral
            }
        };

        let active_mode = self.active_ephemeral_mode().ok_or_else(|| {
            FinalisedStateError::Custom(
                "ephemeral routing mode missing after incrementing reference count".to_string(),
            )
        })?;

        self.apply_ephemeral_mode(ephemeral.as_ref(), active_mode);

        Ok(EphemeralReference::new(Arc::clone(self), ephemeral, mode))
    }

    /// Releases one ephemeral reference.
    ///
    /// This is called automatically from [`EphemeralReference::drop`]. Callers should not call this
    /// directly.
    ///
    /// If the dropped reference was the final ephemeral reference:
    /// - ephemeral routing is disabled,
    /// - full primary capability is restored,
    /// - the ephemeral backend is removed and shut down asynchronously when possible.
    ///
    /// If other ephemeral references remain:
    /// - routing is recalculated from the remaining read-only and full reference counts,
    /// - full mode remains active while at least one full-mode reference exists,
    /// - read-only mode remains active while no full-mode references exist and at least one read-only
    ///   reference exists.
    ///
    /// Full mode takes precedence over read-only mode. Multiple full-mode references are supported.
    fn release_ephemeral_reference(
        &self,
        ephemeral_reference: Arc<FinalisedSource<T>>,
        mode: EphemeralMode,
    ) where
        T: Send + Sync + 'static,
    {
        let ephemeral_to_shutdown = {
            let _ephemeral_lifecycle_guard = self
                .ephemeral_lifecycle_lock
                .lock()
                .expect("ephemeral lifecycle mutex poisoned");

            self.decrement_ephemeral_reference_count(mode);

            let ephemeral_guard = self.ephemeral.load();

            let Some(active_ephemeral) = ephemeral_guard.as_ref() else {
                return;
            };

            if !Arc::ptr_eq(&ephemeral_reference, active_ephemeral) {
                return;
            }

            match self.active_ephemeral_mode() {
                Some(active_mode) => {
                    self.apply_ephemeral_mode(active_ephemeral.as_ref(), active_mode);
                    return;
                }
                None => {
                    self.restore_primary_capability();
                    self.ephemeral.swap(None)
                }
            }
        };

        drop(ephemeral_reference);

        if let Some(ephemeral) = ephemeral_to_shutdown {
            match Handle::try_current() {
                Ok(handle) => {
                    handle.spawn(async move {
                        if let Err(error) = ephemeral.shutdown().await {
                            tracing::warn!("ephemeral shutdown failed during release: {error}");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(
                    "ephemeral backend removed from routing but could not be shut down asynchronously: {error}"
                );
                }
            }
        }
    }

    /// Updates the persistent database height reported by the active ephemeral backend.
    ///
    /// This updates only the optional ephemeral backend currently held by the router. It never touches
    /// the primary backend and does not use capability routing.
    ///
    /// This method is intended for sync and migration progress reporting while ephemeral is serving
    /// normal finalised-state reads. The reported height should reflect the actual persistent on-disk
    /// database height, not the backing source height.
    ///
    /// If no ephemeral backend is active, this method is a no-op. This allows normal sync code to call it
    /// after successful routed writes without needing to know whether ephemeral routing is currently
    /// enabled.
    pub(crate) fn update_ephemeral_db_height(
        &self,
        db_height: Option<Height>,
    ) -> Result<(), FinalisedStateError> {
        let Some(ephemeral) = self.ephemeral.load_full() else {
            return Ok(());
        };

        match ephemeral.as_ref() {
            FinalisedSource::Ephemeral(ephemeral) => ephemeral.update_db_height(db_height),
            FinalisedSource::V0(_) | FinalisedSource::V1(_) => Err(FinalisedStateError::Custom(
                "router ephemeral slot contained a persistent database backend".to_string(),
            )),
        }
    }

    /// Returns `true` if the primary backend is ephemeral.
    ///
    /// This is used by callers that need to avoid starting persistent database work when the router is
    /// running in ephemeral/ephemeral mode.
    pub(crate) fn primary_is_ephemeral(&self) -> bool {
        matches!(self.primary.load().as_ref(), FinalisedSource::Ephemeral(_))
    }

    /// Returns `true` if at least one full-mode ephemeral reference is active.
    ///
    /// While this is true, normal routed writes must not attempt to sync the persistent primary database.
    pub(crate) fn has_full_ephemeral_reference(&self) -> bool {
        self.ephemeral_full_reference_count.load(Ordering::Acquire) > 0
    }

    /// Returns the currently active ephemeral routing mode from ephemeral reference counters.
    ///
    /// Full mode takes precedence over read-only mode. This means any active full-mode
    /// reference keeps normal routed writes frozen until all full-mode references are dropped.
    fn active_ephemeral_mode(&self) -> Option<EphemeralMode> {
        if self.ephemeral_full_reference_count.load(Ordering::Acquire) > 0 {
            Some(EphemeralMode::Full)
        } else if self
            .ephemeral_read_only_reference_count
            .load(Ordering::Acquire)
            > 0
        {
            Some(EphemeralMode::ReadOnly)
        } else {
            None
        }
    }

    /// Returns the primary backend's declared capability set.
    fn primary_capability(&self) -> Capability {
        self.primary.load_full().capability()
    }

    /// Returns the ephemeral capability set used for read-only routing.
    ///
    /// This is the ephemeral backend capability set with `WRITE_CORE` removed.
    fn read_only_ephemeral_capability(ephemeral: &FinalisedSource<T>) -> Capability {
        ephemeral.capability() & !Capability::WRITE_CORE
    }

    /// Returns the primary capability set used while read-only ephemeral routing is active.
    ///
    /// This is normally only `WRITE_CORE`.
    fn primary_write_capability(&self) -> Capability {
        self.primary_capability() & Capability::WRITE_CORE
    }

    /// Applies the routing masks required by `mode`.
    ///
    /// [`EphemeralMode::ReadOnly`] routes read/query capabilities to ephemeral and keeps
    /// `WRITE_CORE` on primary.
    ///
    /// [`EphemeralMode::Full`] routes all ephemeral-supported capabilities to ephemeral and removes
    /// primary from routed service capability.
    fn apply_ephemeral_mode(&self, ephemeral: &FinalisedSource<T>, mode: EphemeralMode) {
        match mode {
            EphemeralMode::ReadOnly => {
                self.ephemeral_mask.store(
                    Self::read_only_ephemeral_capability(ephemeral).bits(),
                    Ordering::Release,
                );
                self.primary_mask
                    .store(self.primary_write_capability().bits(), Ordering::Release);
            }

            EphemeralMode::Full => {
                self.ephemeral_mask
                    .store(ephemeral.capability().bits(), Ordering::Release);
                self.primary_mask.store(0, Ordering::Release);
            }
        }
    }

    /// Restores steady-state routing to the primary backend and disables ephemeral routing.
    fn restore_primary_capability(&self) {
        self.primary_mask
            .store(self.primary_capability().bits(), Ordering::Release);
        self.ephemeral_mask.store(0, Ordering::Release);
    }

    /// Decrements the ephemeral reference count for `mode`.
    ///
    /// This is used when ephemeral initialization fails after the reference count has already been
    /// incremented. Normal ephemeral reference release is handled by [`Router::release_ephemeral_reference`].
    fn decrement_ephemeral_reference_count(&self, mode: EphemeralMode) {
        match mode {
            EphemeralMode::ReadOnly => {
                let previous_reference_count = self
                    .ephemeral_read_only_reference_count
                    .fetch_sub(1, Ordering::AcqRel);

                if previous_reference_count == 0 {
                    self.ephemeral_read_only_reference_count
                        .store(0, Ordering::Release);

                    tracing::warn!("ephemeral read-only reference count underflow");
                }
            }
            EphemeralMode::Full => {
                let previous_reference_count = self
                    .ephemeral_full_reference_count
                    .fetch_sub(1, Ordering::AcqRel);

                if previous_reference_count == 0 {
                    self.ephemeral_full_reference_count
                        .store(0, Ordering::Release);

                    tracing::warn!("ephemeral full reference count underflow");
                }
            }
        }
    }

    // ***** Primary routing *****

    /// Returns the current primary backend, bypassing ephemeral service routing.
    ///
    /// This is a maintenance accessor. It is intended for migrations and database maintenance code
    /// that must intentionally inspect or mutate the persistent backend while normal service traffic
    /// is routed elsewhere.
    ///
    /// Normal read/write service paths should not use this method.
    pub(crate) fn primary_backend(&self) -> Arc<FinalisedSource<T>> {
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
    pub(crate) fn replace_primary(
        &self,
        new_primary: Arc<FinalisedSource<T>>,
    ) -> Arc<FinalisedSource<T>> {
        let _ephemeral_lifecycle_guard = self
            .ephemeral_lifecycle_lock
            .lock()
            .expect("ephemeral lifecycle mutex poisoned");

        let old_primary = self.primary.swap(new_primary);

        match self.ephemeral.load().as_ref() {
            Some(ephemeral) => match self.active_ephemeral_mode() {
                Some(active_mode) => {
                    self.apply_ephemeral_mode(ephemeral.as_ref(), active_mode);
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
    /// after `FinalisedState::spawn` has already returned.
    pub(crate) fn store_primary_status(&self, status: StatusType) {
        self.primary.load_full().store_status(status);
    }

    /// Registers the start of a background operation (sync or migration).
    ///
    /// This increments the `background_ops` counter immediately, in the caller's (foreground) task,
    /// and returns a [`BackgroundOpGuard`] that decrements it on drop. Callers must create the guard
    /// *before* spawning the background task and move it into the spawned future, so the counter is
    /// non-zero from before the spawning method returns until the task completes. That ordering is
    /// what makes [`FinalisedState::wait_until_synced`] race-free.
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
    /// This disables ephemeral routing, removes the ephemeral backend if present, restores primary
    /// capability routing, shuts down the primary backend, and then shuts down the removed ephemeral
    /// backend.
    async fn shutdown(&self) -> Result<(), FinalisedStateError> {
        self.ephemeral_mask.store(0, Ordering::Release);

        let ephemeral = self.ephemeral.swap(None);

        self.primary_mask.store(
            self.primary.load_full().capability().bits(),
            Ordering::Release,
        );

        let primary_shutdown_result = self.primary.load_full().shutdown().await;

        let ephemeral_shutdown_result = match ephemeral {
            Some(ephemeral) => ephemeral.shutdown().await,
            None => Ok(()),
        };

        primary_shutdown_result?;
        ephemeral_shutdown_result?;

        Ok(())
    }
}

/// Core write surface routed through `WRITE_CORE`.
///
/// These methods represent normal service writes. They must use routed backend selection so the
/// router can freeze normal writes during full-mode migrations.
///
/// Migration code that intentionally mutates the persistent database must not use these methods
/// while full ephemeral routing is active; it should use [`Router::primary_backend`] or a dedicated
/// replacement backend.
#[async_trait]
impl<T: BlockchainSource> DbWrite for Router<T> {
    /// Writes a block via the backend currently serving `WRITE_CORE`.
    async fn write_block(&self, blk: IndexedBlock) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .write_block(blk)
            .await
    }

    /// Bulk catch-up ingestion via the backend currently serving `WRITE_CORE`.
    async fn write_blocks_to_height<S: crate::chain_index::source::BlockchainSource>(
        &self,
        height: Height,
        source: &S,
    ) -> Result<(), FinalisedStateError> {
        self.backend(CapabilityRequest::WriteCore)?
            .write_blocks_to_height(height, source)
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
/// These methods represent normal service reads. During ephemeral routing they may be served by the
/// ephemeral backend rather than the persistent primary backend.
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
