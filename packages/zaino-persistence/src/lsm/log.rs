//! Segment set's write side: batches → segments on the commit path, merges on background threads
//!
//! - merge output rides the next batch's manifest (no commit of its own)
//! - same as LevelDB/RocksDB `VersionEdit` via `LogAndApply`
//! - inputs unlinked only once that manifest is durable (open removes anything unlisted)

use std::{
    fmt, panic,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::{
    dir::SegmentDir,
    emit::{self, Series},
    layout::Shape,
    meta::{merge_candidates, tier_of, tier_shape},
    reader::SegmentsView,
    slots::Slots,
    writer::{Merged, SegmentWriter},
    Fanout, Result, SegmentMeta,
};
use crate::{fs::Fs, manifest::IndexKind, port::MapTable};

/// Idle windows a merging tier may queue before [`SegmentLog::batch`] waits on its merge
/// (RocksDB `level0_stop_writes_trigger`: bounded segment count = bounded read fan-out)
const STALL_WINDOWS: usize = 2;

/// Map's durable list + merges running under it + list staged for the next manifest
///
/// - one merge per tier at a time, <= `slots` working engine-wide (lowest tier first:
///   small merges never queue behind a large one)
/// - `batch` stages → owner's manifest commit → `on_manifest_durable` maps the new list, unlinks,
///   launches
pub(crate) struct SegmentLog {
    series: Series,
    segments_view: Arc<SegmentsView>,
    writer: SegmentWriter,
    fanout: Fanout,
    slots: Arc<Slots>,
    next: u32,
    segments: Vec<SegmentMeta>,
    staged: Option<Staged>,
    merges: Vec<Merge>,
    shape_tiers: usize,
}

/// List awaiting the owner's manifest + merge inputs it no longer lists + merges it lands
struct Staged {
    segments: Vec<SegmentMeta>,
    retired: Vec<SegmentMeta>,
    landed: Vec<Landed>,
}

/// Merge the log swaps in: `tier` → `merged`, `took` = its thread's wall time
#[derive(Debug, Clone, Copy)]
pub(super) struct Landed {
    pub(super) tier: u32,
    pub(super) merged: Merged,
    pub(super) took: Duration,
}

/// Background merge (thread yields `(merged, wall time)`, `None` = cancelled)
struct Merge {
    tier: u32,
    inputs: Vec<SegmentMeta>,
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<Result<Option<(Merged, Duration)>>>,
}

impl SegmentLog {
    /// `index`'s `table` segments in `dir`, at the durable `listed`
    ///
    /// - panics on a `table` the LSM cannot hold ([`Shape::of`])
    /// - unlisted files removed ([`SegmentDir::retain`])
    /// - merges launched on every idle tier holding `fanout` segments (reopen cancelled the last
    ///   process's: else no stall bounds a tier until the next commit)
    pub(crate) fn open(
        fs: Arc<dyn Fs>,
        dir: &Path,
        index: IndexKind,
        table: &MapTable,
        listed: &[SegmentMeta],
        fanout: Fanout,
        slots: Arc<Slots>,
    ) -> Result<Self> {
        let shape = Shape::of(table);
        let dir = SegmentDir::new(fs, dir);
        dir.retain(listed)?;
        let segments_view = Arc::new(SegmentsView::open(&dir, shape, listed)?);
        let next = listed
            .iter()
            .map(|segment| segment.id.checked_add(1).expect("segment ids below u32::MAX"))
            .max()
            .unwrap_or(0);
        let mut log = Self {
            writer: SegmentWriter::new(dir, shape),
            series: Series { index: index.name(), table: table.name },
            segments_view,
            fanout,
            slots,
            next,
            segments: listed.to_vec(),
            staged: None,
            merges: Vec::new(),
            shape_tiers: 0,
        };
        emit::opened(log.series, stall_at(fanout));
        log.launch_merges()?;
        Ok(log)
    }

    /// Durable segments, mapped
    pub(crate) fn segments_view(&self) -> &Arc<SegmentsView> {
        &self.segments_view
    }

    /// `rows` (`(key, value)`, `None` = tombstone) as one segment (sealed, durably linked) + every
    /// finished merge swapped in for its inputs = list for the owner's next manifest
    ///
    /// - merge panic resumes here, merge error returns here
    /// - waits only on a merging tier the staged list holds `STALL_WINDOWS` windows behind
    ///   (checked after landing + the new segment, repeated: a landed output joins the tier above)
    pub(crate) fn batch(&mut self, rows: Vec<(&[u8], Option<&[u8]>)>) -> Result<Vec<SegmentMeta>> {
        let name = self.series.table;
        assert!(self.staged.is_none(), "{name}: batch before the last one was committed");
        let shape = self.writer.shape();
        for (key, value) in &rows {
            assert_eq!(key.len(), shape.key_len, "LSM map {name}: key width");
            match value {
                Some(value) => assert_eq!(value.len(), shape.value_len, "LSM map {name}: value"),
                None => assert!(shape.deletes, "LSM map {name}: tombstone"),
            }
        }
        let mut staged =
            Staged { segments: self.segments.clone(), retired: Vec::new(), landed: Vec::new() };
        self.land(&mut staged, |merge, _| merge.thread.is_finished())?;

        let id = self.allocate();
        if let Some(segment) = self.writer.write(id, rows)? {
            self.writer.dir().sync()?;
            emit::batched(self.series, &segment);
            staged.segments.push(segment);
        }

        let (fanout, waited) = (self.fanout, Instant::now());
        let mut stalled = Vec::new();
        loop {
            let tiers =
                self.land(&mut staged, |merge, segments| behind(merge, segments, fanout))?;
            if tiers.is_empty() {
                break;
            }
            stalled.extend(tiers);
        }
        if !stalled.is_empty() {
            emit::stalled(self.series, &stalled, waited.elapsed());
        }

        let segments = staged.segments.clone();
        self.staged = Some(staged);
        Ok(segments)
    }

    /// Owner's manifest with [`batch`](Self::batch)'s list durable → mapped, retired inputs
    /// unlinked, merges launched on every idle tier holding `fanout` segments
    pub(crate) fn on_manifest_durable(&mut self) -> Result<()> {
        let staged = self.staged.take().expect("a commit follows batch");
        self.segments = staged.segments;
        self.segments_view = Arc::new(self.segments_view.next(self.writer.dir(), &self.segments)?);
        let name = self.series.table;
        assert_eq!(self.segments_view.segments(), self.segments, "{name}: readers see the list");
        self.writer.dir().remove(&staged.retired)?;
        for landed in &staged.landed {
            emit::swapped(self.series, landed);
        }
        self.launch_merges()
    }

    /// Merge on every idle tier listing `fanout` segments, then the tier shape published
    fn launch_merges(&mut self) -> Result<()> {
        loop {
            let busy: Vec<u32> = self.merges.iter().map(|merge| merge.tier).collect();
            let Some((tier, inputs)) = merge_candidates(&self.segments, self.fanout, &busy) else {
                self.publish_shape();
                return Ok(());
            };
            let merge = self.launch(tier, inputs)?;
            self.merges.push(merge);
        }
    }

    /// Every tier up to the highest ever published (emptied tier re-sent as zero)
    fn publish_shape(&mut self) {
        let merging: Vec<u32> = self.merges.iter().map(|merge| merge.tier).collect();
        let shape = tier_shape(&self.segments, &merging, self.fanout, self.shape_tiers);
        self.shape_tiers = shape.len();
        emit::shape(self.series, &shape);
    }

    fn launch(&mut self, tier: u32, inputs: Vec<SegmentMeta>) -> Result<Merge> {
        let (series, name) = (self.series, self.series.table);
        assert_eq!(inputs.len(), self.fanout.get(), "{name}: merge of {} segments", inputs.len());
        for input in &inputs {
            assert_eq!(tier_of(input, self.fanout), tier, "{name}: {input:?}");
        }
        let (id, writer) = (self.allocate(), self.writer.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        emit::launched(series, tier, &inputs);
        let thread = thread::Builder::new().name(format!("merge {name} t{tier}")).spawn({
            let (inputs, cancel) = (inputs.clone(), Arc::clone(&cancel));
            let slots = Arc::clone(&self.slots);
            let span = tracing::Span::current();
            move || {
                let _owner = span.enter();
                crate::thread::background_priority();
                let Some(slot) = slots.acquire(tier, &cancel) else {
                    return Ok(None);
                };
                let started = Instant::now();
                let Some(merged) = writer.merge(id, &inputs, &cancel, &slot)? else {
                    return Ok(None);
                };
                writer.dir().sync()?;
                let took = started.elapsed();
                emit::merged(series, tier, &merged, took);
                Ok(Some((merged, took)))
            }
        })?;
        Ok(Merge { tier, inputs, cancel, thread })
    }

    /// Joins every merge `lands` picks (given the staged list) and swaps it in → their tiers
    fn land(
        &mut self,
        staged: &mut Staged,
        lands: impl Fn(&Merge, &[SegmentMeta]) -> bool,
    ) -> Result<Vec<u32>> {
        let (landed, running): (Vec<Merge>, _) = std::mem::take(&mut self.merges)
            .into_iter()
            .partition(|merge| lands(merge, &staged.segments));
        self.merges = running;
        let mut tiers = Vec::with_capacity(landed.len());
        for merge in landed {
            let (merged, took) = merge
                .thread
                .join()
                .unwrap_or_else(|payload| panic::resume_unwind(payload))?
                .expect("merges cancel only on drop");
            tiers.push(merge.tier);
            staged.swap(merge.tier, merge.inputs, merged, took);
        }
        Ok(tiers)
    }

    fn allocate(&mut self) -> u32 {
        let id = self.next;
        self.next = id.checked_add(1).expect("segment ids below u32::MAX");
        id
    }
}

/// `segments` lists [`stall_at`] of `merge`'s tier
fn behind(merge: &Merge, segments: &[SegmentMeta], fanout: Fanout) -> bool {
    let listed = segments.iter().filter(|segment| tier_of(segment, fanout) == merge.tier).count();
    listed >= stall_at(fanout)
}

/// Merging tier's segment count a commit waits at: `fanout` inputs + `STALL_WINDOWS` idle windows
pub(crate) fn stall_at(fanout: Fanout) -> usize {
    (1 + STALL_WINDOWS) * fanout.get()
}

impl Staged {
    /// `merged`'s output listed where the oldest input was (none: nothing in their place), every
    /// input retired
    fn swap(&mut self, tier: u32, inputs: Vec<SegmentMeta>, merged: Merged, took: Duration) {
        let at = self
            .segments
            .iter()
            .position(|segment| *segment == inputs[0])
            .expect("merge inputs stay listed until swapped");
        let listed = self.segments.len();
        self.segments.retain(|segment| !inputs.contains(segment));
        assert_eq!(listed - self.segments.len(), inputs.len(), "every input listed");
        if let Some(output) = merged.output {
            self.segments.insert(at, output);
        }
        self.landed.push(Landed { tier, merged, took });
        self.retired.extend(inputs);
    }
}

/// Cancels + joins every merge (detached one could race the next open's id allocation)
///
/// - outcomes dropped: outputs unlisted, merge panic already reported by the panic hook
impl Drop for SegmentLog {
    fn drop(&mut self) {
        if !self.merges.is_empty() {
            emit::cancelled(self.series, self.merges.len());
        }
        for merge in &self.merges {
            merge.cancel.store(true, Ordering::Relaxed);
        }
        for merge in self.merges.drain(..) {
            let _ = merge.thread.join();
        }
    }
}

impl fmt::Debug for SegmentLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SegmentLog")
            .field("series", &self.series)
            .field("segments", &self.segments.len())
            .field("merging", &self.merges.iter().map(|merge| merge.tier).collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SegmentLog {
        /// Durable list, ≈ data age (readers probe newest first, answers never depend on order)
        pub(crate) fn segments(&self) -> &[SegmentMeta] {
            &self.segments
        }

        /// Merges launched and not yet landed (each may have an unlisted output on disk)
        pub(crate) fn merging(&self) -> usize {
            self.merges.len()
        }

        /// Blocks until every running merge finishes (lands on the next `batch`)
        pub(crate) fn settle(&self) {
            while self.merges.iter().any(|merge| !merge.thread.is_finished()) {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
