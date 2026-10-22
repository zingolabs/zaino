//! Reading across one map's durable segments
//!
//! - [`SegmentsView`] = one durable list, mapped, immutable (a commit builds the next one)
//! - merge landing mid-request changes nothing held (pre-merge segments hold the same rows)
//! - `deletes()` table: a key's tombstone in any segment = absent, whatever the order
//!   (insert-once / remove-once: at most one value + one tombstone exist)

use std::{ops::Range, sync::Arc};

use bytes::Bytes;
use rayon::prelude::*;

use super::{
    dir::SegmentDir,
    file::{Prefetch, SegmentFile},
    layout::Shape,
    Result, SegmentMeta,
};
use crate::{
    fs::Access,
    pages::{Pages, PAGE},
};

/// Batch size from which [`SegmentsView::get_many`] fans out on rayon (measured, 9 segments, 2.3M
/// rows: 2-16 keys 5-10× slower on rayon (wake-up), crossover ≈ 64, 256 keys 3× faster)
const PARALLEL_FROM: usize = 64;

/// Map's durable segments, mapped for reads
pub(crate) struct SegmentsView {
    shape: Shape,
    segments: Vec<Arc<SegmentFile>>,
}

impl std::fmt::Debug for SegmentsView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentsView").field("segments", &self.segments.len()).finish()
    }
}

/// `meta`'s segment mapped for reads, its filter read and checked up front
fn mapped(dir: &SegmentDir, shape: Shape, meta: &SegmentMeta) -> Result<Arc<SegmentFile>> {
    // probed = point lookups on hash-like keys, else range scans
    let access = if shape.probed { Access::Random } else { Access::Normal };
    let file = SegmentFile::open(dir, meta, shape, access)?;
    file.warm_filter();
    Ok(Arc::new(file))
}

/// Ascending byte ranges widened to whole pages (readahead's unit), merged where they overlap or
/// touch
///
/// - one readahead request per run of contiguous pages
/// - empty ranges dropped
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

impl SegmentsView {
    /// Every listed segment mapped (lengths only, pages checked on touch)
    pub(crate) fn open(dir: &SegmentDir, shape: Shape, listed: &[SegmentMeta]) -> Result<Self> {
        let segments = listed.iter().map(|meta| mapped(dir, shape, meta)).collect::<Result<_>>()?;
        Ok(Self { shape, segments })
    }

    /// `listed` mapped, sharing the segments this view already maps
    pub(crate) fn next(&self, dir: &SegmentDir, listed: &[SegmentMeta]) -> Result<Self> {
        let mut segments = Vec::with_capacity(listed.len());
        for meta in listed {
            let reused = self.segments.iter().find(|file| file.meta == *meta).map(Arc::clone);
            segments.push(match reused {
                Some(file) => file,
                None => mapped(dir, self.shape, meta)?,
            });
        }
        Ok(Self { shape: self.shape, segments })
    }

    /// `(key, value)` for `start <= key < end`, ascending across all segments (`None` once more
    /// than `limit` match)
    ///
    /// - stops scanning at row `limit + 1` (cost bounded by `limit`, not by the range)
    /// - `start` and `end` sharing the filtered prefix → only segments whose filter may hold it
    /// - `deletes()` table: every row in range read (a tombstone may cancel a row of any other
    ///   segment), keys with a tombstone dropped, then `limit` applied to what is left
    pub(crate) fn range(
        &self,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<(Bytes, Bytes)>> {
        let mut rows = Vec::new();
        for (file, slot) in self.scan(start, end) {
            // no tombstones: every row read is one answered
            if !self.shape.deletes && rows.len() == limit {
                return None;
            }
            rows.push(file.entry(slot));
        }
        rows.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        let mut found = Vec::new();
        for held in rows.chunk_by(|(a, _), (b, _)| a == b) {
            match held {
                [(key, Some(value))] => found.push((key.clone(), value.clone())),
                [(_, None)] | [(_, Some(_)), (_, None)] | [(_, None), (_, Some(_))] => {}
                many => panic!("a key held {} times across durable segments", many.len()),
            }
        }
        (found.len() <= limit).then_some(found)
    }

    /// Every candidate segment's rows in `start..end`, `(file, slot)`
    fn scan<'a>(
        &'a self,
        start: &'a [u8],
        end: &'a [u8],
    ) -> impl Iterator<Item = (&'a SegmentFile, usize)> {
        self.candidates(start, end).flat_map(move |file| {
            (file.seek(start)..file.records())
                .take_while(move |&slot| file.key(slot) < end)
                .map(move |slot| (file.as_ref(), slot))
        })
    }

