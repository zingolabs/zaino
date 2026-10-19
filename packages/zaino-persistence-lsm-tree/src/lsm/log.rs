//! Segment set's write side: batches → segments on the commit path, merges on background threads
//!
//! - merge output rides the next batch's manifest (no commit of its own; LevelDB/RocksDB
//!   `VersionEdit` via `LogAndApply`)
//! - inputs unlinked only once that manifest is durable (open removes anything unlisted)

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::{
    emit,
    layout::Shape,
    meta::{merge_candidates, tier_of, tier_shape},
    reader::Snapshot,
    report::{self, Landed},
    slots::Slots,
    writer::{Merged, SegmentWriter},
    Result, SegmentError, SegmentMeta,
};
use crate::fs::Fs;

/// Idle windows a merging tier may queue before [`SegmentLog::batch`] waits on its merge
/// (RocksDB `level0_stop_writes_trigger`: bounded segment count = bounded read fan-out)
const STALL_WINDOWS: usize = 2;

/// Map's committed list + merges running under it + list staged for the next manifest
///
/// - one merge per tier at a time, <= `slots` working engine-wide (lowest tier first:
///   small merges never queue behind a large one)
/// - `batch` stages → owner's manifest commit → `committed` maps the new list, unlinks, launches
pub(crate) struct SegmentLog {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    name: String,
    shape: Shape,
    snapshot: Arc<Snapshot>,
    writer: SegmentWriter,
    fanout: usize,
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

/// Background merge; thread yields `(merged, wall time)`, `None` = cancelled
struct Merge {
    tier: u32,
    inputs: Vec<SegmentMeta>,
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<Result<Option<(Merged, Duration)>>>,
}

impl SegmentLog {
    /// Map `name`'s segments in `dir`, at the committed `listed`
    ///
    /// - `fanout` below 2 refused (a merge of one segment = no merge)
    /// - unlisted files removed ([`Snapshot::open`])
    /// - merges launched on every idle tier holding `fanout` segments (reopen cancelled the last
    ///   process's: else no stall bounds a tier until the next commit)
    pub(crate) fn open(
        fs: Arc<dyn Fs>,
        dir: &Path,
        name: &str,
        shape: Shape,
        listed: &[SegmentMeta],
        fanout: usize,
        slots: Arc<Slots>,
    ) -> Result<Self> {
        if fanout < 2 {
            return Err(SegmentError::Shape {
                name: name.to_owned(),
                reason: format!("merge fanout {fanout} < 2"),
            });
        }
        let snapshot = Arc::new(Snapshot::open(fs.as_ref(), dir, shape, listed)?);
        let mut next: u32 = 0;
        for segment in listed {
            let after = segment
                .id
                .checked_add(1)
                .ok_or(SegmentError::Overflow("segment id"))?;
            next = next.max(after);
        }
        let mut log = Self {
            writer: SegmentWriter::open(Arc::clone(&fs), dir, shape),
            fs,
            dir: dir.to_path_buf(),
            name: name.to_owned(),
            shape,
            snapshot,
            fanout,
            slots,
            next,
            segments: listed.to_vec(),
            staged: None,
            merges: Vec::new(),
            shape_tiers: 0,
        };
        emit::stall_at(&log.name, stall_at(fanout));
        log.launch_merges()?;
        Ok(log)
    }

    /// Committed segments, mapped
    pub(crate) fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    /// Committed list, ≈ data age (readers probe newest first; answers never depend on order)
    #[cfg(test)]
    pub(crate) fn segments(&self) -> &[SegmentMeta] {
        &self.segments
    }

    /// `rows` (`(key, value)`, `None` = tombstone) as one segment (sealed, durably linked) + every
    /// finished merge swapped in for its inputs = list for the owner's next manifest
    ///
    /// - merge error or panic returns here
    /// - waits only on a merging tier the staged list holds `STALL_WINDOWS` windows behind
    ///   (checked after landing + the new segment, repeated: a landed output joins the tier above)
    pub(crate) fn batch(&mut self, rows: Vec<(&[u8], Option<&[u8]>)>) -> Result<Vec<SegmentMeta>> {
        if self.staged.is_some() {
            return Err(SegmentError::Internal(
                "batch before the last one was committed",
            ));
        }
        let mut staged = Staged {
            segments: self.segments.clone(),
            retired: Vec::new(),
            landed: Vec::new(),
        };
        self.land(&mut staged, |merge, _| merge.thread.is_finished())?;

        let id = self.allocate()?;
        if let Some(segment) = self.writer.write(id, rows)? {
            self.writer.sync_dir()?;
            emit::batched(&self.name, &segment);
            staged.segments.push(segment);
        }

        let (fanout, waited) = (self.fanout, Instant::now());
        let mut stalled = Vec::new();
        loop {
            let tiers = self.land(&mut staged, |merge, segments| {
                behind(merge, segments, fanout)
            })?;
            if tiers.is_empty() {
                break;
            }
            stalled.extend(tiers);
        }
        if !stalled.is_empty() {
            emit::stalled(&self.name, waited.elapsed());
            report::stalled(&self.name, &stalled, waited.elapsed());
        }

        let segments = staged.segments.clone();
        self.staged = Some(staged);
        Ok(segments)
    }

