//! Cheap, cloneable read handle onto the tip-agnostic core mempool.
//!
//! Serves the live set (never frozen): `getrawmempool` / `getmempoolinfo` /
//! `GetMempoolTx`-style reads. The tip-*coherent* reads and the raw-transaction
//! stream live in [`crate::coherence`], behind the `tip_aware_mempool` feature.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;
use zaino_common::status::{NamedAtomicStatus, StatusType};
use zebra_chain::transaction::Hash as TxHash;

use zaino_mempool::config::MempoolConfig;
use zaino_mempool::entry::MempoolEntry;
use zaino_mempool::ports::Mempool;
use zaino_mempool::snapshot::MempoolSnapshot;
use zaino_mempool::update::MempoolUpdate;

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

/// A validated client-supplied exclude suffix.
///
/// The lightwallet protocol's `exclude_txid_suffixes` are the trailing bytes of
/// the txid in internal (little-endian) byte order; a transaction is excluded
/// when its txid **ends with** these bytes (equivalently, its big-endian display
/// hex starts with the reversed bytes — the form lightwalletd matches). The
/// bytes are stored exactly as supplied and matched with [`slice::ends_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxIdExcludeSuffix {
    suffix: Vec<u8>,
}

/// Errors validating a client-supplied exclude list.
#[derive(Debug, thiserror::Error)]
pub enum MempoolFilterError {
    /// The exclude list exceeds the configured cap.
    #[error("exclude list too large: {actual} > {max}")]
    TooManyExcludes {
        /// Supplied count.
        actual: usize,
        /// Configured maximum.
        max: usize,
    },
    /// A suffix is shorter than the configured minimum.
    #[error("exclude suffix too short: {actual} < {min}")]
    ExcludeSuffixTooShort {
        /// Supplied length.
        actual: usize,
        /// Configured minimum.
        min: usize,
    },
    /// A suffix is longer than the configured maximum.
    #[error("exclude suffix too long: {actual} > {max}")]
    ExcludeSuffixTooLong {
        /// Supplied length.
        actual: usize,
        /// Configured maximum.
        max: usize,
    },
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
    pub fn max_cost_bytes(&self) -> u64 {
        self.config.max_cost_bytes()
    }

    /// Adjust the mempool memory bound at runtime. Shared with the service and
    /// every other subscriber; takes effect on the next update.
    pub fn set_max_cost_bytes(&self, bytes: u64) {
        self.config.set_max_cost_bytes(bytes);
    }

    /// Aggregate metrics for `getmempoolinfo`, from the local snapshot.
    pub fn get_mempool_info(&self) -> MempoolInfo {
        let snapshot = self.snapshot();
        MempoolInfo {
            size: snapshot.tx_count as u64,
            bytes: snapshot.raw_bytes,
            usage: snapshot.cost_bytes,
        }
    }

    /// Whether the current snapshot contains `txid`.
    pub fn contains_txid(&self, txid: &TxHash) -> bool {
        self.snapshot().by_txid.contains_key(txid)
    }

    /// The entry for `txid` in the current snapshot, if present.
    pub fn get_transaction(&self, txid: &TxHash) -> Option<Arc<MempoolEntry>> {
        self.snapshot().by_txid.get(txid).cloned()
    }

    /// The current snapshot's txids, sorted by canonical (reversed) byte order.
    pub fn get_txids(&self) -> Arc<[TxHash]> {
        self.snapshot().txids_sorted.clone()
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
        if exclude_suffixes_client_endian.len() > self.config.max_exclude_count {
            return Err(MempoolFilterError::TooManyExcludes {
                actual: exclude_suffixes_client_endian.len(),
                max: self.config.max_exclude_count,
            });
        }

        exclude_suffixes_client_endian
            .iter()
            .map(|suffix| {
                if suffix.len() < self.config.min_exclude_suffix_len {
                    return Err(MempoolFilterError::ExcludeSuffixTooShort {
                        actual: suffix.len(),
                        min: self.config.min_exclude_suffix_len,
                    });
                }
                if suffix.len() > self.config.max_exclude_suffix_len {
                    return Err(MempoolFilterError::ExcludeSuffixTooLong {
                        actual: suffix.len(),
                        max: self.config.max_exclude_suffix_len,
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
        let snapshot = self.snapshot();

        let mut excluded: HashSet<TxHash> = HashSet::new();
        for exclude in exclude_suffixes {
            if let Some(txid) = unique_suffix_match(&snapshot.txids_sorted, &exclude.suffix) {
                excluded.insert(txid);
            }
        }

        snapshot
            .entries_in_order
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

/// If exactly one txid in `txids_reversed_sorted` ends with `suffix`, return it;
/// otherwise (zero or multiple matches) return `None`.
///
/// `txids_reversed_sorted` must be sorted by *reversed* txid bytes (as the
/// snapshot's `txids_sorted` is). Matching on a suffix then becomes a prefix
/// match over the reversed bytes, resolved by binary range search in
/// `O(log n)` per suffix.
fn unique_suffix_match(txids_reversed_sorted: &[TxHash], suffix: &[u8]) -> Option<TxHash> {
    if suffix.is_empty() || suffix.len() > 32 {
        return None;
    }

    // Compare `reverse(txid)[..suffix.len()]` against `reverse(suffix)`.
    let cmp_rev_prefix = |txid: &TxHash| -> Ordering {
        txid.0
            .iter()
            .rev()
            .take(suffix.len())
            .cmp(suffix.iter().rev())
    };

    let start =
        txids_reversed_sorted.partition_point(|txid| cmp_rev_prefix(txid) == Ordering::Less);

    let first = txids_reversed_sorted.get(start)?;
    if cmp_rev_prefix(first) != Ordering::Equal {
        return None; // zero matches
    }
    match txids_reversed_sorted.get(start + 1) {
        Some(second) if cmp_rev_prefix(second) == Ordering::Equal => None, // ambiguous
        _ => Some(*first),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txid(bytes: [u8; 32]) -> TxHash {
        TxHash(bytes)
    }

    #[test]
    fn unique_suffix_match_semantics() {
        // Two txids share the trailing bytes `.. 0x22 0x22`.
        let mut a = [0u8; 32];
        a[30] = 0x22;
        a[31] = 0x22;
        let mut b = [0x99; 32];
        b[30] = 0x22;
        b[31] = 0x22;
        let mut ids = vec![txid([0x11; 32]), txid(a), txid(b), txid([0x33; 32])];
        // `unique_suffix_match` requires the txids sorted by reversed bytes (as
        // the live snapshot keeps them).
        ids.sort_unstable_by(|x, y| x.0.iter().rev().cmp(y.0.iter().rev()));

        // Unique suffix -> that txid (only [0x11; 32] ends with 0x11 0x11).
        assert_eq!(
            unique_suffix_match(&ids, &[0x11, 0x11]),
            Some(txid([0x11; 32]))
        );
        // Zero matches -> None.
        assert_eq!(unique_suffix_match(&ids, &[0xAB, 0xCD]), None);
        // Ambiguous suffix (two txids end 0x22 0x22) -> None.
        assert_eq!(unique_suffix_match(&ids, &[0x22, 0x22]), None);
        // Empty suffix -> None (never matches everything).
        assert_eq!(unique_suffix_match(&ids, &[]), None);
    }
}
