//! The core mempool service: a single writer task that mirrors the validator's
//! mempool as an immutable, bounded, **tip-agnostic** read model.
//!
//! # Tip-agnostic, but tip-tagged
//!
//! This service never freezes. Each poll it diffs the validator's current mempool
//! against the held set and applies the delta, so tip-agnostic reads
//! (`GetMempoolTx`, `getrawmempool`, `getmempoolinfo`) always serve the live set
//! — even mid-reorg. It does, however, **tag** every published snapshot with the
//! validator tip it was fetched at ([`MempoolSnapshot::source_tip`]), read from the
//! *same* source that serves the mempool data. That single-source tag is what lets
//! the optional coherence layer (`crate::coherence`) decide `V == NS` without
//! re-fetching; without it, coherence would have to correlate the set and the tip
//! across two independent reads — the race the rework closed.
//!
//! The only bound the core enforces itself is the ZIP-401 capacity backstop:
//! additions are admitted in the set's deterministic order up to
//! `max_cost_bytes`; those that would breach it are refused, remembered, and the
//! set is marked
//! [`IncompleteCapacityLimited`](zaino_mempool::snapshot::MempoolCompleteness::IncompleteCapacityLimited)
//! rather than exceeding the bound.

use std::hash::BuildHasher as _;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zaino_status::{NamedAtomicStatus, StatusType};

use crate::subscriber::MempoolSubscriber;
use zaino_mempool::config::MempoolConfig;
use zaino_mempool::ports::MempoolSource;
use zaino_mempool::snapshot::MempoolSnapshot;
use zaino_mempool::update::MempoolUpdate;

mod poll;
mod publish;
mod state;

/// The core mempool read-model service.
///
/// Generic over its one outbound port ([`MempoolSource`]) so the core has no
/// `zaino-state` dependency and no chain-tip knowledge beyond the tag it stamps.
pub struct MempoolService<S: MempoolSource> {
    source: S,
    current: Arc<ArcSwap<MempoolSnapshot>>,
    updates: broadcast::Sender<MempoolUpdate>,
    config: MempoolConfig,
    status: NamedAtomicStatus,
    cancel: CancellationToken,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Per-process salt for the admission tiebreak (see
    /// [`admission_key`](state::admission_key)).
    admission_salt: u64,
}

impl<S: MempoolSource> std::fmt::Debug for MempoolService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MempoolService")
            .field("status", &self.status.load())
            .finish_non_exhaustive()
    }
}

impl<S: MempoolSource> MempoolService<S> {
    /// Spawn the core service and its background poll task.
    ///
    /// The admission tiebreak salt is drawn per process, so the order additions
    /// take at capacity is not predictable to a transaction's sender (see the
    /// `admission_key` tiebreak).
    pub fn spawn(source: S, config: MempoolConfig, cancel: CancellationToken) -> Arc<Self> {
        // `RandomState` is the std hasher's per-process random seed; hashing a
        // fixed value through it yields a stable random salt without pulling in
        // an RNG dependency.
        let salt = std::collections::hash_map::RandomState::new().hash_one(0u8);
        Self::spawn_with_admission_salt(source, config, cancel, salt)
    }

    /// [`Self::spawn`] with an explicit admission salt.
    ///
    /// Exists so tests can pin the tiebreak and assert its *properties*
    /// deterministically (same salt ⇒ same order, different salt ⇒ different
    /// order) rather than by sampling a random one.
    pub fn spawn_with_admission_salt(
        source: S,
        config: MempoolConfig,
        cancel: CancellationToken,
        admission_salt: u64,
    ) -> Arc<Self> {
        let (updates, _) = broadcast::channel(config.event_buffer_len());

        let service = Arc::new(Self {
            source,
            current: Arc::new(ArcSwap::from_pointee(MempoolSnapshot::empty())),
            updates,
            config,
            status: NamedAtomicStatus::new("Mempool", StatusType::Spawning),
            cancel,
            task: std::sync::Mutex::new(None),
            admission_salt,
        });

        let task_service = Arc::clone(&service);
        let handle = tokio::spawn(async move {
            task_service.run().await;
        });

        *service.task.lock().expect("mempool task lock poisoned") = Some(handle);

        service
    }

    /// A cheap, cloneable read handle onto the core mempool.
    pub fn subscriber(&self) -> MempoolSubscriber {
        MempoolSubscriber::new(
            Arc::clone(&self.current),
            self.updates.clone(),
            self.config.clone(),
            self.status.clone(),
        )
    }

    /// Current service status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// The current mempool memory bound (max total ZIP-401 cost), in bytes.
    pub fn max_cost_bytes(&self) -> u64 {
        self.config.max_cost_bytes()
    }

    /// Adjust the mempool memory bound at runtime; takes effect on the next
    /// poll. Lowering it below the current set does not evict — the set shrinks
    /// as transactions are mined, and additions are refused meanwhile.
    ///
    /// Deliberately on the service, not on the read handles: it is a
    /// capacity-control knob for whoever owns the mempool, and reaching it
    /// through a cloneable read handle would make every RPC path a freeze
    /// switch.
    pub fn set_max_cost_bytes(&self, bytes: u64) {
        self.config.set_max_cost_bytes(bytes);
    }

    /// Signal shutdown: publish the `Closing` update, then stop the task.
    ///
    /// `Closing` is published synchronously here (rather than relying on the
    /// background task to observe cancellation first) so subscribers reliably see
    /// it even though the task is aborted immediately after.
    pub fn close(&self) {
        self.publish_closing();
        self.cancel.cancel();
        if let Some(handle) = self.task.lock().expect("mempool task lock poisoned").take() {
            handle.abort();
        }
    }
}
