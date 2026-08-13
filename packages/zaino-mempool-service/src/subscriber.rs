//! Cheap, cloneable read handle onto the tip-agnostic core mempool.
//!
//! Serves the live set (never frozen): `getrawmempool` / `getmempoolinfo` /
//! `GetMempoolTx`-style reads. The tip-*coherent* reads and the raw-transaction
//! stream live in [`crate::coherence`], behind the `tip_aware_mempool` feature.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;
use zaino_primitives::types::TransactionId;
use zaino_status::{NamedAtomicStatus, StatusType};

use zaino_mempool::config::MempoolConfig;
use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::Mempool;
use zaino_mempool::snapshot::MempoolSnapshot;
use zaino_mempool::update::MempoolUpdate;

mod filter;

pub use filter::{MempoolFilterError, TxIdExcludeSuffix};

/// Aggregate mempool metrics for `getmempoolinfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MempoolInfo {
    /// Number of transactions.
    pub size: u64,
    /// Sum of serialized transaction sizes, in bytes.
    pub bytes: u64,
    /// Approximate memory usage, in bytes (the ZIP-401 cost total).
    pub usage: u64,
}

/// A cheap, cloneable read handle onto the core mempool.
#[derive(Clone)]
pub struct MempoolSubscriber {
    current: Arc<ArcSwap<MempoolSnapshot>>,
    updates: broadcast::Sender<MempoolUpdate>,
    config: MempoolConfig,
    status: NamedAtomicStatus,
}

impl std::fmt::Debug for MempoolSubscriber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MempoolSubscriber")
            .field("status", &self.status.load())
            .finish_non_exhaustive()
    }
}

impl MempoolSubscriber {
    pub(crate) fn new(
        current: Arc<ArcSwap<MempoolSnapshot>>,
        updates: broadcast::Sender<MempoolUpdate>,
        config: MempoolConfig,
        status: NamedAtomicStatus,
    ) -> Self {
        Self {
            current,
            updates,
            config,
            status,
        }
    }

    /// The current immutable snapshot.
    pub fn snapshot(&self) -> Arc<MempoolSnapshot> {
        self.current.load_full()
    }

    /// The mempool service status.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// The current mempool memory bound (max total ZIP-401 cost), in bytes.
    ///
    /// Read-only here by design: the bound is a service-level safety knob, so
    /// it is set on `MempoolService`, not on a read handle that every RPC path
    /// holds a clone of.
    pub fn max_cost_bytes(&self) -> u64 {
        self.config.max_cost_bytes()
    }

    /// Aggregate metrics for `getmempoolinfo`, from the local snapshot.
    ///
    /// Reads through a `load` guard rather than [`snapshot`](Self::snapshot):
    /// the snapshot `Arc` does not escape, and taking one would touch the shared
    /// refcount on a path every reader hits.
    pub fn get_mempool_info(&self) -> MempoolInfo {
        let snapshot = self.current.load();
        MempoolInfo {
            size: snapshot.tx_count() as u64,
            bytes: snapshot.raw_bytes(),
            usage: snapshot.cost_bytes(),
        }
    }

    /// Whether the current snapshot contains `txid`.
    pub fn contains_txid(&self, txid: &TransactionId) -> bool {
        self.current.load().by_txid().contains_key(txid)
    }

    /// The entry for `txid` in the current snapshot, if present.
    pub fn get_transaction(&self, txid: &TransactionId) -> Option<Arc<MempoolEntry>> {
        self.current.load().by_txid().get(txid).cloned()
    }

    /// The current snapshot's txids, sorted by canonical (reversed) byte order.
    pub fn get_txids(&self) -> Arc<[TransactionId]> {
        self.current.load().txids_sorted().clone()
    }

    /// Subscribe to the bounded mempool change feed (the raw receiver).
    ///
    /// Prefer [`mempool_updates`](Self::mempool_updates) unless you need the raw
    /// receiver: it surfaces a lag as `RecvError::Lagged`, which is easy to drop
    /// silently. Honour the consistency contract in the `update` module either
    /// way (subscribe before reading `snapshot`; resync on lag).
    pub fn subscribe_updates(&self) -> broadcast::Receiver<MempoolUpdate> {
        self.updates.subscribe()
    }

