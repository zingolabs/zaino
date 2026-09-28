//! Correcting a watermark that claims more than the index holds.
//!
//! The watermark is a stamp beside the data: the highest height every index
//! in the set has committed. The data outranks it. A watermark *below* the
//! data is the ordinary crash-recovery state and the indexer re-covers the
//! difference. A watermark *above* the data is corruption of the stamp alone,
//! and it is not harmless: the composed view routes every height up to the
//! watermark to this store, so each height in the gap is answered "no such
//! block" while the chain head holds it, and the indexer resumes from a
//! height it never reached, leaving the gap unindexed for good.
//!
//! The repair is the only sane one: find the highest header the index
//! actually holds, check the run below it is unbroken, and re-stamp the
//! watermark at the top of the unbroken run. A hole below the top is the
//! other shape the same fault leaves — a range that failed to index while a
//! later one succeeded — and a watermark above a hole makes the indexer skip
//! it forever. Heights beyond the window are trusted: they were written by
//! the bulk catch-up, whose batches are contiguous by construction.
//!
//! It is checked on every boot, before anything reads the stamp, and
//! reported loudly when it fires.
//!
//! ```text
//! held(h)    ⟺  headers[h] present
//! top        =   max { h ≤ w : held(h) }
//! repaired   =   min { g < top : ¬held(g) ∧ top − g ≤ WINDOW } − 1    if such g
//!            |   top                                                    otherwise
//! ```

use zaino_indexes::index_set::Builds;
use zaino_indexes::indexes::headers::{self, HeadersIndex};
use zaino_persistence::{Backend, BackendWriter, CommitError, OpenError};
use zaino_persistence_codec::watermark;
use zaino_primitives::types::Height;

use crate::{read_index_value, StoreReader};

/// How far below a bad watermark the repair looks for a header before giving
/// up. A gap this size is not a stamp fault but an index that never held the
/// heights it claims, which a re-stamp would only paper over.
const MAX_SEARCH: u32 = 1 << 20;

/// How far below the top header the repair checks for holes. Wider than any
/// run a follow sync writes between two stamps, so a range that failed to
/// index behind a later one that succeeded is inside it.
const WINDOW: u32 = 1 << 14;

/// A watermark the headers index did not bear out, and where it now points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatermarkRepair {
    /// The height the stamp claimed.
    pub claimed: Height,
    /// The top of the unbroken run of headers, which the stamp now names.
    pub corrected: Height,
}

/// The watermark could not be checked against the index, or not corrected.
#[derive(Debug, thiserror::Error)]
pub enum WatermarkRepairError {
    /// The backend could not open a reader or a writer.
    #[error("opening the store")]
    Open(#[source] OpenError),

    /// The watermark itself could not be read.
    #[error("reading the watermark")]
    Read(#[source] zaino_persistence::ReadError),

    /// A header probe failed for a reason other than absence.
    #[error("probing the headers index: {0}")]
    Probe(String),

    /// The corrected stamp could not be written.
    #[error("re-stamping the watermark")]
    Commit(#[source] CommitError),

    /// No header exists at the watermark or within [`MAX_SEARCH`] heights
    /// below it: the index does not hold what the stamp claims, and by more
    /// than a stamp fault could explain.
    #[error(
        "watermark {claimed} is ahead of the headers index and no header was found within \
         {searched} heights below it"
    )]
    NoHeaderBelow {
        /// The height the stamp claimed.
        claimed: Height,
        /// How many heights below it were probed.
        searched: u32,
    },
}

impl<B, M> StoreReader<B, M>
where
    B: Backend + 'static,
    M: Builds<HeadersIndex>,
{
    /// Check the watermark against the headers index and re-stamp it at the
    /// top of the unbroken run of headers if it claims more.
    ///
    /// `Ok(None)` when the stamp is consistent with the data (or there is no
    /// stamp); `Ok(Some(repair))` when it was corrected. The write is a
    /// single committed operation, so a crash mid-repair leaves either stamp,
    /// never neither.
    pub fn repair_watermark(&self) -> Result<Option<WatermarkRepair>, WatermarkRepairError> {
        let reader = self.backend.reader().map_err(WatermarkRepairError::Open)?;
        let Some(claimed) = watermark::read(&reader).map_err(WatermarkRepairError::Read)? else {
            return Ok(None);
        };

        // The highest header at or below the claim.
        let mut top = claimed;
        let mut searched = 0u32;
        while !header_held::<B>(&reader, top)? {
            searched += 1;
            let below = top.checked_sub(1).filter(|_| searched <= MAX_SEARCH);
            let Some(below) = below else {
                return Err(WatermarkRepairError::NoHeaderBelow { claimed, searched });
            };
            top = below;
        }

        // The lowest hole within the window below it, if any: the run the
        // stamp may name ends just beneath it.
        let mut corrected = top;
        let mut probe = top;
        for _ in 0..WINDOW {
            let Some(below) = probe.checked_sub(1) else {
                break;
            };
            if !header_held::<B>(&reader, below)? {
                corrected = below
                    .checked_sub(1)
                    .ok_or(WatermarkRepairError::NoHeaderBelow { claimed, searched })?;
            }
            probe = below;
        }

        if corrected == claimed {
            return Ok(None);
        }

        let mut writer = self.backend.writer().map_err(WatermarkRepairError::Open)?;
        writer
            .commit(vec![watermark::stamp(corrected)])
            .map_err(WatermarkRepairError::Commit)?;
        Ok(Some(WatermarkRepair { claimed, corrected }))
    }
}

/// Whether the headers index holds a header at `height`. A stale index reads
/// as absent, so a stale store is not silently re-stamped as healthy.
fn header_held<B: Backend>(
    reader: &B::Reader,
    height: Height,
) -> Result<bool, WatermarkRepairError> {
    read_index_value::<HeadersIndex, B>(reader, headers::ID.into(), height)
        .map(|header| header.is_some())
        .map_err(|transient| WatermarkRepairError::Probe(transient.0))
}
