//! Reading across a set of segments
//!
//! - one [`Snapshot`] pin per request (one atomic load)
//! - a merge landing mid-request changes nothing pinned (pre-merge segments hold the same rows)

use std::{
    marker::PhantomData,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use rayon::prelude::*;

use super::{
    file::{Prefetch, SegmentFile},
    layout::Shape,
    parse_file_name,
    record::{Key, Record},
    spill::is_scratch,
    Result, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::PAGE,
};

/// Batch size from which [`Snapshot::get_many`] fans out on rayon (measured, 9 segments, 2.3M
/// rows: 2-16 keys 5-10× slower on rayon (wake-up), crossover ≈ 64, 256 keys 3× faster)
const PARALLEL_FROM: usize = 64;

/// One mapped segment, typed by its key
struct Mapped<K> {
    file: SegmentFile,
    _key: PhantomData<fn() -> K>,
}

impl<K: Key> Mapped<K> {
    /// `meta`'s segment mapped for reads, its filter read and checked up front
    fn open<R: Record<Key = K>>(
        fs: &Arc<dyn Fs>,
        dir: &Path,
        meta: &SegmentMeta,
    ) -> Result<Arc<Self>> {
        let file = SegmentFile::open(fs.as_ref(), dir, meta, Shape::of::<R>(), reads::<R>())?;
        file.warm_filter();
        Ok(Arc::new(Self { file, _key: PhantomData }))
    }

    /// The row keyed exactly `key` (filter first: a miss touches no record)
    fn get<R: Record<Key = K>>(&self, key: &[u8]) -> Option<R> {
        if !self.file.may_contain(&key[..self.file.filtered()]) {
            return None;
        }
        let slot = self.file.seek(key);
        (slot < self.file.records() && self.file.key(slot) == key)
            .then(|| decoded(&self.file, slot))
    }
}

/// How a reader's mapping is read: probed = point lookups on hash-like keys, else range scans
fn reads<R: Record>() -> Access {
    match Shape::of::<R>().probed {
        true => Access::Random,
        false => Access::Normal,
    }
}

/// Sealed records decode (a failure = a writer bug, never data)
fn decoded<R: Record>(file: &SegmentFile, slot: usize) -> R {
    R::decode(file.row(slot)).expect("a sealed segment's records decode")
}

/// Ascending byte ranges widened to whole pages (readahead's unit), then merged where they
/// overlap or touch: one readahead request per run of contiguous pages; empty ranges dropped
fn coalesced(ranges: impl Iterator<Item = Range<usize>>) -> Vec<Range<usize>> {
    let pages = ranges
        .filter(|range| !range.is_empty())
        .map(|range| range.start / PAGE * PAGE..range.end.next_multiple_of(PAGE));
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in pages {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// A consistent view of every committed segment
pub struct Snapshot<K> {
    segments: Vec<Arc<Mapped<K>>>,
}

impl<K: Key> Snapshot<K> {
    /// Every record keyed `start` inclusive to `end` exclusive, ascending, across all segments
    pub fn range<R: Record<Key = K>>(&self, start: &K, end: &K) -> Vec<R> {
        self.range_at_most(start, end, usize::MAX).expect("no range holds usize::MAX rows")
    }

    /// [`range`](Self::range), or `None` once more than `limit` rows match across all segments
    ///
    /// - stops scanning at row `limit + 1` (a serve-path budget: cost bounded by `limit`, not by
    ///   how many rows the range holds)
    /// - a range whose start and end share the set's `FILTER_PREFIX` visits only the segments
    ///   whose filter may hold that prefix
    pub fn range_at_most<R: Record<Key = K>>(
        &self,
        start: &K,
        end: &K,
        limit: usize,
    ) -> Option<Vec<R>> {
        let (start, end) = (start.encode(), end.encode());
        let filtered = Shape::of::<R>().filtered;
        let prefix =
            (filtered > 0 && start[..filtered] == end[..filtered]).then(|| &start[..filtered]);
        let mut found = Vec::new();

        for mapped in &self.segments {
            let file = &mapped.file;
            if prefix.is_some_and(|prefix| !file.may_contain(prefix)) {
                continue;
            }
            for slot in file.seek(&start)..file.records() {
                if file.key(slot) >= end.as_slice() {
                    break;
                }
                if found.len() == limit {
                    return None;
                }
                found.push(decoded::<R>(file, slot));
            }
        }

        found.sort_by_key(|record| record.key());
        for pair in found.windows(2) {
            assert!(pair[0].key() < pair[1].key(), "a key listed in two committed segments");
        }
        Some(found)
    }

    /// The row keyed `key` (committed segments hold a key at most once; the owner's invariant)
    ///
    /// - probed sets: each segment's filter first, so a miss is a memory probe per segment
    pub fn get<R: Record<Key = K>>(&self, key: &K) -> Option<R> {
        self.find(&key.encode())
    }

    /// Rows for `keys`, in `keys`' order (`None` = absent)
    ///
    /// - probed in ascending key order: neighbouring keys share fence, filter and record pages
    /// - [`PARALLEL_FROM`] keys and up: prefetched ([`prefetch`](Self::prefetch)), then contiguous
    ///   runs probed on the rayon pool; below: this thread, no prefetch
    pub fn get_many<R: Record<Key = K> + Send>(&self, keys: &[K]) -> Vec<Option<R>>
    where
        K: Sync,
    {
        let mut sorted: Vec<(Vec<u8>, usize)> =
            keys.iter().enumerate().map(|(at, key)| (key.encode(), at)).collect();
        sorted.sort_unstable();
        if sorted.len() >= PARALLEL_FROM {
            self.prefetch(&sorted);
        }
        let probe = |(key, at): &(Vec<u8>, usize)| (*at, self.find(key));
        let found: Vec<(usize, Option<R>)> = match sorted.len() < PARALLEL_FROM {
            true => sorted.iter().map(probe).collect(),
            false => sorted.par_iter().map(probe).collect(),
        };

        let mut rows: Vec<Option<R>> = std::iter::repeat_with(|| None).take(keys.len()).collect();
        for (at, row) in found {
            rows[at] = row;
        }
        rows
    }

    /// Readahead for a batch of point lookups (`sorted` = ascending encoded keys), in the order a
    /// seek reads its pages; advisory, so it never changes an answer
    ///
    /// A cold seek faults twice in a row in each segment it searches: its fence group, then its
    /// block of records. A faulting thread waits on each read, so across a batch the device only
    /// ever sees one read per probing thread. `MADV_WILLNEED` queues reads without waiting, so the
    /// device sees the whole batch at once: first every candidate's fence group, then (with those
    /// reads in flight) every candidate's block of records.
    ///
    /// - candidate = a segment whose filter admits the key, which for a filtered set is almost
    ///   only the segment holding it (an unfiltered set is skipped: every segment would be a
    ///   candidate, while a lookup stops at its first hit)
    /// - sorted keys give ascending ranges per segment, merged where they share a page
    fn prefetch(&self, sorted: &[(Vec<u8>, usize)]) {
        for step in [Prefetch::Fences, Prefetch::Records] {
            for (segment, range) in self.prefetch_plan(step, sorted) {
                self.segments[segment].file.will_need(range);
            }
        }
    }

    /// One round of [`prefetch`](Self::prefetch): the byte ranges `step` reads for `sorted`, as
    /// `(segment's list index, range)`, candidates only, coalesced per segment
    ///
    /// - a [`Prefetch::Records`] round reads fences: plan it after the fences round's advice
    pub(super) fn prefetch_plan(
        &self,
        step: Prefetch,
        sorted: &[(Vec<u8>, usize)],
    ) -> Vec<(usize, Range<usize>)> {
        let mut plan = Vec::new();
        for (segment, file) in self.segments.iter().map(|mapped| &mapped.file).enumerate() {
            if file.filtered() == 0 {
                continue;
            }
            let wanted = sorted
                .iter()
                .filter(|(key, _)| file.may_contain(&key[..file.filtered()]))
                .map(|(key, _)| file.prefetch_range(step, key));
            plan.extend(coalesced(wanted).into_iter().map(|range| (segment, range)));
        }
        plan
    }

    /// Newest segment first: list ≈ data age (batches append, a merge takes its oldest input's
    /// slot) and lookups skew recent; order never changes an answer (keys unique across segments)
    fn find<R: Record<Key = K>>(&self, key: &[u8]) -> Option<R> {
        self.segments.iter().rev().find_map(|mapped| mapped.get::<R>(key))
    }

    /// Committed segments in list order (the merge policy's input)
    pub fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.iter().map(|mapped| mapped.file.meta).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

/// Directory of committed segments, published for reading (clones share one snapshot; readers
/// and the publishing writer never block each other)
pub struct SegmentSet<K> {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    snapshot: Arc<ArcSwap<Snapshot<K>>>,
}

impl<K> std::fmt::Debug for SegmentSet<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentSet")
            .field("dir", &self.dir)
            .field("segments", &self.snapshot.load().segments.len())
            .finish()
    }
}

impl<K> Clone for SegmentSet<K> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            dir: self.dir.clone(),
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

impl<K: Key + Send + Sync + 'static> SegmentSet<K> {
    /// Opens `dir` holding exactly `listed`: every other segment file (and its checksums) removed,
    /// every listed one mapped (lengths only; pages checked as reads touch them)
    pub fn open<R: Record<Key = K>>(
        fs: Arc<dyn Fs>,
        dir: &Path,
        listed: &[SegmentMeta],
    ) -> Result<Self> {
        let mut removed = false;
        for name in fs.list(dir)? {
            // a writer's scratch is never listed: whatever is left of one was cut short
            if is_scratch(&name) {
                fs.remove(&dir.join(name))?;
                removed = true;
                continue;
            }
            let Some(id) = parse_file_name(&name) else {
                continue;
            };
            if !listed.iter().any(|segment| segment.id == id) {
                fs.remove(&dir.join(name))?;
                removed = true;
            }
        }
        if removed {
            fs.sync_dir(dir)?;
        }

        let segments =
            listed.iter().map(|meta| Mapped::open::<R>(&fs, dir, meta)).collect::<Result<_>>()?;
        Ok(Self {
            fs,
            dir: dir.to_path_buf(),
            snapshot: Arc::new(ArcSwap::from_pointee(Snapshot { segments })),
        })
    }

    /// Publishes a newly committed list; segments already mapped are shared, not remapped
    pub fn publish<R: Record<Key = K>>(&self, listed: &[SegmentMeta]) -> Result<()> {
        let current = self.snapshot.load();
        let mut segments = Vec::with_capacity(listed.len());
        for meta in listed {
            let reused =
                current.segments.iter().find(|mapped| mapped.file.meta == *meta).map(Arc::clone);
            segments.push(match reused {
                Some(mapped) => mapped,
                None => Mapped::open::<R>(&self.fs, &self.dir, meta)?,
            });
        }
        self.snapshot.store(Arc::new(Snapshot { segments }));
        Ok(())
    }

    /// Current view (one atomic load: once per request)
    pub fn pin(&self) -> Arc<Snapshot<K>> {
        self.snapshot.load_full()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(super) fn fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pages 0, 1 and 2 touched → one run; page 4 after the untouched page 3 → a run of its own;
    /// empty ranges vanish
    #[test]
    fn coalesced_widens_to_pages_and_merges_contiguous_runs() {
        let ranges = [0..10, 5..20, 20..20, 30..100, 4096..4100, 9000..9001, 17000..17001];
        let merged = coalesced(ranges.into_iter());
        assert_eq!(merged, [0..3 * PAGE, 4 * PAGE..5 * PAGE]);
    }
}
