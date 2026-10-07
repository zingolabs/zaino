//! Typed reads over any `SequenceRead` view (a snapshot's layered view, the committed store alike)
//!
//! - one reader = one pinned state: a commit or reorg landing mid-stream cannot move it
//! - clone = the view's clone (pointer copies)

use bytes::Bytes;
use zaino_persistence::{SequenceRead, View};
use zaino_primitives::types::{BlockRef, Height, TreeSizes};
use zcash_protocol::consensus::NetworkType;

use crate::{position, project::record_sizes, BLOCKS};

/// Records one [`CompactBlockReader::range`] reads (one index walk + one readahead)
pub(crate) const WINDOW_RECORDS: u32 = 256;

#[derive(Clone)]
pub struct CompactBlockReader<V> {
    view: V,
    network: NetworkType,
}

impl<V: SequenceRead> CompactBlockReader<V> {
    pub fn new(view: V, network: NetworkType) -> Self {
        Self { view, network }
    }

    pub(crate) fn view(&self) -> &V {
        &self.view
    }

    pub(crate) fn network(&self) -> NetworkType {
        self.network
    }

    /// `None` = nothing held
    pub(crate) fn tip(&self) -> Option<BlockRef> {
        self.view.tip()
    }

    /// Framed, wire-ready, every pool
    pub fn block(&self, at: Height) -> Option<Bytes> {
        self.view.record(BLOCKS, position(at))
    }

    /// Records from `first` toward `last` (both inclusive, both held, either direction) in walk
    /// order, plus the last height reached
    ///
    /// - <= [`WINDOW_RECORDS`] read, cut to `budget` bytes: work per call bounded, not by the range
    /// - always >= 1 record (a caller looping on the reach makes progress)
    pub(crate) fn range(&self, first: Height, last: Height, budget: usize) -> (Vec<Bytes>, Height) {
        let descending = first > last;
        let far = match descending {
            false => first.checked_add(WINDOW_RECORDS - 1).map_or(last, |far| far.min(last)),
            true => first.saturating_sub(WINDOW_RECORDS - 1).max(last),
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
        let walked = u32::try_from(kept - 1).expect("kept <= WINDOW_RECORDS");
        let reached = match descending {
            false => first.checked_add(walked),
            true => first.checked_sub(walked),
        };
        (records, reached.expect("reach within first..=last"))
    }

    /// Cumulative tree sizes after the tip (its record's `chainMetadata`; nothing held = zero)
    pub(crate) fn tip_sizes(&self) -> TreeSizes {
        let Some(tip) = self.tip() else { return TreeSizes::ZERO };
        let record = self.block(tip.height);
        let record =
            record.unwrap_or_else(|| panic!("compact_block: no record at its tip {tip:?}"));
        record_sizes(&record).expect("every record carries its chainMetadata")
    }
}

impl<V: View> std::fmt::Debug for CompactBlockReader<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactBlockReader")
            .field("tip", &self.view.tip())
            .field("network", &self.network)
            .finish()
    }
}
