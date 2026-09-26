//! Which provider answers which heights.
//!
//! One mechanism for every arrangement the providers can be in. A point read is
//! a plan with one segment; a range read is the same plan with more.
//!
//! ```text
//! abut       store [0..1000]  head [1001..2001]   -> 2 segments
//! overlap    store [0..1000]  head  [999..2000]   -> 2 segments, store wins
//! hole       store [0..100]   head  [900..1900]   -> 3 segments, middle to the source
//! no store                    head  [900..1900]   -> below 900 to the source
//! ```
//!
//! Two rules, and both are stated once here rather than at each read.
//!
//! **The store starts at genesis.** Its coverage is the prefix
//! `[genesis, watermark]`, so one height describes it — a contract, not a field.
//!
//! **Where both cover a height, the store wins.** One rule; the durable
//! provider is authoritative for settled history and is the only one carrying
//! absolute chainwork; and the overlap exists as a margin preventing holes, so
//! reading from it would make that margin load-bearing twice over.

use zaino_primitives::types::{BlockRef, Height};

/// Who answers a run of heights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    /// The finalised store.
    Store,
    /// The chain head's recent window.
    Head,
    /// The validator.
    Source,
}

/// A contiguous run of heights answered by one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) provider: Provider,
    pub(crate) start: Height,
    pub(crate) end: Height,
}

/// What the providers cover, captured once when a snapshot is taken.
///
/// Precomputed rather than derived per call: reading the chain head's floor
/// means walking its block iterator, which is boxed, so deriving it on every
/// provider decision would allocate once per read per client.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Coverage {
    /// The top of the store's prefix, or `None` when it is disabled or empty.
    pub(crate) store_top: Option<Height>,
    /// The recent window's retained range.
    pub(crate) head: Option<(Height, Height)>,
    /// The block the chain head's work is counted from, exclusive: the block
    /// the window was anchored on, whose own work is zero.
    ///
    /// Carried here so the rebase to absolute chainwork is pinned with
    /// everything else: a re-anchor changes it, and a snapshot must answer for
    /// the window it was taken over.
    pub(crate) work_anchor: BlockRef,
    /// Whether the validator may fill what neither covers.
    pub(crate) source_fills: bool,
}

impl Coverage {
    /// The highest height any provider knows about.
    ///
    /// Bounds every request: asking for heights above the chain's own tip finds
    /// nothing, and asking the *validator* for them turns "not yet mined" into
    /// a round trip each.
    pub(crate) fn chain_tip(&self) -> Option<Height> {
        match (self.store_top, self.head.map(|(_, top)| top)) {
            (Some(store), Some(head)) => Some(store.max(head)),
            (Some(store), None) => Some(store),
            (None, head) => head,
        }
    }

    /// The lowest height neither local provider covers, if they leave a hole.
    pub(crate) fn gap_from(&self) -> Option<Height> {
        let (floor, _) = self.head?;
        let next = match self.store_top {
            Some(top) => top.checked_add(1)?,
            None => Height::GENESIS,
        };
        (next < floor).then_some(next)
    }

    /// Whether the local providers cover the chain between them.
    ///
    /// What the reads needing Zaino's own indexes check: the validator runs no
    /// spend index, so an answer spanning a hole would be silently incomplete.
    pub(crate) fn contiguous(&self) -> bool {
        self.gap_from().is_none()
    }

    fn store_covers(&self, height: Height) -> bool {
        self.store_top.is_some_and(|top| height <= top)
    }

    fn head_covers(&self, height: Height) -> bool {
        self.head
            .is_some_and(|(floor, top)| height >= floor && height <= top)
    }

    /// Who answers `height`.
    ///
    /// `Ok(None)` is a height above the chain tip — not yet mined, a miss
    /// rather than a failure. `Err` is a hole nothing can fill, which happens
    /// only with the validator switched off.
    pub(crate) fn provider_at(&self, height: Height) -> Result<Option<Provider>, ()> {
        let Some(tip) = self.chain_tip() else {
            return Ok(None);
        };
        if height > tip {
            return Ok(None);
        }
        if self.store_covers(height) {
            return Ok(Some(Provider::Store));
        }
        if self.head_covers(height) {
            return Ok(Some(Provider::Head));
        }
        if self.source_fills {
            return Ok(Some(Provider::Source));
        }
        Err(())
    }

