//! Reading across one map's committed segments
//!
//! - [`Snapshot`] = one committed list, mapped; immutable (a commit builds the next one)
//! - merge landing mid-request changes nothing held (pre-merge segments hold the same rows)

use std::{ops::Range, path::Path, sync::Arc};

use bytes::Bytes;
use rayon::prelude::*;

use super::{
    file::{Prefetch, SegmentFile},
    layout::Shape,
    parse_file_name,
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

/// Map's committed segments, mapped for reads
pub(crate) struct Snapshot {
    shape: Shape,
    segments: Vec<Arc<SegmentFile>>,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").field("segments", &self.segments.len()).finish()
    }
}

/// `meta`'s segment mapped for reads, its filter read and checked up front
fn mapped(fs: &dyn Fs, dir: &Path, shape: Shape, meta: &SegmentMeta) -> Result<Arc<SegmentFile>> {
    // probed = point lookups on hash-like keys, else range scans
    let access = if shape.probed { Access::Random } else { Access::Normal };
    let file = SegmentFile::open(fs, dir, meta, shape, access)?;
    file.warm_filter();
    Ok(Arc::new(file))
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

impl Snapshot {
    /// `dir` holding exactly `listed`: every other segment file (and its checksums) and any
    /// writer's scratch removed, every listed one mapped (lengths only; pages checked on touch)
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
        let segments =
            listed.iter().map(|meta| mapped(fs, dir, shape, meta)).collect::<Result<_>>()?;
        Ok(Self { shape, segments })
    }

    /// `listed` mapped, sharing the segments this snapshot already maps
    pub(crate) fn next(&self, fs: &dyn Fs, dir: &Path, listed: &[SegmentMeta]) -> Result<Self> {
        let mut segments = Vec::with_capacity(listed.len());
        for meta in listed {
            let reused = self.segments.iter().find(|file| file.meta == *meta).map(Arc::clone);
            segments.push(match reused {
                Some(file) => file,
                None => mapped(fs, dir, self.shape, meta)?,
            });
        }
        Ok(Self { shape: self.shape, segments })
    }

    /// `(key, value)` for `start <= key < end`, ascending across all segments; `None` once more
    /// than `limit` match
    ///
    /// - stops scanning at row `limit + 1` (cost bounded by `limit`, not by the range)
    /// - `start` and `end` sharing the filtered prefix → only segments whose filter may hold it
    pub(crate) fn range(
        &self,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<(Bytes, Bytes)>> {
        let filtered = self.shape.filtered;
        let prefix = (start.len() >= filtered && end.len() >= filtered)
            .then(|| &start[..filtered])
            .filter(|prefix| *prefix == &end[..filtered]);
        let mut found = Vec::new();

        for file in &self.segments {
            if prefix.is_some_and(|prefix| !file.may_contain(prefix)) {
                continue;
            }
            for slot in file.seek(start)..file.records() {
                if file.key(slot) >= end {
                    break;
                }
                if found.len() == limit {
                    return None;
                }
                found.push(file.entry(slot));
            }
        }

        found.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        for pair in found.windows(2) {
            assert!(pair[0].0 < pair[1].0, "a key listed in two committed segments");
        }
        Some(found)
    }

    /// Value under `key` (each segment's filter first: a miss = a memory probe per segment)
    pub(crate) fn get(&self, key: &[u8]) -> Option<Bytes> {
        self.find(key)
    }

    /// Values for `keys`, in `keys`' order (`None` = absent)
    ///
    /// - probed in ascending key order: neighbouring keys share fence, filter and record pages
    /// - [`PARALLEL_FROM`] keys and up: prefetched ([`prefetch`](Self::prefetch)), then probed on
    ///   the rayon pool; below: this thread, no prefetch
    pub(crate) fn get_many(&self, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        let mut sorted: Vec<(&[u8], usize)> =
            keys.iter().enumerate().map(|(at, key)| (*key, at)).collect();
        sorted.sort_unstable();
        if sorted.len() >= PARALLEL_FROM {
            self.prefetch(&sorted);
        }
        let probe = |(key, at): &(&[u8], usize)| (*at, self.find(key));
        let found: Vec<(usize, Option<Bytes>)> = match sorted.len() < PARALLEL_FROM {
            true => sorted.iter().map(probe).collect(),
            false => sorted.par_iter().map(probe).collect(),
        };

        let mut values = vec![None; keys.len()];
        for (at, value) in found {
            values[at] = value;
        }
        values
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
    fn prefetch(&self, sorted: &[(&[u8], usize)]) {
        for step in [Prefetch::Fences, Prefetch::Records] {
            for (segment, range) in self.prefetch_plan(step, sorted) {
                self.segments[segment].will_need(range);
            }
        }
    }

    /// Round of [`prefetch`](Self::prefetch): byte ranges `step` reads for `sorted`, as
    /// `(segment's list index, range)`, candidates only, coalesced per segment
    ///
    /// - [`Prefetch::Records`] round reads fences: plan it after the fences round's advice
    pub(super) fn prefetch_plan(
        &self,
        step: Prefetch,
        sorted: &[(&[u8], usize)],
    ) -> Vec<(usize, Range<usize>)> {
        let mut plan = Vec::new();
        for (segment, file) in self.segments.iter().enumerate() {
            let wanted = sorted
                .iter()
                .filter(|(key, _)| file.may_contain(&key[..file.filtered()]))
                .map(|(key, _)| file.prefetch_range(step, key));
            plan.extend(coalesced(wanted).into_iter().map(|range| (segment, range)));
        }
        plan
    }

    /// Newest segment first: list ≈ data age (batches append, merge takes its oldest input's slot)
    /// + lookups skew recent; order never changes an answer (keys unique across segments)
    fn find(&self, key: &[u8]) -> Option<Bytes> {
        assert_eq!(key.len(), self.shape.key_len, "a point lookup names a whole key");
        self.segments.iter().rev().find_map(|file| {
            if !file.may_contain(&key[..self.shape.filtered]) {
                return None;
            }
            let slot = file.seek(key);
            (slot < file.records() && file.key(slot) == key).then(|| file.entry(slot).1)
        })
    }

    /// Committed segments in list order (the merge policy's input)
    pub(crate) fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.iter().map(|file| file.meta).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - pages 0, 1, 2 touched → one run; page 4 after untouched page 3 → its own run
    /// - empty ranges vanish
    #[test]
    fn coalesced_widens_to_pages_and_merges_contiguous_runs() {
        let ranges = [0..10, 5..20, 20..20, 30..100, 4096..4100, 9000..9001, 17000..17001];
        let merged = coalesced(ranges.into_iter());
        assert_eq!(merged, [0..3 * PAGE, 4 * PAGE..5 * PAGE]);
    }
}
