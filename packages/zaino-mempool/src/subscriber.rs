//! Cheap, cloneable read handle onto the mempool read model.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;
use zaino_common::status::{NamedAtomicStatus, StatusType};
use zebra_chain::transaction::Hash as TxHash;

use crate::config::MempoolConfig;
use crate::entry::MempoolEntry;
use crate::event::MempoolEvent;
use crate::snapshot::MempoolSnapshot;

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

/// A validated, canonical-endian txid prefix used to filter the mempool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxIdPrefix {
    canonical_prefix: Vec<u8>,
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

/// A cheap, cloneable read handle onto the mempool.
#[derive(Clone)]
pub struct MempoolSubscriber {
    current: Arc<ArcSwap<MempoolSnapshot>>,
    events: broadcast::Sender<Arc<MempoolEvent>>,
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
        events: broadcast::Sender<Arc<MempoolEvent>>,
        config: MempoolConfig,
        status: NamedAtomicStatus,
    ) -> Self {
        Self {
            current,
            events,
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

    /// The current snapshot's txids, sorted by canonical byte order.
    pub fn get_txids(&self) -> Arc<[TxHash]> {
        self.snapshot().txids_sorted.clone()
    }

    /// Subscribe to the bounded mempool event stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<MempoolEvent>> {
        self.events.subscribe()
    }

    /// Validate and canonicalize a client-supplied exclude list.
    ///
    /// Enforces the configured aggregate-count and per-suffix length bounds
    /// before any work touches the mempool, then reverses each client-endian
    /// suffix into canonical (internal) byte order for prefix lookup. Bounding
    /// here is the fix for the unbounded-exclusion-filter denial of service.
    pub fn validate_exclude_suffixes(
        &self,
        exclude_suffixes_client_endian: &[Vec<u8>],
    ) -> Result<Vec<TxIdPrefix>, MempoolFilterError> {
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
                let mut canonical_prefix = suffix.clone();
                canonical_prefix.reverse();
                Ok(TxIdPrefix { canonical_prefix })
            })
            .collect()
    }

    /// The current snapshot's entries (in deterministic order) with the uniquely
    /// matched excluded txids removed.
    ///
    /// A prefix that uniquely matches one txid excludes it; a prefix matching
    /// zero txids is ignored; a prefix matching multiple txids excludes none
    /// (the lightwallet shortened-txid contract). Matching uses a binary range
    /// lookup over the sorted txids, so cost is `O(excludes · log n)`.
    pub fn get_filtered_entries(&self, exclude_prefixes: &[TxIdPrefix]) -> Vec<Arc<MempoolEntry>> {
        let snapshot = self.snapshot();

        let mut excluded: HashSet<TxHash> = HashSet::new();
        for prefix in exclude_prefixes {
            if let Some(txid) =
                unique_prefix_match(&snapshot.txids_sorted, &prefix.canonical_prefix)
            {
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

/// If exactly one txid in `sorted_txids` starts with `prefix`, return it;
/// otherwise (zero or multiple matches) return `None`.
///
/// `sorted_txids` must be sorted ascending by canonical txid bytes.
fn unique_prefix_match(sorted_txids: &[TxHash], prefix: &[u8]) -> Option<TxHash> {
    if prefix.is_empty() || prefix.len() > 32 {
        return None;
    }

    // First index whose leading `prefix.len()` bytes are >= `prefix`.
    let start = sorted_txids.partition_point(|txid| txid.0[..prefix.len()] < *prefix);

    let first = sorted_txids.get(start)?;
    if &first.0[..prefix.len()] != prefix {
        return None; // zero matches
    }
    match sorted_txids.get(start + 1) {
        Some(second) if &second.0[..prefix.len()] == prefix => None, // ambiguous
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
    fn unique_prefix_match_semantics() {
        let mut ids = vec![
            txid([0x11; 32]),
            txid([0x22; 32]),
            txid([
                0x22, 0x22, 0x22, 0x99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0,
            ]),
            txid([0x33; 32]),
        ];
        ids.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        // Unique prefix -> that txid.
        assert_eq!(
            unique_prefix_match(&ids, &[0x11, 0x11]),
            Some(txid([0x11; 32]))
        );
        // Zero matches -> None.
        assert_eq!(unique_prefix_match(&ids, &[0xAB, 0xCD]), None);
        // Ambiguous prefix (two txids start 0x22 0x22) -> None.
        assert_eq!(unique_prefix_match(&ids, &[0x22, 0x22]), None);
        // Empty prefix -> None (never matches everything).
        assert_eq!(unique_prefix_match(&ids, &[]), None);
    }
}
