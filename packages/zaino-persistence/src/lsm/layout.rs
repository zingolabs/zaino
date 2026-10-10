//! Segment file: packed records, then what a reader navigates by
//!
//! ```text
//! records   records × STRIDE, keys strictly ascending
//! fences    blocks × first key          block = BLOCK_BYTES / STRIDE records (≈ one page)
//! summary   groups × first key          group = BLOCK_BYTES / key_len fences (≈ one page)
//! filter    binary fuse over each key's first `filtered` bytes (`filter.rs`)
//! ```
//!
//! - summary in reader memory (≈ 1/100 of the fences): key's block = one page of fences touched,
//!   not a binary search over all of them on disk
//! - row = key ‖ value, both fixed width; a `deletes()` table adds a flag byte: key ‖ value ‖ flag
//!   (0 = value, 1 = tombstone with zeroed value bytes; [`encode_row`] / [`decode_row`])
//! - integrity = the file's page checksums (`crate::pages`); nothing here re-proven on read

use std::{cmp::Ordering, ops::Range, path::Path, sync::Arc};

use super::{
    filter::{FilterError, FilterLayout, FilterWriter},
    spill::{scratch_path, Spill},
};
use crate::{
    fs::Fs,
    pages::PagedFile,
    port::{MapTable, Width},
};

const BLOCK_BYTES: usize = 4096;

/// What a map's segments look like on disk
///
/// - `probed` = point lookups (scope 0: random access, filter over whole keys)
/// - `filtered` = key bytes the filter covers (whole key, or the scope)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Shape {
    pub(crate) stride: usize,
    pub(crate) key_len: usize,
    pub(crate) value_len: usize,
    pub(crate) block_rows: usize,
    pub(crate) group_fences: usize,
    pub(crate) probed: bool,
    pub(crate) filtered: usize,
    pub(crate) cache_writes: bool,
    pub(crate) deletes: bool,
}

/// One row's content: a value, or (on a `deletes()` table) the key's tombstone
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row<'a> {
    Value(&'a [u8]),
    Tombstone,
}

/// Flag byte of a `deletes()` table's row
const VALUE: u8 = 0;
const TOMBSTONE: u8 = 1;

/// `key ‖ value` (+ flag on a `deletes()` table) into `out`
///
/// - panics: a tombstone on a table without deletes, or widths off the shape (bugs)
pub(crate) fn encode_row(shape: &Shape, key: &[u8], row: Row<'_>, out: &mut Vec<u8>) {
    assert_eq!(key.len(), shape.key_len, "row key width");
    out.extend_from_slice(key);
    match row {
        Row::Value(value) => {
            assert_eq!(value.len(), shape.value_len, "row value width");
            out.extend_from_slice(value);
        }
        Row::Tombstone => {
            assert!(shape.deletes, "a tombstone in a table without deletes");
            out.resize(out.len() + shape.value_len, 0);
        }
    }
    if shape.deletes {
        out.push(if matches!(row, Row::Tombstone) { TOMBSTONE } else { VALUE });
    }
}

/// `(key, row)` of an encoded row; `None` = a flag that is neither, or a tombstone whose value
/// bytes are not zero (corruption: page checksums passed, so a writer bug)
pub(crate) fn decode_row<'a>(shape: &Shape, row: &'a [u8]) -> Option<(&'a [u8], Row<'a>)> {
    let (key, rest) = row.split_at(shape.key_len);
    let (value, flag) = rest.split_at(shape.value_len);
    let decoded = match flag {
        [] | [VALUE] => Row::Value(value),
        [TOMBSTONE] if value.iter().all(|&byte| byte == 0) => Row::Tombstone,
        _ => return None,
    };
    Some((key, decoded))
}

