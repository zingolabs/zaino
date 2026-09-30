//! One segment file: packed records, then what a reader navigates by
//!
//! ```text
//! records   records × STRIDE, keys strictly ascending
//! fences    blocks × first key          block = BLOCK_BYTES / STRIDE records (≈ one page)
//! summary   groups × first key          group = BLOCK_BYTES / key_len fences (≈ one page)
//! filter    filtered sets only (`filter.rs`), else absent
//! ```
//!
//! A reader keeps the summary in memory (about 1/100 of the fences), so finding a key's block
//! touches one page of fences instead of binary-searching all of them on disk.
//!
//! - integrity = the file's page checksums (`crate::pages`); nothing here re-proven on read

use std::{cmp::Ordering, path::Path, sync::Arc};

use super::{
    filter::{FilterError, FilterLayout, FilterWriter},
    record::{Key, Record},
    spill::{scratch_path, Spill},
};
use crate::{fs::Fs, pages::PagedFile};

const BLOCK_BYTES: usize = 4096;

/// What a record type's segments look like on disk
///
/// - `probed` = point lookups (random access); `filtered` = key bytes the filter covers (the whole
///   key for probed sets, `FILTER_PREFIX` for range-scanned ones, 0 = no filter)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) stride: usize,
    pub(crate) key_len: usize,
    pub(crate) block_rows: usize,
    pub(crate) group_fences: usize,
    pub(crate) probed: bool,
    pub(crate) filtered: usize,
}

impl Shape {
    pub(crate) fn of<R: Record>() -> Self {
        let (key_len, probed) = (<R::Key as Key>::LEN, <R::Key as Key>::PROBED);
        let prefix = <R::Key as Key>::FILTER_PREFIX;
        const {
            let (len, prefix) = (<R::Key as Key>::LEN, <R::Key as Key>::FILTER_PREFIX);
            assert!(len > 0, "keys hold bytes");
            assert!(len <= R::STRIDE, "key within its record");
            assert!(!<R::Key as Key>::PROBED || len >= 8, "probed key ≥ 8 bytes");
            assert!(!<R::Key as Key>::PROBED || prefix == 0, "a probed set filters whole keys");
            assert!(prefix == 0 || (8 <= prefix && prefix <= len), "filter prefix 8..=LEN bytes");
        };
        Self {
            stride: R::STRIDE,
            key_len,
            block_rows: (BLOCK_BYTES / R::STRIDE).max(1),
            group_fences: (BLOCK_BYTES / key_len).max(1),
            probed,
            filtered: if probed { key_len } else { prefix },
        }
    }

    pub(crate) fn blocks(&self, records: u64) -> usize {
        usize::try_from(records.div_ceil(self.block_rows as u64)).expect("block count fits usize")
    }

    /// Summary entries: one per `group_fences` fences
    pub(crate) fn groups(&self, records: u64) -> usize {
        self.blocks(records).div_ceil(self.group_fences)
    }
}

/// Where a segment's sections sit (from its record count and total length)
#[derive(Debug)]
pub(crate) struct Sections {
    pub(crate) shape: Shape,
    pub(crate) records: usize,
    pub(crate) fences: usize,
    pub(crate) summary: std::ops::Range<usize>,
    pub(crate) filter: Option<(usize, FilterLayout)>,
}

impl Sections {
    /// `filter` = the filter section's bytes (filtered sets), parsed for its shard offsets
    pub(crate) fn new(
        shape: Shape,
        records: u64,
        len: usize,
        filter: impl FnOnce(usize) -> Option<FilterLayout>,
    ) -> Self {
        let fences = usize::try_from(records).expect("record count fits usize") * shape.stride;
        let summary_at = fences + shape.blocks(records) * shape.key_len;
        let filter_at = summary_at + shape.groups(records) * shape.key_len;
        let filter = (shape.filtered > 0).then(|| {
            (filter_at, filter(filter_at).expect("a sealed segment's filter section parses"))
        });
        assert!(filter.is_some() || filter_at == len, "an unfiltered segment ends at its summary");
        let records = usize::try_from(records).expect("record count fits usize");
        Self { shape, records, fences, summary: summary_at..filter_at, filter }
    }

    pub(crate) fn fence(&self, b: usize) -> std::ops::Range<usize> {
        let at = self.fences + b * self.shape.key_len;
        at..at + self.shape.key_len
    }