    /// The mempool change feed as an ergonomic, hard-to-misuse [`Stream`].
    ///
    /// Yields each [`MempoolUpdate`] in order and, when this consumer falls behind
    /// the bounded feed, an explicit in-band [`MempoolUpdate::Lagged`] — never a
    /// silent skip. On `Lagged` (and on each `Reset` batch boundary) resync from
    /// [`snapshot`](Self::snapshot): no *state* is ever lost, only intermediate
    /// deltas that the fresh snapshot already reflects. The stream ends when the
    /// service closes.
    ///
    /// Subscribing happens when this method is called, so read `snapshot` *after*
    /// calling it to avoid a gap (the module-level contract).
    ///
    /// [`Stream`]: futures::Stream
    pub fn mempool_updates(&self) -> impl futures::Stream<Item = MempoolUpdate> + Send {
        let mut receiver = self.updates.subscribe();
        async_stream::stream! {
            loop {
                match receiver.recv().await {
                    Ok(update) => yield update,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        yield MempoolUpdate::Lagged { missed };
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }

    /// Validate and canonicalize a client-supplied exclude list.
    ///
    /// Enforces the configured aggregate-count and per-suffix length bounds
    /// before any work touches the mempool. Bounding here is the fix for the
    /// unbounded-exclusion-filter denial of service.
    pub fn validate_exclude_suffixes(
        &self,
        exclude_suffixes_client_endian: &[Vec<u8>],
    ) -> Result<Vec<TxIdExcludeSuffix>, MempoolFilterError> {
        if exclude_suffixes_client_endian.len() > self.config.max_exclude_count() {
            return Err(MempoolFilterError::TooManyExcludes {
                actual: exclude_suffixes_client_endian.len(),
                max: self.config.max_exclude_count(),
            });
        }

        exclude_suffixes_client_endian
            .iter()
            .map(|suffix| {
                if suffix.len() < self.config.min_exclude_suffix_len() {
                    return Err(MempoolFilterError::ExcludeSuffixTooShort {
                        actual: suffix.len(),
                        min: self.config.min_exclude_suffix_len(),
                    });
                }
                if suffix.len() > self.config.max_exclude_suffix_len() {
                    return Err(MempoolFilterError::ExcludeSuffixTooLong {
                        actual: suffix.len(),
                        max: self.config.max_exclude_suffix_len(),
                    });
                }
                // Stored verbatim; matched with `ends_with` against internal txid
                // bytes (see `TxIdExcludeSuffix`).
                Ok(TxIdExcludeSuffix {
                    suffix: suffix.clone(),
                })
            })
            .collect()
    }

    /// The current snapshot's entries (in deterministic order) with the uniquely
    /// matched excluded txids removed.
    ///
    /// A suffix that uniquely matches one txid excludes it; a suffix matching
    /// zero txids is ignored; a suffix matching multiple txids excludes none (the
    /// lightwallet shortened-txid contract). Cost is `O(excludes · log n)` via a
    /// binary range search, bounded because `validate_exclude_suffixes` caps the
    /// exclude count and the snapshot is cost-bounded.
    pub fn get_filtered_entries(
        &self,
        exclude_suffixes: &[TxIdExcludeSuffix],
    ) -> Vec<Arc<MempoolEntry>> {
        let snapshot = self.current.load();

        let mut excluded: HashSet<TransactionId> = HashSet::new();
        for exclude in exclude_suffixes {
            if let Some(txid) =
                filter::unique_suffix_match(snapshot.txids_sorted(), &exclude.suffix)
            {
                excluded.insert(txid);
            }
        }

        snapshot
            .entries_in_order()
            .iter()
            .filter(|entry| !excluded.contains(&entry.txid))
            .cloned()
            .collect()
    }
}

impl Mempool for MempoolSubscriber {
    fn current(&self) -> Arc<MempoolSnapshot> {
        self.snapshot()
    }

    fn subscribe_updates(&self) -> broadcast::Receiver<MempoolUpdate> {
        self.updates.subscribe()
    }
}
