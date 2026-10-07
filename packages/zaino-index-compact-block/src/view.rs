//! One pinned view of every tier a read resolves against
//!
//! - loaded **once** per request or stream (one atomic load, not one per refill)
//! - held/committed seam = a field of the view: "which tier owns `h`" has one answer for the
//!   request's life, a commit landing mid-stream cannot move it
//! - long-range safety from pinned bytes, not from durable records being append-only

use bytes::Bytes;
use zaino_persistence::{SequenceRead, TieredView, View};
use zaino_primitives::types::Height;

use crate::{position, BLOCKS, HASH};

/// Records one [`ReadView::span`] reads (one index walk + one readahead)
pub(crate) const SPAN_RECORDS: u32 = 256;

/// Held blocks + committed records, one publication (clone = pointer copies, republished on
/// every applied block)
#[derive(Clone)]
pub struct ReadView<V> {
    view: TieredView<V>,
}

impl<V: View> ReadView<V> {
    pub(crate) fn new(view: TieredView<V>) -> Self {
        Self { view }
    }

    /// Tip height + hash (`GetLatestBlock` reads no record)
    pub(crate) fn tip_id(&self) -> Option<(Height, [u8; HASH])> {
        self.view.tip().map(|tip| (tip.height, tip.hash.into()))
    }

    /// Last committed height, inclusive (the seam; `None` = nothing committed)
    pub(crate) fn finalized_tip(&self) -> Option<Height> {
        self.view.durable().tip().map(|tip| tip.height)
    }

    /// Last height any tier answers, inclusive (`None` = nothing held)
    pub(crate) fn tip(&self) -> Option<Height> {
        self.view.tip().map(|tip| tip.height)
    }
}

impl<V: SequenceRead> ReadView<V> {
    /// Framed, wire-ready, every pool
    pub(crate) fn block(&self, height: Height) -> Option<Bytes> {
        self.view.record(BLOCKS, position(height))
    }

    /// Held above the committed tip: RAM, no page touched (`None` = not held, maybe committed)
    pub(crate) fn resident_block(&self, height: Height) -> Option<Bytes> {
        (Some(height) > self.finalized_tip()).then(|| self.block(height)).flatten()
    }

    /// Records from `first` toward `last` (both inclusive, both held, either direction) in walk
    /// order, plus the last height reached
    ///
    /// - <= [`SPAN_RECORDS`] read, cut to `budget` bytes: work per call bounded, not by the range
    /// - always >= 1 record, so a caller looping on the reach makes progress
    /// - committed ones = zero-copy slices of the mapping
    pub(crate) fn span(&self, first: Height, last: Height, budget: usize) -> (Vec<Bytes>, Height) {
        let descending = first > last;
        let far = match descending {
            false => first.checked_add(SPAN_RECORDS - 1).map_or(last, |far| far.min(last)),
            true => first.saturating_sub(SPAN_RECORDS - 1).max(last),
        };
        let (low, high) = (first.min(far), first.max(far));
        let mut records = self.view.records(BLOCKS, position(low)..position(high) + 1);
        if descending {
            records.reverse();
        }

        let mut bytes = 0;
        let fit = records.iter().take_while(|record| {
            bytes += record.len();
            bytes <= budget
        });
        let kept = fit.count().max(1);
        records.truncate(kept);
        let walked = u32::try_from(kept - 1).expect("kept <= SPAN_RECORDS");
        let reached = match descending {
            false => first.checked_add(walked),
            true => first.checked_sub(walked),
        };
        (records, reached.expect("reach within first..=last"))
    }
}

impl<V: View> std::fmt::Debug for ReadView<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadView")
            .field("finalized_tip", &self.finalized_tip())
            .field("tip", &self.tip())
            .finish_non_exhaustive()
    }
}