    /// Block a key could sit in: the last whose fence ≤ `key` (block 0 when below every fence)
    ///
    /// `summary` (in memory) picks the group of fences, then one search inside that group, which
    /// spans at most two pages of fences on disk.
    pub(crate) fn block_of<'a>(
        &self,
        summary: &[u8],
        fence: impl Fn(usize) -> &'a [u8],
        key: &[u8],
    ) -> usize {
        let key_len = self.shape.key_len;
        let groups = summary.len() / key_len;
        let group = last_at_most(groups, |g| &summary[g * key_len..(g + 1) * key_len], key);
        let low = group * self.shape.group_fences;
        let high = (low + self.shape.group_fences).min(self.shape.blocks(self.records as u64));
        low + last_at_most(high - low, |b| fence(low + b), key)
    }
}

/// Index of the last of `count` ascending entries ≤ `key` (0 when every entry is above it)
fn last_at_most<'a>(count: usize, entry: impl Fn(usize) -> &'a [u8], key: &[u8]) -> usize {
    let (mut low, mut high) = (0, count);
    while low < high {
        let mid = low + (high - low) / 2;
        match entry(mid).cmp(key) {
            Ordering::Greater => high = mid,
            _ => low = mid + 1,
        }
    }
    low.saturating_sub(1)
}

/// A segment's navigation sections, built while its records stream out in key order
///
/// - fences and filter fingerprints go through [`Spill`]s, so memory stays bounded whatever the
///   segment's size; only the summary (1/100 of the fences) and the filter's shard table stay
///   in memory
pub(crate) struct Navigation {
    shape: Shape,
    records: u64,
    fences: Spill,
    summary: Vec<u8>,
    filter: Option<FilterWriter<Spill>>,
    previous: Vec<u8>,
}

impl Navigation {
    /// `records` = the exact count that will be pushed (sizes the filter's shards); scratch
    /// files, if any, go beside segment `id` in `dir`
    pub(crate) fn new(shape: Shape, records: u64, fs: &Arc<dyn Fs>, dir: &Path, id: u32) -> Self {
        let spill = |part| Spill::new(Arc::clone(fs), scratch_path(dir, id, part));
        Self {
            shape,
            records: 0,
            fences: spill("fences"),
            summary: Vec::with_capacity(shape.groups(records) * shape.key_len),
            filter: (shape.filtered > 0).then(|| FilterWriter::new(records, spill("filter"))),
            previous: Vec::with_capacity(shape.key_len),
        }
    }

    /// One encoded record, strictly after the one before (asserted: a sort or merge bug)
    ///
    /// - the filter takes each distinct filtered prefix once (keys sharing one arrive together)
    pub(crate) fn push(&mut self, row: &[u8]) -> Result<(), FilterError> {
        assert_eq!(row.len(), self.shape.stride, "record encodes to its stride");
        let key = &row[..self.shape.key_len];
        let first = self.records == 0;
        assert!(
            first || self.previous.as_slice() < key,
            "segment records strictly ascending (duplicate or unsorted batch)"
        );
        if (self.records as usize).is_multiple_of(self.shape.block_rows) {
            let fence = self.fences.len() / self.shape.key_len as u64;
            if fence.is_multiple_of(self.shape.group_fences as u64) {
                self.summary.extend_from_slice(key);
            }
            self.fences.push(key)?;
        }
        if let Some(filter) = &mut self.filter {
            let filtered = self.shape.filtered;
            if first || self.previous[..filtered] != key[..filtered] {
                filter.push(&key[..filtered])?;
            }
        }
        self.previous.clear();
        self.previous.extend_from_slice(key);
        self.records += 1;
        Ok(())
    }

    /// `fences ‖ summary ‖ filter`, appended to `out` after the records (scratch files removed)
    pub(crate) fn finish(self, expected: u64, out: &mut PagedFile) -> Result<(), FilterError> {
        assert_eq!(self.records, expected, "pushed every record the segment was sized for");
        self.fences.append_to(out)?;
        out.append(&self.summary)?;
        if let Some(filter) = self.filter {
            let (head, fingerprints) = filter.finish()?;
            out.append(&head)?;
            fingerprints.append_to(out)?;
        }
        Ok(())
    }
}