impl Shape {
    /// Panics: map the LSM cannot hold (schema = constant: mismatch = bug)
    pub(crate) fn of(table: &MapTable) -> Self {
        let name = &table.name;
        let Width::Fixed(key) = table.key else { panic!("LSM map {name}: keys must be Fixed") };
        let Width::Fixed(value) = table.value else {
            panic!("LSM map {name}: values must be Fixed")
        };
        let (key_len, scope) = (key.get() as usize, table.scope as usize);
        assert!(scope <= key_len, "LSM map {name}: scope {scope} > its {key_len}-byte key");
        let filtered = if scope == 0 { key_len } else { scope };
        assert!(filtered >= 8, "LSM map {name}: filter shards on 8 key bytes, has {filtered}");
        let value_len = value.get() as usize;
        let stride = key_len + value_len + usize::from(table.deletes);
        Self {
            stride,
            key_len,
            value_len,
            block_rows: (BLOCK_BYTES / stride).max(1),
            group_fences: (BLOCK_BYTES / key_len).max(1),
            probed: scope == 0,
            filtered,
            cache_writes: table.cache_writes,
            deletes: table.deletes,
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
    pub(crate) summary: Range<usize>,
    pub(crate) filter: (usize, FilterLayout),
}

impl Sections {
    /// `filter` = the filter section (from its offset), parsed for its shard offsets
    pub(crate) fn new(
        shape: Shape,
        records: u64,
        filter: impl FnOnce(usize) -> Option<FilterLayout>,
    ) -> Self {
        let fences = usize::try_from(records).expect("record count fits usize") * shape.stride;
        let summary_at = fences + shape.blocks(records) * shape.key_len;
        let filter_at = summary_at + shape.groups(records) * shape.key_len;
        let filter = filter(filter_at).expect("a sealed segment's filter section parses");
        let records = usize::try_from(records).expect("record count fits usize");
        Self { shape, records, fences, summary: summary_at..filter_at, filter: (filter_at, filter) }
    }

    pub(crate) fn fence(&self, b: usize) -> Range<usize> {
        let at = self.fences + b * self.shape.key_len;
        at..at + self.shape.key_len
    }

    /// Blocks whose fences a search for `key` reads: the group `summary` (in memory) picks, so
    /// no disk read; spans at most two pages of fences
    pub(crate) fn group_of(&self, summary: &[u8], key: &[u8]) -> Range<usize> {
        let key_len = self.shape.key_len;
        let groups = summary.len() / key_len;
        let group = last_at_most(groups, |g| &summary[g * key_len..(g + 1) * key_len], key);
        let low = group * self.shape.group_fences;
        low..(low + self.shape.group_fences).min(self.shape.blocks(self.records as u64))
    }

    /// Block a key could sit in: the last whose fence ≤ `key` (block 0 when below every fence),
    /// searched inside [`group_of`](Self::group_of)
    pub(crate) fn block_of<'a>(
        &self,
        summary: &[u8],
        fence: impl Fn(usize) -> &'a [u8],
        key: &[u8],
    ) -> usize {
        let group = self.group_of(summary, key);
        group.start + last_at_most(group.len(), |b| fence(group.start + b), key)
    }

    /// Bytes of block `b`'s records
    pub(crate) fn rows(&self, b: usize) -> Range<usize> {
        let rows = self.shape.block_rows;
        let (first, end) = (b * rows, ((b + 1) * rows).min(self.records));
        first * self.shape.stride..end * self.shape.stride
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

/// Segment's navigation sections, built while its records stream out in key order
///
/// - fences + filter fingerprints via [`Spill`]s (memory bounded whatever the segment's size)
/// - in memory: only the summary (1/100 of the fences) + the filter's shard table
pub(crate) struct Navigation {
    shape: Shape,
    records: u64,
    fences: Spill,
    summary: Vec<u8>,
    filter: FilterWriter<Spill>,
    previous: Vec<u8>,
}

impl Navigation {
    /// - `records` = most it will be pushed (sizes the filter's shards; a merge cancelling pairs
    ///   pushes fewer)
    /// - scratch files, if any, beside segment `id` in `dir`
    pub(crate) fn new(shape: Shape, records: u64, fs: &Arc<dyn Fs>, dir: &Path, id: u32) -> Self {
        let spill = |part| Spill::new(Arc::clone(fs), scratch_path(dir, id, part));
        Self {
            shape,
            records: 0,
            fences: spill("fences"),
            summary: Vec::with_capacity(shape.groups(records) * shape.key_len),
            filter: FilterWriter::new(records, spill("filter")),
            previous: Vec::with_capacity(shape.key_len),
        }
    }

    /// Encoded record, strictly after the one before (asserted: else a sort or merge bug)
    ///
    /// - filter takes each distinct filtered prefix once (keys sharing one arrive together)
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
        let filtered = self.shape.filtered;
        if first || self.previous[..filtered] != key[..filtered] {
            self.filter.push(&key[..filtered])?;
        }
        self.previous.clear();
        self.previous.extend_from_slice(key);
        self.records += 1;
        Ok(())
    }

    /// Records pushed so far
    pub(crate) fn records(&self) -> u64 {
        self.records
    }

    /// `fences ‖ summary ‖ filter`, appended to `out` after the records (scratch files removed)
    pub(crate) fn finish(self, out: &mut PagedFile) -> Result<(), FilterError> {
        self.fences.append_to(out)?;
        out.append(&self.summary)?;
        let (head, fingerprints) = self.filter.finish()?;
        out.append(&head)?;
        fingerprints.append_to(out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - `key ‖ value` without `deletes()`; `key ‖ value ‖ flag` with (tombstone value = zeros)
    /// - each decodes back; an unknown flag or a non-zero tombstone value decodes to `None`
    #[test]
    fn rows_encode_to_golden_bytes_and_a_corrupt_flag_decodes_to_none() {
        let table = MapTable::new(0, "rows", Width::fixed(8), Width::fixed(4), 0);
        let (plain, removable) = (Shape::of(&table), Shape::of(&table.deletes()));
        let key = [7u8; 8];
        let cases: [(Shape, Row<'_>, &[u8]); 3] = [
            (plain, Row::Value(&[1, 2, 3, 4]), &[7, 7, 7, 7, 7, 7, 7, 7, 1, 2, 3, 4]),
            (removable, Row::Value(&[1, 2, 3, 4]), &[7, 7, 7, 7, 7, 7, 7, 7, 1, 2, 3, 4, 0]),
            (removable, Row::Tombstone, &[7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 0, 1]),
        ];
        for (shape, row, golden) in cases {
            let mut out = Vec::new();
            encode_row(&shape, &key, row, &mut out);
            assert_eq!(out, golden, "{row:?} encoded");
            assert_eq!(out.len(), shape.stride, "{row:?} = one stride");
            assert_eq!(decode_row(&shape, &out), Some((&key[..], row)), "{row:?} decoded");
        }
        let corrupt: [&[u8]; 2] =
            [&[7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 0, 2], &[7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 9, 1]];
        for row in corrupt {
            assert_eq!(decode_row(&removable, row), None, "{row:?}");
        }
    }
}