    /// `lo..=hi` split into runs, each with the provider that answers it.
    ///
    /// Merged where a provider continues, so a caller issues one request per
    /// provider rather than one per height. Truncated at the chain tip.
    pub(crate) fn segments(&self, lo: Height, hi: Height) -> Result<Vec<Segment>, ()> {
        let Some(tip) = self.chain_tip() else {
            return Ok(Vec::new());
        };
        let hi = hi.min(tip);
        if lo > hi {
            return Ok(Vec::new());
        }

        let mut runs: Vec<Segment> = Vec::new();
        let mut cursor = lo;
        while cursor <= hi {
            let Some(provider) = self.provider_at(cursor)? else {
                break;
            };
            let end = match provider {
                Provider::Store => self.store_top.unwrap_or(cursor).min(hi),
                Provider::Head => self.head.map_or(cursor, |(_, top)| top).min(hi),
                // A hole runs until the window begins; only the head can start
                // above the cursor, because the store's coverage is a prefix.
                Provider::Source => match self.head {
                    Some((floor, _)) if floor > cursor => {
                        floor.checked_sub(1).unwrap_or(cursor).min(hi)
                    }
                    _ => hi,
                },
            };

            match runs.last_mut() {
                Some(last) if last.provider == provider => last.end = end,
                _ => runs.push(Segment {
                    provider,
                    start: cursor,
                    end,
                }),
            }

            let Some(next) = end.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("test height is within the protocol limit")
    }

    /// Coverage with the validator filling whatever the two tiers leave.
    ///
    /// `work_anchor` is the head's floor throughout: it says where chainwork is
    /// measured from, which no routing decision consults.
    fn covering(store_top: u32, head_floor: u32, head_tip: u32) -> Coverage {
        Coverage {
            store_top: Some(height(store_top)),
            head: Some((height(head_floor), height(head_tip))),
            work_anchor: BlockRef {
                hash: zaino_primitives::types::BlockHash::from([0; 32]),
                height: height(head_floor),
            },
            source_fills: true,
        }
    }

    fn abutting() -> Coverage {
        covering(1000, 1001, 2001)
    }

    fn overlapping() -> Coverage {
        covering(1000, 999, 2000)
    }

    fn gapped() -> Coverage {
        covering(100, 900, 1900)
    }

    fn segment(provider: Provider, start: u32, end: u32) -> Segment {
        Segment {
            provider,
            start: height(start),
            end: height(end),
        }
    }

    /// A point read is a one-segment plan.
    #[test]
    fn a_point_read_is_one_segment() {
        assert_eq!(
            abutting()
                .segments(height(500), height(500))
                .expect("covered"),
            vec![segment(Provider::Store, 500, 500)]
        );
    }

    /// A range spanning the seam splits in two, store first.
    #[test]
    fn a_range_spanning_the_seam_splits_in_two() {
        assert_eq!(
            abutting()
                .segments(height(900), height(1100))
                .expect("covered"),
            vec![
                segment(Provider::Store, 900, 1000),
                segment(Provider::Head, 1001, 1100),
            ]
        );
    }

    /// Where both cover a height, the store answers.
    #[test]
    fn the_store_wins_the_overlap() {
        assert_eq!(
            overlapping()
                .segments(height(998), height(1002))
                .expect("covered"),
            vec![
                segment(Provider::Store, 998, 1000),
                segment(Provider::Head, 1001, 1002),
            ]
        );
    }

    /// A hole is filled by the validator, in the middle of the range.
    #[test]
    fn a_hole_goes_to_the_source() {
        assert_eq!(
            gapped()
                .segments(height(50), height(1000))
                .expect("covered"),
            vec![
                segment(Provider::Store, 50, 100),
                segment(Provider::Source, 101, 899),
                segment(Provider::Head, 900, 1000),
            ]
        );
    }

    /// With no store, the validator covers everything below the window.
    #[test]
    fn with_no_store_the_source_covers_below_the_window() {
        let coverage = Coverage {
            store_top: None,
            ..gapped()
        };
        assert_eq!(
            coverage.segments(height(0), height(1000)).expect("covered"),
            vec![
                segment(Provider::Source, 0, 899),
                segment(Provider::Head, 900, 1000),
            ]
        );
    }

    /// A hole nothing can fill is refused.
    #[test]
    fn an_unfillable_hole_is_refused() {
        let coverage = Coverage {
            source_fills: false,
            ..gapped()
        };
        assert!(coverage.segments(height(50), height(1000)).is_err());
        // Either side of it still plans.
        assert!(coverage.segments(height(50), height(100)).is_ok());
        assert!(coverage.segments(height(900), height(1000)).is_ok());
    }

    /// A request above the tip is clamped, not refused.
    #[test]
    fn a_request_above_the_tip_is_clamped() {
        assert_eq!(
            abutting()
                .segments(height(1900), height(5000))
                .expect("covered"),
            vec![segment(Provider::Head, 1900, 2001)]
        );
        assert!(abutting()
            .segments(height(3000), height(5000))
            .expect("no failure")
            .is_empty());
    }

    /// The hole is reported where there is one, and not where there is not.
    #[test]
    fn the_hole_is_reported() {
        assert_eq!(gapped().gap_from(), Some(height(101)));
        assert_eq!(abutting().gap_from(), None);
        assert_eq!(overlapping().gap_from(), None);
        assert!(abutting().contiguous());
        assert!(!gapped().contiguous());
    }

    /// Segments tile the request exactly: no hole, no overlap, no repeat.
    ///
    /// The structural invariant every streaming caller relies on when it
    /// concatenates — a hole would be silently dropped blocks, an overlap a
    /// duplicated one.
    #[test]
    fn segments_tile_the_request_exactly() {
        for coverage in [abutting(), overlapping(), gapped()] {
            let plan = coverage.segments(height(0), height(1500)).expect("covered");
            assert_eq!(plan.first().expect("non-empty").start, height(0));
            assert_eq!(plan.last().expect("non-empty").end, height(1500));
            for pair in plan.windows(2) {
                assert_eq!(u32::from(pair[0].end) + 1, u32::from(pair[1].start));
                assert_ne!(pair[0].provider, pair[1].provider);
            }
        }
    }
}
