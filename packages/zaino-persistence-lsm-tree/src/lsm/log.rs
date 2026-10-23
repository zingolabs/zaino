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
        Arc, Weak,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::{
    emit::{self, Labels},
    file::Probed,
    file_name, file_prefix,
    layout::Shape,
    meta::{merge_candidates, tier_of, tier_of_rows, tier_shape},
    reader::Snapshot,
    report::{self, Landed},
    slots::Slots,
    spill::Scratch,
    writer::{Merged, SegmentWriter},
    LsmConfig, MergeFault, Result, SegmentError, SegmentMeta,
};
use crate::{fs::Fs, pages::sums_path};

/// Idle windows a merging tier may queue before [`SegmentLog::batch`] waits on its merge
/// (RocksDB `level0_stop_writes_trigger`: bounded segment count = bounded read fan-out)
const STALL_WINDOWS: usize = 2;

/// Map's committed list + merges running under it
///
/// - one merge per tier at a time, <= `slots` working engine-wide (lowest tier first:
///   small merges never queue behind a large one)
/// - `prepare` joins finished merges before a write touches anything (a failed one = the
///   write's `MergeFailed`) → `batch` stages everything else that can fail (segments written, the
///   next list mapped) → owner's manifest commit → `committed` swaps it in and can't fail
///   (removal failures warned and retried; a launch failure returned by the next write)
pub(crate) struct SegmentLog {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    name: String,
    snapshot: Arc<Snapshot>,
    writer: SegmentWriter,
    fanout: usize,
    max_retries: u32,
    slots: Arc<Slots>,
    next: u32,
    filtered: usize,
    segments: Vec<SegmentMeta>,
    leftover: Vec<Leftover>,
    removal_failure: Option<String>,
    labels: Labels,
    unlinked: metrics::Gauge,
    merges: Vec<Merge>,
    ready: Vec<Ready>,
    launch_failed: Option<SegmentError>,
    /// Transient merge failures in a row (reset by a merge that succeeds)
    failures: u32,
    max_merge_failures: u32,
    shape_tiers: usize,
}

/// List awaiting the owner's manifest, mapped, + merge inputs it no longer lists + merges it lands
/// + scratch files its writes could not remove
#[derive(Debug)]
pub(crate) struct Staged {
    segments: Vec<SegmentMeta>,
    snapshot: Arc<Snapshot>,
    retired: Vec<SegmentMeta>,
    landed: Vec<Landed>,
    scratch: Vec<Scratch>,
}

/// A merge joined before a write, its output sealed: swapped in by that write's batch
struct Ready {
    inputs: Vec<SegmentMeta>,
    landed: Landed,
}

impl Staged {
    /// The list for the owner's manifest
    pub(crate) fn segments(&self) -> &[SegmentMeta] {
        &self.segments
    }

    /// The list, mapped for readers
    pub(crate) fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }
}

/// A file no committed manifest lists, still on disk
/// - `mapped` = a retired segment's mapping, counted as unlinked once its file is removed
struct Leftover {
    path: PathBuf,
    bytes: u64,
    mapped: Option<Weak<Probed>>,
}

/// Background merge into segment `id`; thread yields `(merged, wall time)`, `None` = cancelled
struct Merge {
    id: u32,
    tier: u32,
    inputs: Vec<SegmentMeta>,
    cancel: Arc<AtomicBool>,
    thread: JoinHandle<Result<Option<(Merged, Duration)>>>,
}

