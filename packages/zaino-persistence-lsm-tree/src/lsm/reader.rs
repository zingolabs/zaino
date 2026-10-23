//! Reading across one map's committed segments
//!
//! - [`Snapshot`] = one committed list, mapped; immutable (a commit builds the next one)
//! - merge landing mid-request changes nothing held (pre-merge segments hold the same rows)
//! - `deletes` map: a key's tombstone in any segment = absent, whatever the order
//!   (insert-once / remove-once: at most one value + one tombstone exist; `lsm-deletes.md`)
//! - [`Cursor`] = ordered walk of a key range, either direction, resumable, rows borrowed from
//!   the mapping (no per-row copy or reference count)

use std::{
    cmp::Ordering,
    ops::{Bound, Range},
    path::Path,
    sync::{Arc, Weak},
};

use bytes::Bytes;
use rayon::prelude::*;

use super::{
    file::{Prefetch, Probed, SegmentFile},
    layout::{Row, Shape},
    parse_file_name,
    spill::is_scratch,
    Breach, Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::{Pages, PAGE},
};

/// Batch size from which [`Snapshot::get_many`] fans out on rayon (measured, 9 segments, 2.3M
/// rows: 2-16 keys 5-10× slower on rayon (wake-up), crossover ≈ 64, 256 keys 3× faster)
const PARALLEL_FROM: usize = 64;

/// Map's committed segments, mapped for reads
pub(crate) struct Snapshot {
    shape: Shape,
    segments: Vec<Arc<Probed>>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("segments", &self.segments.len())
            .finish()
    }
}

/// A key's state across a snapshot's segments, values zero-copy from the mapping
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lookup {
    Absent,
    Value(Bytes),
    /// Its tombstone is still listed (a `deletes` map)
    Removed,
}

/// Where a lookup landed: the segment + slot holding the key's value
enum Hit<'a> {
    Absent,
    Value(&'a SegmentFile, usize),
    Removed,
}

/// `meta`'s segment mapped for reads, its filter read and checked up front
fn mapped(fs: &dyn Fs, dir: &Path, shape: Shape, meta: &SegmentMeta) -> Result<Arc<Probed>> {
    // probed = point lookups on hash-like keys, else range scans
    let access = if shape.probed {
        Access::Random
    } else {
        Access::Normal
    };
    Ok(Arc::new(
        SegmentFile::open(fs, dir, meta, shape, access)?.probed()?,
    ))
}

