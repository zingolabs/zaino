//! Size-tiered merge policy over a segment list: tiers, merge picks, commit stall bound

use std::collections::BTreeMap;

use super::{Fanout, SegmentMeta};

/// Idle windows a merging tier may queue before [`SegmentSet::batch`] waits on its merge
/// (RocksDB `level0_stop_writes_trigger`: bounded segment count = bounded read fan-out)
///
/// [`SegmentSet::batch`]: super::SegmentSet::batch
const STALL_WINDOWS: usize = 2;

/// Size tier `⌊log_fanout(records)⌋`: `fanout` peers merge into the tier above
/// (write amplification `log_fanout(N)`)
pub(super) fn tier_of(segment: &SegmentMeta, fanout: Fanout) -> u32 {
    segment.records.ilog(fanout.get() as u64)
}

/// Next size-tiered merge: lowest tier outside `busy` listing `fanout` segments → its oldest
/// `fanout`
///
/// - peers need not be adjacent (keys unique across segments → order never changes an answer)
/// - lowest first: cheapest merge, most segments retired per byte
pub(super) fn merge_candidates(
    segments: &[SegmentMeta],
    fanout: Fanout,
    busy: &[u32],
) -> Option<(u32, Vec<SegmentMeta>)> {
    let mut peers = BTreeMap::<u32, Vec<SegmentMeta>>::new();
    for segment in segments {
        let tier = tier_of(segment, fanout);
        if !busy.contains(&tier) {
            peers.entry(tier).or_default().push(*segment);
        }
    }
    peers.into_iter().find_map(|(tier, mut segments)| {
        (segments.len() >= fanout.get()).then(|| {
            segments.truncate(fanout.get());
            (tier, segments)
        })
    })
}

/// Per tier `(segments listed, merge running)`, `0..` highest occupied tier or `floor`
pub(super) fn tier_shape(
    segments: &[SegmentMeta],
    merging: &[u32],
    fanout: Fanout,
    floor: usize,
) -> Vec<(usize, bool)> {
    let tiers: Vec<usize> =
        segments.iter().map(|segment| tier_of(segment, fanout) as usize).collect();
    let len = tiers
        .iter()
        .copied()
        .chain(merging.iter().map(|&tier| tier as usize))
        .map(|tier| tier + 1)
        .fold(floor, usize::max);
    let mut shape = vec![(0, false); len];
    for tier in tiers {
        shape[tier].0 += 1;
    }
    for &tier in merging {
        shape[tier as usize].1 = true;
    }
    shape
}

/// `segments` lists [`stall_at`] of merging `tier`
pub(super) fn behind(tier: u32, segments: &[SegmentMeta], fanout: Fanout) -> bool {
    let listed = segments.iter().filter(|segment| tier_of(segment, fanout) == tier).count();
    listed >= stall_at(fanout)
}

/// Merging tier's segment count a commit waits at: `fanout` inputs + `STALL_WINDOWS` idle windows
pub(crate) fn stall_at(fanout: Fanout) -> usize {
    (1 + STALL_WINDOWS) * fanout.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::Sealed;

    fn segment(id: u32, records: u64) -> SegmentMeta {
        SegmentMeta {
            id,
            records,
            tombstones: records / 4,
            sealed: Sealed { len: records * 3, tail: id * 7, sums: id * 11 },
        }
    }

    /// Lowest idle tier holding `fanout` segments merges its oldest `fanout`, adjacent or not
    #[test]
    fn merge_picks_the_oldest_peers_of_the_lowest_idle_full_tier() {
        let sizes = |sizes: &[u64]| -> Vec<SegmentMeta> {
            (0u32..).zip(sizes).map(|(id, &n)| segment(id, n)).collect()
        };

        // (records/segment, busy tiers) → (tier, ids merged)
        // fanout 4: tier 0 < 4 ≤ tier 1 < 16
        for (records, busy, expected) in [
            (vec![], vec![], None),
            (vec![1, 1, 1], vec![], None),
            (vec![1, 1, 1, 3], vec![], Some((0, vec![0, 1, 2, 3]))),
            (vec![1, 1, 1, 1, 1], vec![], Some((0, vec![0, 1, 2, 3]))),
            (vec![1, 100, 1, 9, 1, 1], vec![], Some((0, vec![0, 2, 4, 5]))),
            (vec![4, 5, 6, 7, 1, 1, 1, 1], vec![], Some((0, vec![4, 5, 6, 7]))),
            (vec![4, 5, 6, 7, 1, 1, 1, 1], vec![0], Some((1, vec![0, 1, 2, 3]))),
            (vec![4, 5, 6, 7, 1, 1, 1, 1], vec![0, 1], None),
            (vec![16, 1, 20, 1, 30, 40], vec![], Some((2, vec![0, 2, 4, 5]))),
        ] {
            let fanout = Fanout::new(4).expect(">= 2");
            let picked =
                merge_candidates(&sizes(&records), fanout, &busy).map(|(tier, segments)| {
                    (tier, segments.iter().map(|segment| segment.id).collect::<Vec<_>>())
                });
            assert_eq!(picked, expected, "{records:?} busy {busy:?}");
        }
    }

    /// - counts + merging flags per tier
    /// - length = highest occupied or merging tier, never below `floor` (emptied tier stays as 0)
    #[test]
    fn tier_shape_counts_every_tier_and_never_shrinks_below_the_floor() {
        let sizes = |sizes: &[u64]| -> Vec<SegmentMeta> {
            (0u32..).zip(sizes).map(|(id, &n)| segment(id, n)).collect()
        };

        // (records/segment, merging tiers, floor) → shape
        // fanout 4: tier 0 < 4 ≤ tier 1 < 16
        for (records, merging, floor, expected) in [
            (vec![], vec![], 0, vec![]),
            (vec![], vec![], 3, vec![(0, false); 3]),
            (vec![1, 2, 5], vec![], 0, vec![(2, false), (1, false)]),
            (vec![1, 2, 5], vec![0], 0, vec![(2, true), (1, false)]),
            (vec![1], vec![2], 0, vec![(1, false), (0, false), (0, true)]),
            (vec![20, 1], vec![], 4, vec![(1, false), (0, false), (1, false), (0, false)]),
        ] {
            let shape =
                tier_shape(&sizes(&records), &merging, Fanout::new(4).expect(">= 2"), floor);
            assert_eq!(shape, expected, "{records:?} merging {merging:?} floor {floor}");
        }
    }
}
