//! Committed segment as its manifest records it + merge policy over a segment list

use std::collections::BTreeMap;

use crate::{
    manifest::{BodyReader, ManifestError},
    pages::Sealed,
};

/// Committed segment: file id, record count, its file's seal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentMeta {
    pub(crate) id: u32,
    pub(crate) records: u64,
    pub(crate) sealed: Sealed,
}

impl SegmentMeta {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.records.to_le_bytes());
        self.sealed.encode(out);
    }

    fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        Ok(Self { id: body.u32()?, records: body.u64()?, sealed: Sealed::decode(body)? })
    }
}

/// `count u32 ‖ (id u32 ‖ records u64 ‖ sealed len u64 ‖ tail crc u32)*`, in list order
pub(crate) fn encode_list(segments: &[SegmentMeta], out: &mut Vec<u8>) {
    out.extend_from_slice(
        &u32::try_from(segments.len()).expect("segment count < 2^32").to_le_bytes(),
    );
    for segment in segments {
        segment.encode(out);
    }
}

/// Duplicate id or empty segment refused (neither ever committed)
pub(crate) fn decode_list(body: &mut BodyReader<'_>) -> Result<Vec<SegmentMeta>, ManifestError> {
    let count = body.u32()?;
    let mut segments = Vec::new();
    for _ in 0..count {
        let segment = SegmentMeta::decode(body)?;
        if segment.records == 0 {
            return Err(ManifestError::Body("empty segment listed"));
        }
        if segments.iter().any(|seen: &SegmentMeta| seen.id == segment.id) {
            return Err(ManifestError::Body("segment listed twice"));
        }
        segments.push(segment);
    }
    Ok(segments)
}

/// Size tier `⌊log_fanout(records)⌋`: `fanout` peers merge into the tier above
/// (write amplification `log_fanout(N)`)
pub(crate) fn tier_of(segment: &SegmentMeta, fanout: usize) -> u32 {
    assert!(fanout >= 2, "merge fanout {fanout} < 2");
    segment.records.ilog(u64::try_from(fanout).expect("fanout fits u64"))
}

/// Next size-tiered merge: lowest tier outside `busy` listing `fanout` segments → its oldest
/// `fanout`
///
/// - peers need not be adjacent (keys unique across segments → order never changes an answer)
/// - lowest first: cheapest merge, most segments retired per byte
pub(crate) fn merge_candidates(
    segments: &[SegmentMeta],
    fanout: usize,
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
        (segments.len() >= fanout).then(|| {
            segments.truncate(fanout);
            (tier, segments)
        })
    })
}

/// Per tier `(segments listed, merge running)`, `0..` highest occupied tier or `floor`
pub(crate) fn tier_shape(
    segments: &[SegmentMeta],
    merging: &[u32],
    fanout: usize,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(id: u32, records: u64) -> SegmentMeta {
        SegmentMeta {
            id,
            records,
            sealed: Sealed { len: records * 3, tail: id * 7, sums: id * 11 },
        }
    }

    /// List codec round trip; duplicate id or empty segment in a manifest refused
    #[test]
    fn segment_lists_round_trip_and_refuse_duplicates_and_empties() {
        let segments = vec![segment(4, 10), segment(9, 1), segment(2, 1 << 40)];
        let mut bytes = Vec::new();
        encode_list(&segments, &mut bytes);
        assert_eq!(bytes.len(), 4 + 3 * 28);
        let mut body = BodyReader::new(&bytes);
        assert_eq!(decode_list(&mut body).expect("decode"), segments);
        body.finish().expect("whole list consumed");

        for (bad, reason) in [
            (vec![segment(1, 5), segment(1, 6)], "segment listed twice"),
            (vec![segment(1, 0)], "empty segment listed"),
        ] {
            let mut bytes = Vec::new();
            encode_list(&bad, &mut bytes);
            let decoded = decode_list(&mut BodyReader::new(&bytes));
            assert!(matches!(decoded, Err(ManifestError::Body(r)) if r == reason), "{reason}");
        }
    }

    /// Lowest idle tier holding `fanout` segments merges its oldest `fanout`, adjacent or not
    #[test]
    fn merge_picks_the_oldest_peers_of_the_lowest_idle_full_tier() {
        let sizes = |sizes: &[u64]| -> Vec<SegmentMeta> {
            (0u32..).zip(sizes).map(|(id, &n)| segment(id, n)).collect()
        };

        // (records/segment, busy tiers) → (tier, ids merged); fanout 4: tier 0 < 4 ≤ tier 1 < 16
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
            let picked = merge_candidates(&sizes(&records), 4, &busy).map(|(tier, segments)| {
                (tier, segments.iter().map(|segment| segment.id).collect::<Vec<_>>())
            });
            assert_eq!(picked, expected, "{records:?} busy {busy:?}");
        }
    }

    /// Counts + merging flags per tier; length = highest occupied or merging tier, never below
    /// `floor` (emptied tier stays published as zero)
    #[test]
    fn tier_shape_counts_every_tier_and_never_shrinks_below_the_floor() {
        let sizes = |sizes: &[u64]| -> Vec<SegmentMeta> {
            (0u32..).zip(sizes).map(|(id, &n)| segment(id, n)).collect()
        };

        // (records/segment, merging tiers, floor) → shape; fanout 4: tier 0 < 4 ≤ tier 1 < 16
        for (records, merging, floor, expected) in [
            (vec![], vec![], 0, vec![]),
            (vec![], vec![], 3, vec![(0, false); 3]),
            (vec![1, 2, 5], vec![], 0, vec![(2, false), (1, false)]),
            (vec![1, 2, 5], vec![0], 0, vec![(2, true), (1, false)]),
            (vec![1], vec![2], 0, vec![(1, false), (0, false), (0, true)]),
            (vec![20, 1], vec![], 4, vec![(1, false), (0, false), (1, false), (0, false)]),
        ] {
            let shape = tier_shape(&sizes(&records), &merging, 4, floor);
            assert_eq!(shape, expected, "{records:?} merging {merging:?} floor {floor}");
        }
    }
}
