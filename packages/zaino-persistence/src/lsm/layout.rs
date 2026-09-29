//! One segment file: packed records, then what a reader navigates by
//!
//! ```text
//! records   records × STRIDE, keys strictly ascending
//! fences    blocks × first key          block = BLOCK_BYTES / STRIDE records (≈ one page)
//! filter    probed sets only (`filter.rs`), else absent
//! ```
//!
//! - integrity = the file's page checksums (`crate::pages`); nothing here re-proven on read

use std::cmp::Ordering;

use super::{
    filter::{FilterLayout, FilterWriter},
    record::{Key, Record},
};

const BLOCK_BYTES: usize = 4096;

/// What a record type's segments look like on disk
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) stride: usize,
    pub(crate) key_len: usize,
    pub(crate) block_rows: usize,
    pub(crate) probed: bool,
}

impl Shape {
    pub(crate) fn of<R: Record>() -> Self {
        const {
            assert!(<R::Key as Key>::LEN > 0, "keys hold bytes");
            assert!(<R::Key as Key>::LEN <= R::STRIDE, "key within its record");
            assert!(!<R::Key as Key>::PROBED || <R::Key as Key>::LEN >= 8, "probed key ≥ 8 bytes");
        };
        let (key_len, probed) = (<R::Key as Key>::LEN, <R::Key as Key>::PROBED);
        Self { stride: R::STRIDE, key_len, block_rows: (BLOCK_BYTES / R::STRIDE).max(1), probed }
    }

    pub(crate) fn blocks(&self, records: u64) -> usize {
        usize::try_from(records.div_ceil(self.block_rows as u64)).expect("block count fits usize")
    }
}

/// Where a segment's sections sit (from its record count and total length)
#[derive(Debug)]
pub(crate) struct Sections {
    pub(crate) shape: Shape,
    pub(crate) records: usize,
    pub(crate) fences: usize,
    pub(crate) filter: Option<(usize, FilterLayout)>,
}

impl Sections {
    /// `filter` = the filter section's bytes (probed sets), parsed for its shard offsets
    pub(crate) fn new(
        shape: Shape,
        records: u64,
        len: usize,
        filter: impl FnOnce(usize) -> Option<FilterLayout>,
    ) -> Self {
        let records = usize::try_from(records).expect("record count fits usize");
        let fences = records * shape.stride;
        let filter_at = fences + shape.blocks(records as u64) * shape.key_len;
        let filter = shape.probed.then(|| {
            (filter_at, filter(filter_at).expect("a sealed segment's filter section parses"))
        });
        assert!(shape.probed || filter_at == len, "an unprobed segment ends at its fences");
        Self { shape, records, fences, filter }
    }

    pub(crate) fn fence(&self, b: usize) -> std::ops::Range<usize> {
        let at = self.fences + b * self.shape.key_len;
        at..at + self.shape.key_len
    }

    /// Block a key could sit in: last whose fence ≤ `key` (block 0 when below every fence)
    pub(crate) fn block_of<'a>(&self, fence: impl Fn(usize) -> &'a [u8], key: &[u8]) -> usize {
        let (mut low, mut high) = (0, self.shape.blocks(self.records as u64));
        while low < high {
            let mid = low + (high - low) / 2;
            match fence(mid).cmp(key) {
                Ordering::Greater => high = mid,
                _ => low = mid + 1,
            }
        }
        low.saturating_sub(1)
    }
}

/// A segment's navigation sections, built while its records stream out in key order
pub(crate) struct Navigation {
    shape: Shape,
    records: u64,
    fences: Vec<u8>,
    filter: Option<FilterWriter>,
    previous: Vec<u8>,
}

impl Navigation {
    /// `records` = the exact count that will be pushed (sizes the filter's shards)
    pub(crate) fn new(shape: Shape, records: u64) -> Self {
        Self {
            shape,
            records: 0,
            fences: Vec::with_capacity(shape.blocks(records) * shape.key_len),
            filter: shape.probed.then(|| FilterWriter::new(records)),
            previous: Vec::with_capacity(shape.key_len),
        }
    }

    /// One encoded record, strictly after the one before (asserted: a sort or merge bug)
    pub(crate) fn push(&mut self, row: &[u8]) -> Result<(), &'static str> {
        assert_eq!(row.len(), self.shape.stride, "record encodes to its stride");
        let key = &row[..self.shape.key_len];
        let ascending = self.records == 0 || self.previous.as_slice() < key;
        assert!(ascending, "segment records strictly ascending (duplicate or unsorted batch)");
        if (self.records as usize).is_multiple_of(self.shape.block_rows) {
            self.fences.extend_from_slice(key);
        }
        if let Some(filter) = &mut self.filter {
            filter.push(key)?;
        }
        self.previous.clear();
        self.previous.extend_from_slice(key);
        self.records += 1;
        Ok(())
    }

    /// `fences ‖ filter`, appended after the records
    pub(crate) fn finish(self, expected: u64) -> Result<Vec<u8>, &'static str> {
        assert_eq!(self.records, expected, "pushed every record the segment was sized for");
        let mut out = self.fences;
        if let Some(filter) = self.filter {
            out.extend_from_slice(&filter.finish()?);
        }
        Ok(out)
    }
}