impl SegmentLog {
    /// Map `name`'s segments in `dir`, at the committed `listed`, run by `lsm` (checked by the
    /// engine's open)
    ///
    /// - unlisted files removed ([`Snapshot::open`])
    /// - merges launched on every idle tier holding `fanout` segments (reopen cancelled the last
    ///   process's: else no stall bounds a tier until the next commit); none while a segment's
    ///   filter covers another scope than `shape`'s ([`rescope`](Self::rescope) first)
    pub(crate) fn open(
        fs: Arc<dyn Fs>,
        dir: &Path,
        labels: &Labels,
        shape: Shape,
        listed: &[SegmentMeta],
        lsm: &LsmConfig,
        slots: Arc<Slots>,
    ) -> Result<Self> {
        let name = labels.set();
        let fanout = lsm.fanout;
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
            snapshot,
            fanout,
            max_retries: lsm.max_retries,
            slots,
            next,
            filtered: shape.filtered,
            segments: listed.to_vec(),
            leftover: Vec::new(),
            removal_failure: None,
            unlinked: emit::unlinked(labels),
            labels: labels.clone(),
            merges: Vec::new(),
            ready: Vec::new(),
            launch_failed: None,
            failures: 0,
            max_merge_failures: lsm.max_merge_failures,
            shape_tiers: 0,
        };
        emit::stall_at(&log.labels, stall_at(fanout));
        if log.stale().is_empty() {
            log.launch_merges()?;
        }
        Ok(log)
    }

    /// Committed segments, mapped
    pub(crate) fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    /// Listed segments whose filter covers another prefix than the map's scope
    fn stale(&self) -> Vec<SegmentMeta> {
        self.segments
            .iter()
            .filter(|segment| usize::try_from(segment.filtered).ok() != Some(self.filtered))
            .copied()
            .collect()
    }

    /// Every segment written under another scope rewritten under the map's (its filter rebuilt;
    /// under the merge bandwidth), staged for the owner's manifest in the old one's place
    /// (`None` = every segment current); merges launch once it commits
    ///
    /// - a crash before that commit: the rewrites are unlisted (removed at open), the old
    ///   segments still listed, so the next open rescopes again
    pub(crate) fn rescope(&mut self) -> Result<Option<Staged>> {
        let stale = self.stale();
        if stale.is_empty() {
            return Ok(None);
        }
        report::rescoping(&self.name, &stale);
        let mut segments = self.segments.clone();
        let mut scratch = Vec::new();
        let cancel = AtomicBool::new(false);
        for old in &stale {
            let id = self.allocate()?;
            let slot = self
                .slots
                .acquire(tier_of(old, self.fanout), &cancel)
                .ok_or(SegmentError::Internal("a rescope refused its slot"))?;
            let merged = self
                .writer
                .merge(id, std::slice::from_ref(old), &cancel, &slot)?
                .ok_or(SegmentError::Internal("a rescope cancelled"))?;
            scratch.extend(merged.scratch);
            let rewritten = merged
                .output
                .filter(|output| output.records == old.records)
                .ok_or(SegmentError::Internal("a rescope off its segment's rows"))?;
            let listed = segments
                .iter_mut()
                .find(|segment| *segment == old)
                .ok_or(SegmentError::Internal("a rescoped segment unlisted"))?;
            *listed = rewritten;
        }
        self.writer.sync_dir()?;
        let snapshot = Arc::new(self.snapshot.next(self.fs.as_ref(), &self.dir, &segments)?);
        Ok(Some(Staged {
            segments,
            snapshot,
            retired: stale,
            landed: Vec::new(),
            scratch,
        }))
    }

    /// Before a write touches any file: every finished merge joined, and every merge the write
    /// would leave its tier `STALL_WINDOWS` windows behind (given `upcoming`, the record counts
    /// of the segments it adds; repeated: a landed output joins the tier above); successes kept
    /// for the write's [`batch`](Self::batch)
    ///
    /// - a merge that failed, or failed to start at the last write = `MergeFailed`: nothing
    ///   written, its partial files removed, its inputs still listed (merged again at a later
    ///   write); the most severe failure when several merges failed
    pub(crate) fn prepare(&mut self, upcoming: &[u64]) -> Result<()> {
        if let Some(error) = self.launch_failed.take() {
            return Err(self.merge_failed(error));
        }
        let mut failure = None;
        self.join(|merge, _| merge.thread.is_finished(), &mut failure);
        let (fanout, waited) = (self.fanout, Instant::now());
        let mut stalled = Vec::new();
        loop {
            let tiers = self.projected_tiers(upcoming);
            let joined = self.join(|merge, _| behind(merge.tier, &tiers, fanout), &mut failure);
            if joined.is_empty() {
                break;
            }
            stalled.extend(joined);
        }
        if !stalled.is_empty() {
            emit::stalled(&self.labels, waited.elapsed());
            report::stalled(&self.name, &stalled, waited.elapsed());
        }
        match failure {
            Some(error) => Err(self.merge_failed(error)),
            None => Ok(()),
        }
    }

    /// `error` as the write's `MergeFailed`: a transient failure past `max_merge_failures` in a
    /// row is fatal (retrying no longer helps; segments would pile up)
    fn merge_failed(&mut self, error: SegmentError) -> SegmentError {
        let mut fault = MergeFault::of(&error);
        if fault == MergeFault::Transient {
            self.failures = self.failures.saturating_add(1);
            if self.failures > self.max_merge_failures {
                fault = MergeFault::Fatal;
            }
        }
        SegmentError::MergeFailed {
            name: self.name.clone(),
            fault,
            source: Box::new(error),
        }
    }

    /// Tier of every segment the next manifest would list: the list with each ready merge swapped
    /// in, plus `upcoming`
    fn projected_tiers(&self, upcoming: &[u64]) -> Vec<u32> {
        let mut records: Vec<u64> = self
            .segments
            .iter()
            .filter(|segment| {
                !self
                    .ready
                    .iter()
                    .any(|ready| ready.inputs.contains(segment))
            })
            .map(|segment| segment.records)
            .collect();
        records.extend(
            self.ready
                .iter()
                .filter_map(|ready| ready.landed.merged.output)
                .map(|output| output.records),
        );
        records.extend(upcoming);
        records
            .into_iter()
            .map(|records| tier_of_rows(records, self.fanout))
            .collect()
    }

    /// `rows` (`(key, value)`, `None` = tombstone) as one segment, then `removed` (keys `rows`
    /// inserts) as tombstones in a newer one (sealed, durably linked) + every merge
    /// [`prepare`](Self::prepare) joined swapped in for its inputs = list for the owner's next
    /// manifest, mapped for readers
    ///
    /// - `removed`: a pair inserted + removed between two writes stays recorded until a merge
    ///   cancels it (a removal is forgotten only by a merge)
    pub(crate) fn batch(
        &mut self,
        rows: Vec<(&[u8], Option<&[u8]>)>,
        removed: Vec<&[u8]>,
    ) -> Result<Staged> {
        let mut staged = Staged {
            segments: self.segments.clone(),
            snapshot: Arc::clone(&self.snapshot),
            retired: Vec::new(),
            landed: Vec::new(),
            scratch: Vec::new(),
        };
        for ready in std::mem::take(&mut self.ready) {
            staged.swap(ready.inputs, ready.landed)?;
        }

        let tombstones = removed.into_iter().map(|key| (key, None)).collect();
        let mut written = false;
        for rows in [rows, tombstones] {
            if rows.is_empty() {
                continue;
            }
            let id = self.allocate()?;
            if let Some(segment) = self.writer.write(id, rows)? {
                emit::batched(&self.labels, &segment.meta);
                staged.segments.push(segment.meta);
                staged.scratch.extend(segment.scratch);
                written = true;
            }
        }
        if written {
            self.writer.sync_dir()?;
        }

        staged.snapshot = Arc::new(self.snapshot.next(
            self.fs.as_ref(),
            &self.dir,
            &staged.segments,
        )?);
        if staged.snapshot.segments() != staged.segments {
            return Err(SegmentError::Internal(
                "readers' list differs from the staged one",
            ));
        }
        Ok(staged)
    }

    /// Owner's manifest with `staged`'s list durable → swapped in; retired inputs and scratch
    /// files its writes could not remove are removed, and merges launched on every idle tier
    /// holding `fanout` segments (never an error: the commit is durable; a removal failure kept
    /// as a leftover, a launch failure returned by the next write)
    pub(crate) fn committed(&mut self, mut staged: Staged) {
        let mut removed: Vec<Leftover> = staged
            .retired
            .iter()
            .flat_map(|segment| {
                let path = self.dir.join(file_name(segment.id));
                [
                    Leftover {
                        path: sums_path(&path),
                        bytes: segment.sealed.sums_len(),
                        mapped: None,
                    },
                    Leftover {
                        path,
                        bytes: segment.sealed.len,
                        mapped: self.snapshot.file(segment.id),
                    },
                ]
            })
            .collect();
        let merges = staged
            .landed
            .iter_mut()
            .flat_map(|landed| std::mem::take(&mut landed.merged.scratch));
        removed.extend(
            staged
                .scratch
                .drain(..)
                .chain(merges)
                .map(|scratch| Leftover {
                    path: scratch.path,
                    bytes: scratch.bytes,
                    mapped: None,
                }),
        );
        self.segments = staged.segments;
        self.snapshot = staged.snapshot;
        for landed in &staged.landed {
            report::swapped(&self.name, landed);
            emit::merged(&self.labels, landed.tier, &landed.merged, landed.took);
        }
        self.clean(removed);
        if let Err(error) = self.launch_merges() {
            emit::launch_failed(&self.labels);
            report::launch_failed(&self.name, &error);
            self.launch_failed = Some(error);
        }
    }

    /// Older leftovers removed, one attempt each (no wait: their cause is not transient); `new`
    /// ones retried in rounds, up to `max_retries` times, 10, 20, 40 ms … apart (one pause per
    /// round for all of them: a write waits at most the pauses' sum, whatever the file count);
    /// those still failing kept for the next write
    fn clean(&mut self, new: Vec<Leftover>) {
        let old = std::mem::take(&mut self.leftover);
        let mut kept = self.remove_once(old);
        let mut pending = self.remove_once(new);
        let mut pause = Duration::from_millis(10);
        for retried in 0..self.max_retries {
            if pending.is_empty() {
                break;
            }
            for (leftover, error) in &pending {
                report::removal_retry(&self.name, &leftover.path, retried, error);
            }
            thread::sleep(pause);
            pause = pause.saturating_mul(2);
            let failed = pending.into_iter().map(|(leftover, _)| leftover).collect();
            pending = self.remove_once(failed);
        }
        kept.extend(pending);
        self.removal_failure = kept.last().map(|(_, error)| error.to_string());
        self.leftover = kept.into_iter().map(|(leftover, _)| leftover).collect();
        emit::leftover(&self.labels, self.leftover().1);
    }

    /// One removal attempt each (already gone = done); those that failed, with why
    fn remove_once(&self, leftovers: Vec<Leftover>) -> Vec<(Leftover, std::io::Error)> {
        let mut failed = Vec::new();
        for leftover in leftovers {
            match self.fs.remove(&leftover.path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    failed.push((leftover, error));
                }
                _ => {
                    if let Some(file) = leftover.mapped.as_ref().and_then(Weak::upgrade) {
                        file.unlinked(&self.unlinked);
                    }
                }
            }
        }
        failed
    }

    /// Leftover files no manifest lists and removal keeps failing on: count, bytes, last error
    pub(crate) fn leftover(&self) -> (usize, u64, Option<&str>) {
        let bytes = self
            .leftover
            .iter()
            .fold(0u64, |sum, leftover| sum.saturating_add(leftover.bytes));
        (self.leftover.len(), bytes, self.removal_failure.as_deref())
    }

    /// A failed merge's partial files (its output segment, checksums and scratch) removed; any
    /// still failing kept as leftovers
    fn remove_partial(&mut self, id: u32) {
        let prefix = file_prefix(id);
        let names = match self.fs.list(&self.dir) {
            Ok(names) => names,
            Err(error) => {
                // unlisted, so removed at the next open in any case
                report::partial_unlisted(&self.name, id, &error);
                return;
            }
        };
        let partial = names
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .map(|name| {
                let path = self.dir.join(name);
                let bytes = self
                    .fs
                    .open_read_only(&path)
                    .ok()
                    .flatten()
                    .and_then(|file| file.len().ok())
                    .unwrap_or(0);
                Leftover {
                    path,
                    bytes,
                    mapped: None,
                }
            })
            .collect();
        self.clean(partial);
    }

    /// Merge on every idle tier listing `fanout` segments, then the tier shape published
    fn launch_merges(&mut self) -> Result<()> {
        loop {
            let busy: Vec<u32> = self.merges.iter().map(|merge| merge.tier).collect();
            let Some((tier, inputs)) = merge_candidates(&self.segments, self.fanout, &busy) else {
                self.publish_shape();
                return Ok(());
            };
            match self.launch(tier, inputs) {
                Ok(merge) => self.merges.push(merge),
                Err(error) => {
                    self.publish_shape();
                    return Err(error);
                }
            }
        }
    }

    /// Every tier up to the highest ever published (emptied tier re-sent as zero)
    fn publish_shape(&mut self) {
        let merging: Vec<u32> = self.merges.iter().map(|merge| merge.tier).collect();
        let shape = tier_shape(&self.segments, &merging, self.fanout, self.shape_tiers);
        self.shape_tiers = shape.len();
        emit::shape(&self.labels, &shape);
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
                let (inputs, cancel) = (inputs.clone(), Arc::clone(&cancel));
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
                    Ok(Some((merged, started.elapsed())))
                }
            })?;
        Ok(Merge {
            id,
            tier,
            inputs,
            cancel,
            thread,
        })
    }

    /// Joins every merge `picks` chooses (given the ready list) → their tiers; each success kept
    /// ready for the next batch, each failure's partial files removed and its error kept in
    /// `failure` (the first)
    fn join(
        &mut self,
        picks: impl Fn(&Merge, &[Ready]) -> bool,
        failure: &mut Option<SegmentError>,
    ) -> Vec<u32> {
        let (picked, running): (Vec<Merge>, _) = std::mem::take(&mut self.merges)
            .into_iter()
            .partition(|merge| picks(merge, &self.ready));
        self.merges = running;
        let mut tiers = Vec::with_capacity(picked.len());
        for merge in picked {
            tiers.push(merge.tier);
            let (id, name) = (merge.id, self.name.clone());
            let joined = merge
                .thread
                .join()
                .map_err(|_| SegmentError::MergePanicked { name })
                .and_then(|joined| joined)
                // its rows checked against its inputs' by the merge itself (`SelfCheck`)
                .and_then(|merged| {
                    merged.ok_or(SegmentError::Internal("merge cancelled outside drop"))
                });
            match joined {
                Ok((merged, took)) => {
                    self.failures = 0;
                    self.ready.push(Ready {
                        inputs: merge.inputs,
                        landed: Landed {
                            tier: merge.tier,
                            merged,
                            took,
                        },
                    });
                }
                Err(error) => {
                    self.remove_partial(id);
                    let worse = failure
                        .as_ref()
                        .is_none_or(|kept| severity(&error) > severity(kept));
                    if worse {
                        *failure = Some(error);
                    }
                }
            }
        }
        tiers
    }

    fn allocate(&mut self) -> Result<u32> {
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or(SegmentError::Overflow("segment id"))?;
        Ok(id)
    }
}

/// `tiers` (one per listed segment) hold [`stall_at`] of `tier`
/// How badly a failed merge ends: corrupt input > fatal > transient
fn severity(error: &SegmentError) -> u8 {
    match MergeFault::of(error) {
        MergeFault::Corrupt => 2,
        MergeFault::Fatal => 1,
        MergeFault::Transient => 0,
    }
}

fn behind(tier: u32, tiers: &[u32], fanout: usize) -> bool {
    tiers.iter().filter(|listed| **listed == tier).count() >= stall_at(fanout)
}

/// Merging tier's segment count a commit waits at: `fanout` inputs + `STALL_WINDOWS` idle windows
fn stall_at(fanout: usize) -> usize {
    fanout.saturating_mul(1 + STALL_WINDOWS)
}

impl Staged {
    /// `landed`'s output listed where the oldest input was (none: nothing in their place), every
    /// input retired
    fn swap(&mut self, inputs: Vec<SegmentMeta>, landed: Landed) -> Result<()> {
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
        if let Some(output) = landed.merged.output {
            self.segments.insert(at.min(self.segments.len()), output);
        }
        self.landed.push(landed);
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

#[cfg(test)]
mod tests;
