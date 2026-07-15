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
use crate::ports::NonFinalizedEpoch;
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
    /// lightwallet shortened-txid contract). Cost is `O(excludes · n)`, which is
    /// bounded because `validate_exclude_suffixes` caps the exclude count and the
    /// snapshot is cost-bounded — the linear scan is intentional and safe here.
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

    /// A live stream of serialized mempool transactions.
    ///
    /// Yields the current snapshot's transactions first, then each subsequently
    /// added transaction, until the set freezes, closes, the caller falls behind
    /// the bounded event buffer, or (when `expected_epoch` is given) the live
    /// epoch changes. Returns `None` immediately if `expected_epoch` does not
    /// match the current snapshot — the caller's chain tip is stale and should
    /// re-snapshot.
    pub fn stream_raw_transactions(
        &self,
        expected_epoch: Option<NonFinalizedEpoch>,
    ) -> Option<impl futures::Stream<Item = Vec<u8>>> {
        // Subscribe before snapshotting so no event between the snapshot load and
        // the subscribe is missed; events at or below `start_sequence` are then
        // discarded as already reflected in the initial snapshot.
        let mut receiver = self.subscribe_events();
        let snapshot = self.snapshot();

        if let Some(expected) = expected_epoch {
            if !snapshot.is_valid_for_snapshot(expected) {
                return None;
            }
        }

        let start_sequence = snapshot.event_sequence;
        let initial_entries = snapshot.entries_in_order.clone();

        let stream = async_stream::stream! {
            for entry in initial_entries.iter() {
                yield entry.serialized_bytes().to_vec();
            }

            loop {
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    // Fell behind the bounded buffer, or the service closed.
                    Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Err(broadcast::error::RecvError::Closed) => break,
                };

                match event.as_ref() {
                    MempoolEvent::Added {
                        sequence,
                        valid_for,
                        entry,
                    } => {
                        if *sequence <= start_sequence {
                            continue;
                        }
                        if let Some(expected) = expected_epoch {
                            if *valid_for != expected {
                                break;
                            }
                        }
                        yield entry.serialized_bytes().to_vec();
                    }
                    // Raw streams do not emit removals.
                    MempoolEvent::Removed { .. } => {}
                    // Freeze or shutdown ends the stream; the client re-opens.
                    MempoolEvent::Frozen { .. } | MempoolEvent::Closing { .. } => break,
                    // Snapshot-level metadata only.
                    MempoolEvent::Live { .. } => {}
                }
            }
        };

        Some(stream)
    }
}

/// If exactly one txid in `txids` ends with `suffix`, return it; otherwise (zero
/// or multiple matches) return `None`.
fn unique_suffix_match(txids: &[TxHash], suffix: &[u8]) -> Option<TxHash> {
    if suffix.is_empty() || suffix.len() > 32 {
        return None;
    }

    let mut matches = txids.iter().filter(|txid| txid.0.ends_with(suffix));
    let first = *matches.next()?;
    match matches.next() {
        Some(_) => None, // ambiguous: two or more matches
        None => Some(first),
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
        let ids = vec![txid([0x11; 32]), txid(a), txid(b), txid([0x33; 32])];

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
