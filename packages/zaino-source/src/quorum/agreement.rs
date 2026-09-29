//! k-of-n agreement over the members' tip readings.

use zaino_primitives::types::{BlockHash, Height};

/// One member's tip reading for a round, `None` when its read failed.
pub(super) type Reading = Option<(BlockHash, Height)>;

/// The tip at least `quorum` members agree on, if any.
///
/// Candidates are the distinct readings with at least `quorum` sightings.
/// Among several — possible only when `quorum` is at most half the members —
/// the highest wins; at equal height the one matching `previous` (the last
/// agreed hash), so a stable split does not flap; failing that, the one first
/// reported by the lowest-indexed member, so the choice is deterministic.
pub(super) fn agree(
    readings: &[Reading],
    quorum: usize,
    previous: Option<BlockHash>,
) -> Option<(BlockHash, Height)> {
    let mut candidates: Vec<((BlockHash, Height), usize)> = Vec::new();
    for reading in readings.iter().flatten() {
        match candidates.iter_mut().find(|(tip, _)| tip == reading) {
            Some((_, count)) => *count += 1,
            None => candidates.push((*reading, 1)),
        }
    }
    candidates
        .into_iter()
        .filter(|(_, count)| *count >= quorum)
        .map(|(tip, _)| tip)
        .fold(None, |best, tip| match best {
            None => Some(tip),
            Some(held) if tip.1 > held.1 => Some(tip),
            Some(held) if tip.1 == held.1 && Some(tip.0) == previous => Some(tip),
            Some(held) => Some(held),
        })
}

/// How many members agree on the most-reported tip — the count a
/// below-quorum failure names.
pub(super) fn largest_agreement(readings: &[Reading]) -> usize {
    readings
        .iter()
        .flatten()
        .map(|tip| {
            readings
                .iter()
                .flatten()
                .filter(|other| *other == tip)
                .count()
        })
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tip(hash: u8, height: u32) -> (BlockHash, Height) {
        (
            BlockHash::from([hash; 32]),
            Height::try_from(height).expect("test height"),
        )
    }

    #[test]
    fn two_of_three_agree() {
        let readings = [Some(tip(1, 10)), Some(tip(1, 10)), Some(tip(2, 11))];
        assert_eq!(agree(&readings, 2, None), Some(tip(1, 10)));
        assert_eq!(largest_agreement(&readings), 2);
    }

    #[test]
    fn below_quorum_is_none() {
        let readings = [Some(tip(1, 10)), Some(tip(2, 10)), None];
        assert_eq!(agree(&readings, 2, None), None);
        assert_eq!(largest_agreement(&readings), 1);
        assert_eq!(largest_agreement(&[None, None]), 0);
    }

    #[test]
    fn among_agreeing_tips_the_highest_wins() {
        // k = 1 of 3: every reading qualifies; the highest is chosen.
        let readings = [Some(tip(1, 10)), Some(tip(2, 12)), Some(tip(3, 11))];
        assert_eq!(agree(&readings, 1, None), Some(tip(2, 12)));
    }

    #[test]
    fn an_equal_height_split_prefers_the_previous_hash_then_the_first_seen() {
        let readings = [Some(tip(1, 10)), Some(tip(2, 10))];
        assert_eq!(
            agree(&readings, 1, Some(BlockHash::from([2; 32]))),
            Some(tip(2, 10))
        );
        assert_eq!(agree(&readings, 1, None), Some(tip(1, 10)));
    }

    #[test]
    fn a_single_member_agrees_with_itself() {
        assert_eq!(agree(&[Some(tip(7, 3))], 1, None), Some(tip(7, 3)));
        assert_eq!(agree(&[None], 1, None), None);
    }
}