    /// Segments a `start..end` scan reads: those whose filter may hold a shared filtered prefix
    fn candidates<'a>(
        &'a self,
        start: &'a [u8],
        end: &'a [u8],
    ) -> impl Iterator<Item = &'a Arc<SegmentFile>> {
        let filtered = self.shape.filtered;
        let prefix = (start.len() >= filtered && end.len() >= filtered)
            .then(|| &start[..filtered])
            .filter(|prefix| *prefix == &end[..filtered]);
        self.segments
            .iter()
            .filter(move |file| prefix.is_none_or(|prefix| file.may_contain(prefix)))
    }

    /// Values for `keys`, in `keys`' order (`None` = absent)
    ///
    /// - probed in ascending key order: neighbouring keys share fence, filter and record pages
    /// - [`PARALLEL_FROM`] keys and up: prefetched ([`prefetch`](Self::prefetch)), then probed on
    ///   the rayon pool
    /// - below [`PARALLEL_FROM`]: this thread, no prefetch
    pub(crate) fn get_many(&self, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        let mut sorted: Vec<(&[u8], usize)> =
            keys.iter().enumerate().map(|(at, key)| (*key, at)).collect();
        sorted.sort_unstable();
        if sorted.len() >= PARALLEL_FROM {
            self.prefetch(&sorted);
        }
        let probe = |(key, at): &(&[u8], usize)| (*at, self.get(key));
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

    /// Readahead for a batch of point lookups (`sorted` = ascending keys), advisory only
    /// - fences, then records, on rayon (why: `usage.md` "Page faults on async threads")
    fn prefetch(&self, sorted: &[(&[u8], usize)]) {
        for step in [Prefetch::Fences, Prefetch::Records] {
            let plan = self.prefetch_plan(step, sorted);
            plan.into_par_iter()
                .for_each(|(segment, range)| self.segments[segment].will_need(range));
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
        let per_segment = self.segments.par_iter().enumerate().map(|(segment, file)| {
            let wanted: Vec<Range<usize>> = sorted
                .par_iter()
                .filter(|(key, _)| file.may_contain(&key[..file.filtered()]))
                .map(|(key, _)| file.prefetch_range(step, key))
                .collect();
            coalesced(wanted.into_iter()).into_iter().map(move |range| (segment, range))
        });
        per_segment.flatten_iter().collect()
    }

    /// Value under `key` (each segment's filter first: a miss = a memory probe per segment)
    ///
    /// - newest segment first (lookups skew recent, list ≈ data age: batches append, a merge takes
    ///   its oldest input's slot)
    /// - order never changes an answer (keys unique across segments)
    /// - `deletes()` table: every admitting segment checked, a tombstone in any = absent
    pub(crate) fn get(&self, key: &[u8]) -> Option<Bytes> {
        assert_eq!(key.len(), self.shape.key_len, "a point lookup names a whole key");
        let mut held = self.segments.iter().rev().filter_map(|file| {
            if !file.may_contain(&key[..self.shape.filtered]) {
                return None;
            }
            let slot = file.seek(key);
            (slot < file.records() && file.key(slot) == key).then(|| file.entry(slot).1)
        });
        if !self.shape.deletes {
            return held.next().flatten();
        }
        let mut value = None;
        for held in held {
            match held {
                None => return None,
                Some(held) => value = Some(held),
            }
        }
        value
    }

    /// Every durable segment's file
    pub(crate) fn pages(&self) -> impl Iterator<Item = &Pages> {
        self.segments.iter().map(|file| file.pages())
    }

    /// Durable segments in list order (the merge policy's input)
    pub(crate) fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.iter().map(|file| file.meta).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - pages 0, 1, 2 touched → one run
    /// - page 4 after untouched page 3 → its own run
    /// - empty ranges vanish
    #[test]
    fn coalesced_widens_to_pages_and_merges_contiguous_runs() {
        let ranges = [0..10, 5..20, 20..20, 30..100, 4096..4100, 9000..9001, 17000..17001];
        let merged = coalesced(ranges.clone().into_iter());
        assert_eq!(merged, [0..3 * PAGE, 4 * PAGE..5 * PAGE], "coalesced runs of {ranges:?}");
    }
}
