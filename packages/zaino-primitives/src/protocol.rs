//! Zcash protocol constants and rules (specification facts; restating them elsewhere = bug)

/// No valid reorg rewrites more than this many blocks
pub const MAX_BLOCK_REORG_HEIGHT: u32 = 1000;

/// Serialized block ceiling (protocol spec §7.1 `MAX_BLOCK_SIZE`; a transaction never exceeds it)
pub const MAX_BLOCK_BYTES: usize = 2_000_000;

/// Ancestors whose times a median time past reads (protocol spec `PoWMedianBlockSpan`)
pub const MEDIAN_TIME_SPAN: usize = 11;

/// Upper median of the first [`MEDIAN_TIME_SPAN`] of `newest_first` (`None` = no times)
pub fn median_time_past(newest_first: impl IntoIterator<Item = u32>) -> Option<u32> {
    let mut times: Vec<u32> = newest_first.into_iter().take(MEDIAN_TIME_SPAN).collect();
    times.sort_unstable();
    times.get(times.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unsorted input; only the newest 11 count; an even count takes the upper middle
    #[test]
    fn median_time_past_is_the_upper_median_of_the_newest_eleven() {
        let newest_first = [9, 1, 8, 2, 7, 3, 6, 4, 5, 10, 11, 1_000, 2_000];
        assert_eq!(median_time_past(newest_first), Some(6));
        assert_eq!(median_time_past([4, 1, 3, 2]), Some(3));
        assert_eq!(median_time_past([]), None);
    }
}