    /// Owner's manifest with [`batch`](Self::batch)'s list durable → mapped, retired inputs
    /// unlinked, merges launched on every idle tier holding `fanout` segments
    pub(crate) fn committed(&mut self) -> Result<()> {
        let staged = self
            .staged
            .take()
            .ok_or(SegmentError::Internal("committed without a batch"))?;
        self.segments = staged.segments;
        self.snapshot = Arc::new(self.snapshot.next(
            self.fs.as_ref(),
            &self.dir,
            &self.segments,
        )?);
        if self.snapshot.segments() != self.segments {
            return Err(SegmentError::Internal(
                "readers' list differs from the committed one",
            ));
        }
        self.writer.remove(&staged.retired)?;
        for landed in &staged.landed {
            report::swapped(&self.name, landed);
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
        emit::shape(&self.name, &shape);
    }

    fn launch(&mut self, tier: u32, inputs: Vec<SegmentMeta>) -> Result<Merge> {
        let peers = inputs
            .iter()
            .all(|input| tier_of(input, self.fanout) == tier);
        if inputs.len() != self.fanout || !peers {
            return Err(SegmentError::Internal("merge inputs off their tier"));
        }
        let (id, writer) = (self.allocate()?, self.writer.clone());
        let cancel = Arc::new(AtomicBool::new(false));
        report::launched(&self.name, tier, &inputs);
        let thread = thread::Builder::new()
            .name(format!("merge {} t{tier}", self.name))
            .spawn({
                let (inputs, cancel, name) =
                    (inputs.clone(), Arc::clone(&cancel), self.name.clone());
                let slots = Arc::clone(&self.slots);
                let span = tracing::Span::current();
                move || {
                    let _owner = span.enter();
                    crate::fs::background_priority();
                    let Some(slot) = slots.acquire(tier, &cancel) else {
                        return Ok(None);
                    };
                    let started = Instant::now();
                    let Some(merged) = writer.merge(id, &inputs, &cancel, &slot)? else {
                        return Ok(None);
                    };
                    writer.sync_dir()?;
                    let took = started.elapsed();
                    emit::merged(&name, tier, &merged, took);
                    Ok(Some((merged, took)))
                }
            })?;
        Ok(Merge {
            tier,
            inputs,
            cancel,
            thread,
        })
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
            let joined = merge
                .thread
                .join()
                .map_err(|_| SegmentError::MergePanicked {
                    name: self.name.clone(),
                })?;
            let (merged, took) =
                joined?.ok_or(SegmentError::Internal("merge cancelled outside drop"))?;
            let inputs = merge
                .inputs
                .iter()
                .fold(0u64, |sum, input| sum.saturating_add(input.records));
            let written = merged.output.map_or(0, |output| output.records);
            if written.saturating_add(merged.cancelled.saturating_mul(2)) != inputs {
                return Err(SegmentError::Internal("merge rows off its inputs"));
            }
            tiers.push(merge.tier);
            staged.swap(merge.tier, merge.inputs, merged, took)?;
        }
        Ok(tiers)
    }

    fn allocate(&mut self) -> Result<u32> {
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or(SegmentError::Overflow("segment id"))?;
        Ok(id)
    }

    /// Merges launched and not yet landed (each may have an unlisted output on disk)
    #[cfg(test)]
    pub(crate) fn merging(&self) -> usize {
        self.merges.len()
    }

    /// Blocks until every running merge finishes (lands on the next `batch`)
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        while self.merges.iter().any(|merge| !merge.thread.is_finished()) {
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// `segments` lists [`stall_at`] of `merge`'s tier
fn behind(merge: &Merge, segments: &[SegmentMeta], fanout: usize) -> bool {
    let listed = segments
        .iter()
        .filter(|segment| tier_of(segment, fanout) == merge.tier)
        .count();
    listed >= stall_at(fanout)
}

/// Merging tier's segment count a commit waits at: `fanout` inputs + `STALL_WINDOWS` idle windows
fn stall_at(fanout: usize) -> usize {
    fanout.saturating_mul(1 + STALL_WINDOWS)
}

impl Staged {
    /// `merged`'s output listed where the oldest input was (none: nothing in their place), every
    /// input retired
    fn swap(
        &mut self,
        tier: u32,
        inputs: Vec<SegmentMeta>,
        merged: Merged,
        took: Duration,
    ) -> Result<()> {
        let at = inputs
            .first()
            .and_then(|oldest| self.segments.iter().position(|segment| segment == oldest))
            .ok_or(SegmentError::Internal(
                "merge inputs unlisted before their swap",
            ))?;
        let listed = self.segments.len();
        self.segments.retain(|segment| !inputs.contains(segment));
        if listed.saturating_sub(self.segments.len()) != inputs.len() {
            return Err(SegmentError::Internal("a merge input unlisted"));
        }
        if let Some(output) = merged.output {
            self.segments.insert(at.min(self.segments.len()), output);
        }
        self.landed.push(Landed { tier, merged, took });
        self.retired.extend(inputs);
        Ok(())
    }
}

/// Cancels + joins every merge (detached one could race the next open's id allocation)
///
/// - outcomes dropped: outputs unlisted
impl Drop for SegmentLog {
    fn drop(&mut self) {
        if !self.merges.is_empty() {
            report::cancelled(&self.name, self.merges.len());
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
            .field("name", &self.name)
            .field("segments", &self.segments.len())
            .field(
                "merging",
                &self
                    .merges
                    .iter()
                    .map(|merge| merge.tier)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}
