//! Client-supplied exclude-suffix filtering for tip-agnostic mempool reads.
//!
//! The exclude-list validation types and the unique-suffix matcher backing
//! [`MempoolSubscriber::get_filtered_entries`](super::MempoolSubscriber::get_filtered_entries).

use std::cmp::Ordering;

use zaino_primitives::types::TransactionId;

/// A validated client-supplied exclude suffix.
///
/// The lightwallet protocol's `exclude_txid_suffixes` are the trailing bytes of
/// the txid in internal (little-endian) byte order; a transaction is excluded
/// when its txid **ends with** these bytes (equivalently, its big-endian display
/// hex starts with the reversed bytes — the form lightwalletd matches). The
/// bytes are stored exactly as supplied and matched with [`slice::ends_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxIdExcludeSuffix {
    pub(super) suffix: Vec<u8>,
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

/// If exactly one txid in `txids_reversed_sorted` ends with `suffix`, return it;
/// otherwise (zero or multiple matches) return `None`.
///
/// `txids_reversed_sorted` must be sorted by *reversed* txid bytes (as the
/// snapshot's `txids_sorted` is). Matching on a suffix then becomes a prefix
/// match over the reversed bytes, resolved by binary range search in
/// `O(log n)` per suffix.
pub(super) fn unique_suffix_match(
    txids_reversed_sorted: &[TransactionId],
    suffix: &[u8],
) -> Option<TransactionId> {
    if suffix.is_empty() || suffix.len() > 32 {
        return None;
    }

    // Compare `reverse(txid)[..suffix.len()]` against `reverse(suffix)`.
    let cmp_rev_prefix = |txid: &TransactionId| -> Ordering {
        <[u8; 32]>::from(*txid)
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

    fn txid(bytes: [u8; 32]) -> TransactionId {
        TransactionId::from(bytes)
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
        ids.sort_unstable_by_key(|txid| zaino_mempool::reversed_txid_key(*txid));

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