/// Ascending byte ranges widened to whole pages (readahead's unit), then merged where they
/// overlap or touch: one readahead request per run of contiguous pages; empty ranges dropped
fn coalesced(ranges: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let pages = ranges.filter(|range| !range.is_empty()).map(|range| {
        let start = (range.start / PAGE).saturating_mul(PAGE);
        let end = range
            .end
            .checked_next_multiple_of(PAGE)
            .unwrap_or(usize::MAX);
        start..end
    });
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in pages {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

impl Snapshot {
    /// `dir` holding exactly `listed`: every other segment file (and its checksums) and any
    /// writer's scratch removed, every listed one mapped (lengths, summary and filter checked; record pages on touch)
    pub(crate) fn open(
        fs: &dyn Fs,
        dir: &Path,
        shape: Shape,
        listed: &[SegmentMeta],
    ) -> Result<Self> {
        let mut removed = false;
        for name in fs.list(dir)? {
            // scratch never listed: whatever is left of one was cut short
            let unlisted = match parse_file_name(&name) {
                Some(id) => !listed.iter().any(|segment| segment.id == id),
                None => is_scratch(&name),
            };
            if unlisted {
                fs.remove(&dir.join(name))?;
                removed = true;
            }
        }
        if removed {
            fs.sync_dir(dir)?;
        }
        let segments = listed
            .iter()
            .map(|meta| mapped(fs, dir, shape, meta))
            .collect::<Result<_>>()?;
        Ok(Self { shape, segments })
    }

    /// `listed` mapped, sharing the segments this snapshot already maps
    pub(crate) fn next(&self, fs: &dyn Fs, dir: &Path, listed: &[SegmentMeta]) -> Result<Self> {
        let mut segments = Vec::with_capacity(listed.len());
        for meta in listed {
            let reused = self
                .segments
                .iter()
                .find(|file| file.meta == *meta)
                .map(Arc::clone);
            segments.push(match reused {
                Some(file) => file,
                None => mapped(fs, dir, self.shape, meta)?,
            });
        }
        Ok(Self {
            shape: self.shape,
            segments,
        })
    }

    /// Segments a scan between `lower` and `upper` reads: those whose filter may hold the prefix
    /// (of that segment's filter length) both bounds share (all, when unbounded or not sharing one)
    fn candidates(&self, lower: Bound<&[u8]>, upper: Bound<&[u8]>) -> Result<Vec<usize>> {
        fn prefix(bound: Bound<&[u8]>, filtered: usize) -> Option<&[u8]> {
            match bound {
                Bound::Included(key) | Bound::Excluded(key) => key.get(..filtered),
                Bound::Unbounded => None,
            }
        }
        let mut candidates = Vec::with_capacity(self.segments.len());
        for (at, file) in self.segments.iter().enumerate() {
            let filtered = file.filtered();
            let admitted = match (prefix(lower, filtered), prefix(upper, filtered)) {
                (Some(low), Some(high)) if low == high => file.may_contain(low)?,
                _ => true,
            };
            if admitted {
                candidates.push(at);
            }
        }
        Ok(candidates)
    }

    /// Value under `key`, borrowed from the mapping by `found` (each segment's filter first: a
    /// miss = a memory probe per segment)
    pub(crate) fn lookup_with<R>(
        &self,
        key: &[u8],
        found: impl FnOnce(Option<&[u8]>, bool) -> R,
    ) -> Result<R> {
        match self.find(key)? {
            Hit::Absent => Ok(found(None, false)),
            Hit::Removed => Ok(found(None, true)),
            Hit::Value(file, slot) => match file.content(slot)? {
                (_, Row::Value(value)) => Ok(found(Some(value), false)),
                (_, Row::Tombstone) => Ok(found(None, true)),
            },
        }
    }

    /// States of `keys`, in `keys`' order
    ///
    /// - probed in ascending key order: neighbouring keys share fence, filter and record pages
    /// - [`PARALLEL_FROM`] keys and up: prefetched ([`prefetch`](Self::prefetch)), then probed on
    ///   the rayon pool; below: this thread, no prefetch
    pub(crate) fn get_many(&self, keys: &[&[u8]]) -> Result<Vec<Lookup>> {
        let mut sorted: Vec<(&[u8], usize)> = keys
            .iter()
            .enumerate()
            .map(|(at, key)| (*key, at))
            .collect();
        sorted.sort_unstable();
        if sorted.len() >= PARALLEL_FROM {
            self.prefetch(&sorted)?;
        }
        let probe = |(key, at): &(&[u8], usize)| -> Result<(usize, Lookup)> {
            let lookup = match self.find(key)? {
                Hit::Absent => Lookup::Absent,
                Hit::Removed => Lookup::Removed,
                Hit::Value(file, slot) => Lookup::Value(file.entry(slot)?.1),
            };
            Ok((*at, lookup))
        };
        let found: Vec<(usize, Lookup)> = match sorted.len() < PARALLEL_FROM {
            true => sorted.iter().map(probe).collect::<Result<_>>()?,
            false => sorted.par_iter().map(probe).collect::<Result<_>>()?,
        };

        let mut lookups = vec![Lookup::Absent; keys.len()];
        for (at, lookup) in found {
            if let Some(slot) = lookups.get_mut(at) {
                *slot = lookup;
            }
        }
        Ok(lookups)
    }

    /// Readahead for a batch of point lookups (`sorted` = ascending keys), in a seek's page order
    /// (advisory: never changes an answer)
    ///
    /// - cold seek = two serial faults per segment (fence group, then record block): device sees
    ///   one read per probing thread
    /// - `MADV_WILLNEED` queues without waiting: every candidate's fence group, then (those in
    ///   flight) every candidate's record block → device sees the whole batch at once
    /// - candidate = segment whose filter admits the key (≈ only the segment holding it)
    /// - sorted keys give ascending ranges per segment, merged where they share a page
    /// - planned + advised on the rayon pool (one thread = queue depth 1 when the records round
    ///   faults fences, or advice blocks on a congested device)
    fn prefetch(&self, sorted: &[(&[u8], usize)]) -> Result<()> {
        for step in [Prefetch::Fences, Prefetch::Records] {
            let plan = self.prefetch_plan(step, sorted)?;
            plan.into_par_iter().for_each(|(segment, range)| {
                if let Some(file) = self.segments.get(segment) {
                    file.will_need(range);
                }
            });
        }
        Ok(())
    }

    /// Round of [`prefetch`](Self::prefetch): byte ranges `step` reads for `sorted`, as
    /// `(segment's list index, range)`, candidates only, coalesced per segment
    ///
    /// - [`Prefetch::Records`] round reads fences: plan it after the fences round's advice
    pub(super) fn prefetch_plan(
        &self,
        step: Prefetch,
        sorted: &[(&[u8], usize)],
    ) -> Result<Vec<(usize, Range<usize>)>> {
        let per_segment = self
            .segments
            .par_iter()
            .enumerate()
            .map(|(segment, file)| -> Result<Vec<(usize, Range<usize>)>> {
                let wanted = sorted
                    .par_iter()
                    .map(|(key, _)| -> Result<Option<Range<usize>>> {
                        let prefix = key.get(..file.filtered()).unwrap_or(key);
                        match file.may_contain(prefix)? {
                            true => Ok(Some(file.prefetch_range(step, key)?)),
                            false => Ok(None),
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                let ranges = coalesced(wanted.into_iter().flatten());
                Ok(ranges.into_iter().map(|range| (segment, range)).collect())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(per_segment.into_iter().flatten().collect())
    }

    /// Newest segment first: list ≈ data age (batches append, merge takes its oldest input's slot)
    /// + lookups skew recent; order never changes an answer (keys unique across segments)
    ///
    /// - `deletes` map: every admitting segment checked, a tombstone in any = removed
    /// - a key of the wrong width = `Internal` (the adapter checks widths first)
    fn find(&self, key: &[u8]) -> Result<Hit<'_>> {
        if key.len() != self.shape.key_len {
            return Err(SegmentError::Internal("a point lookup off the key width"));
        }
        let mut value = None;
        for file in self.segments.iter().rev() {
            if !file.may_contain(key.get(..file.filtered()).unwrap_or(key))? {
                continue;
            }
            let slot = file.seek(key)?;
            if slot >= file.records() || file.key(slot)? != key {
                continue;
            }
            if !self.shape.deletes {
                return Ok(Hit::Value(file, slot));
            }
            match file.content(slot)? {
                (_, Row::Tombstone) => return Ok(Hit::Removed),
                (_, Row::Value(_)) => value = Some((file, slot)),
            }
        }
        Ok(value.map_or(Hit::Absent, |(file, slot)| Hit::Value(file, slot)))
    }

    /// Every committed segment's file
    pub(crate) fn pages(&self) -> impl Iterator<Item = &Pages> {
        self.segments.iter().map(|file| file.pages())
    }

    /// Segment `id`'s mapping, not kept alive
    pub(super) fn file(&self, id: u32) -> Option<Weak<Probed>> {
        self.segments
            .iter()
            .find(|file| file.meta.id == id)
            .map(Arc::downgrade)
    }

    /// Committed segments in list order (the merge policy's input)
    pub(crate) fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.iter().map(|file| file.meta).collect()
    }
}

/// Ordered walk of one snapshot's live rows within bounds, forward or reverse
///
/// - k-way merge over each candidate segment's window of slots; `heads` hold each window's next
///   key (copied: one key per step, never a value)
/// - `deletes` map: a key's value + tombstone, or a lone tombstone = skipped; any other repeat
///   (and any repeat without deletes) = `Contract`
/// - owns its snapshot: positions stay valid across calls (no re-seek between chunks)
pub(crate) struct Cursor {
    snapshot: Arc<Snapshot>,
    reverse: bool,
    heads: Vec<Head>,
    current: Option<usize>,
}

/// One segment's remaining slots, in walk order, and the key of the next
struct Head {
    segment: usize,
    window: Range<usize>,
    key: Vec<u8>,
}

impl Head {
    /// Slot the walk visits next (window non-empty)
    fn slot(&self, reverse: bool) -> usize {
        match reverse {
            false => self.window.start,
            true => self.window.end.saturating_sub(1),
        }
    }
}

impl Cursor {
    /// Live rows of `snapshot` between `lower` and `upper`, ascending (or descending when
    /// `reverse`); an empty or inverted range walks nothing
    pub(crate) fn new(
        snapshot: &Arc<Snapshot>,
        lower: Bound<&[u8]>,
        upper: Bound<&[u8]>,
        reverse: bool,
    ) -> Result<Self> {
        let mut heads = Vec::new();
        for segment in snapshot.candidates(lower, upper)? {
            let Some(file) = snapshot.segments.get(segment) else {
                continue;
            };
            let low = match lower {
                Bound::Unbounded => 0,
                Bound::Included(key) => file.seek(key)?,
                Bound::Excluded(key) => past(file, key)?,
            };
            let high = match upper {
                Bound::Unbounded => file.records(),
                Bound::Excluded(key) => file.seek(key)?,
                Bound::Included(key) => past(file, key)?,
            };
            if low < high {
                let mut head = Head {
                    segment,
                    window: low..high,
                    key: Vec::with_capacity(snapshot.shape.key_len),
                };
                head.key.extend_from_slice(file.key(head.slot(reverse))?);
                heads.push(head);
            }
        }
        let mut cursor = Self {
            snapshot: Arc::clone(snapshot),
            reverse,
            heads,
            current: None,
        };
        cursor.resolve()?;
        Ok(cursor)
    }

    /// The next live row as `(key, value)`, borrowed from the mapping; `None` = walked out
    pub(crate) fn current(&self) -> Result<Option<(&[u8], &[u8])>> {
        let Some(head) = self.current.and_then(|at| self.heads.get(at)) else {
            return Ok(None);
        };
        let file = self.file(head.segment)?;
        match file.content(head.slot(self.reverse))? {
            (key, Row::Value(value)) => Ok(Some((key, value))),
            (_, Row::Tombstone) => Err(SegmentError::Internal("cursor resolved to a tombstone")),
        }
    }

    /// Past the current row to the next live one
    pub(crate) fn advance(&mut self) -> Result<()> {
        if let Some(at) = self.current.take() {
            self.step(&[at])?;
        }
        self.resolve()
    }

    fn file(&self, segment: usize) -> Result<&SegmentFile> {
        self.snapshot
            .segments
            .get(segment)
            .map(|file| -> &SegmentFile { file })
            .ok_or(SegmentError::Internal("cursor head past the snapshot"))
    }

    /// Heads at `at` moved one slot on; walked-out heads dropped
    fn step(&mut self, at: &[usize]) -> Result<()> {
        for &index in at {
            let Some(head) = self.heads.get_mut(index) else {
                continue;
            };
            match self.reverse {
                false => head.window.start = head.window.start.saturating_add(1),
                true => head.window.end = head.window.end.saturating_sub(1),
            }
            if !head.window.is_empty() {
                let (segment, slot) = (head.segment, head.slot(self.reverse));
                let key = self
                    .snapshot
                    .segments
                    .get(segment)
                    .ok_or(SegmentError::Internal("cursor head past the snapshot"))?
                    .key(slot)?;
                if let Some(head) = self.heads.get_mut(index) {
                    head.key.clear();
                    head.key.extend_from_slice(key);
                }
            }
        }
        self.heads.retain(|head| !head.window.is_empty());
        Ok(())
    }

    /// `current` = the head holding the next live row, skipping removed keys
    ///
    /// - holders of the next key tracked in two slots + a count (valid data: a value and at most
    ///   its tombstone); only a breach collects every holder, for its error
    fn resolve(&mut self) -> Result<()> {
        loop {
            let (mut first, mut second, mut held) = (None::<usize>, None::<usize>, 0usize);
            for (at, head) in self.heads.iter().enumerate() {
                let order = first
                    .and_then(|best| self.heads.get(best))
                    .map(|best| head.key.cmp(&best.key));
                match (order, self.reverse) {
                    (None, _) | (Some(Ordering::Less), false) | (Some(Ordering::Greater), true) => {
                        (first, second, held) = (Some(at), None, 1);
                    }
                    (Some(Ordering::Equal), _) => {
                        second = second.or(Some(at));
                        held = held.saturating_add(1);
                    }
                    _ => {}
                }
            }
            let Some(first) = first else {
                self.current = None;
                return Ok(());
            };
            let deletes = self.snapshot.shape.deletes;
            match (held, second) {
                (1, _) => match self.is_value(first)? {
                    true => {
                        self.current = Some(first);
                        return Ok(());
                    }
                    false if deletes => self.step(&[first])?,
                    false => return Err(self.breach(first, held)),
                },
                (2, Some(second)) if deletes => {
                    match (self.is_value(first)?, self.is_value(second)?) {
                        (true, false) | (false, true) => self.step(&[first, second])?,
                        _ => return Err(self.breach(first, held)),
                    }
                }
                _ => return Err(self.breach(first, held)),
            }
        }
    }

    /// Whether head `at`'s next row is a value (else a tombstone)
    fn is_value(&self, at: usize) -> Result<bool> {
        let head = self
            .heads
            .get(at)
            .ok_or(SegmentError::Internal("cursor holder past its heads"))?;
        let (_, row) = self.file(head.segment)?.content(head.slot(self.reverse))?;
        Ok(matches!(row, Row::Value(_)))
    }

    /// `Contract`: the key at head `at` held `held` times, naming every segment holding it
    fn breach(&self, at: usize, held: usize) -> SegmentError {
        let key = self.heads.get(at).map(|head| head.key.as_slice());
        let segments = self
            .heads
            .iter()
            .filter(|head| Some(head.key.as_slice()) == key)
            .filter_map(|head| self.snapshot.segments.get(head.segment))
            .map(|file| file.meta.id)
            .collect();
        SegmentError::Contract {
            segments,
            breach: Breach::Repeated { held },
        }
    }
}

/// First slot past every key `<= key`
fn past(file: &SegmentFile, key: &[u8]) -> Result<usize> {
    let slot = file.seek(key)?;
    match slot < file.records() && file.key(slot)? == key {
        true => Ok(slot.saturating_add(1)),
        false => Ok(slot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - pages 0, 1, 2 touched → one run; page 4 after untouched page 3 → its own run
    /// - empty ranges vanish
    #[test]
    fn coalesced_widens_to_pages_and_merges_contiguous_runs() {
        let ranges = [
            0..10,
            5..20,
            20..20,
            30..100,
            4096..4100,
            9000..9001,
            17000..17001,
        ];
        let merged = coalesced(ranges.into_iter());
        assert_eq!(merged, [0..3 * PAGE, 4 * PAGE..5 * PAGE]);
    }
}
