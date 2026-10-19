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
//! - row = key ‖ value, both fixed width; a `deletes` map adds a flag byte: key ‖ value ‖ flag
//!   (0 = value, 1 = tombstone with zeroed value bytes; [`encode_row`] / [`decode_row`])
//! - integrity = the file's page checksums (`crate::pages`); nothing here re-proven on read

use std::{cmp::Ordering, num::NonZeroU32, ops::Range, path::Path, sync::Arc};

use super::{
    filter::{FilterError, FilterLayout, FilterWriter},
    spill::{scratch_path, Spill},
    Breach, Result, SegmentError, Tuning,
};
use crate::{fs::Fs, pages::PagedFile};

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

/// One row's content: a value, or (on a `deletes` map) the key's tombstone
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row<'a> {
    Value(&'a [u8]),
    Tombstone,
}

/// Flag byte of a `deletes` map's row
const VALUE: u8 = 0;
const TOMBSTONE: u8 = 1;

/// `key ‖ value` (+ flag on a `deletes` map) into `out`
///
/// - a tombstone on a map without deletes, or widths off the shape = `Internal` (callers check
///   both first)
pub(crate) fn encode_row(shape: &Shape, key: &[u8], row: Row<'_>, out: &mut Vec<u8>) -> Result<()> {
    if key.len() != shape.key_len {
        return Err(SegmentError::Internal("row key off its shape's width"));
    }
    out.extend_from_slice(key);
    match row {
        Row::Value(value) => {
            if value.len() != shape.value_len {
                return Err(SegmentError::Internal("row value off its shape's width"));
            }
            out.extend_from_slice(value);
        }
        Row::Tombstone => {
            if !shape.deletes {
                return Err(SegmentError::Internal(
                    "a tombstone in a map without deletes",
                ));
            }
            out.resize(out.len().saturating_add(shape.value_len), 0);
        }
    }
    if shape.deletes {
        out.push(if matches!(row, Row::Tombstone) {
            TOMBSTONE
        } else {
            VALUE
        });
    }
    Ok(())
}

/// `(key, row)` of an encoded row; `None` = shorter than the shape, a flag that is neither, or a
/// tombstone whose value bytes are not zero (page checksums passed, so a writer bug)
pub(crate) fn decode_row<'a>(shape: &Shape, row: &'a [u8]) -> Option<(&'a [u8], Row<'a>)> {
    let (key, rest) = row.split_at_checked(shape.key_len)?;
    let (value, flag) = rest.split_at_checked(shape.value_len)?;
    let decoded = match flag {
        [] | [VALUE] => Row::Value(value),
        [TOMBSTONE] if value.iter().all(|&byte| byte == 0) => Row::Tombstone,
        _ => return None,
    };
    Some((key, decoded))
}

impl Shape {
    /// Map `name` of `key`-byte keys to `value`-byte values (removable when `deletes`), laid out
    /// by `tuning`; a `scope` past the key = `Shape`
    pub(crate) fn of(
        name: &str,
        key: NonZeroU32,
        value: NonZeroU32,
        deletes: bool,
        tuning: Tuning,
    ) -> Result<Self> {
        let refused = |reason: String| SegmentError::Shape {
            name: name.to_owned(),
            reason,
        };
        let width = |width: NonZeroU32| {
            usize::try_from(width.get()).map_err(|_| refused(format!("{width}-byte items")))
        };
        let (key_len, value_len) = (width(key)?, width(value)?);
        let scope =
            usize::try_from(tuning.scope).map_err(|_| refused("scope past usize".to_owned()))?;
        if scope > key_len {
            return Err(refused(format!("scope {scope} > its {key_len}-byte key")));
        }
        let filtered = if scope == 0 { key_len } else { scope };
        let stride = key_len
            .checked_add(value_len)
            .and_then(|stride| stride.checked_add(usize::from(deletes)))
            .ok_or_else(|| refused("row width past usize".to_owned()))?;
        Ok(Self {
            stride,
            key_len,
            value_len,
            block_rows: BLOCK_BYTES.checked_div(stride).unwrap_or(0).max(1),
            group_fences: BLOCK_BYTES.checked_div(key_len).unwrap_or(0).max(1),
            probed: scope == 0,
            filtered,
            cache_writes: tuning.cache_writes,
            deletes,
        })
    }

    pub(crate) fn blocks(&self, records: u64) -> Result<usize> {
        let blocks = records.div_ceil(self.block_rows as u64);
        usize::try_from(blocks).map_err(|_| SegmentError::Overflow("block count"))
    }

    /// Summary entries: one per `group_fences` fences
    pub(crate) fn groups(&self, records: u64) -> Result<usize> {
        Ok(self.blocks(records)?.div_ceil(self.group_fences))
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

/// `a * b + c`, or `Overflow(what)`
fn offset(a: usize, b: usize, c: usize, what: &'static str) -> Result<usize> {
    a.checked_mul(b)
        .and_then(|ab| ab.checked_add(c))
        .ok_or(SegmentError::Overflow(what))
}

impl Sections {
    /// `filter` = the filter section (from its offset), parsed for its shard offsets
    pub(crate) fn new(
        segment: u32,
        shape: Shape,
        records: u64,
        filter: impl FnOnce(usize) -> Result<Option<FilterLayout>>,
    ) -> Result<Self> {
        let records =
            usize::try_from(records).map_err(|_| SegmentError::Overflow("record count"))?;
        let fences = offset(records, shape.stride, 0, "records section")?;
        let summary_at = offset(
            shape.blocks(records as u64)?,
            shape.key_len,
            fences,
            "fences",
        )?;
        let filter_at = offset(
            shape.groups(records as u64)?,
            shape.key_len,
            summary_at,
            "summary",
        )?;
        let filter = filter(filter_at)?.ok_or(SegmentError::Corrupt {
            segment,
            reason: "filter section does not parse",
        })?;
        Ok(Self {
            shape,
            records,
            fences,
            summary: summary_at..filter_at,
            filter: (filter_at, filter),
        })
    }

    pub(crate) fn fence(&self, b: usize) -> Result<Range<usize>> {
        let at = offset(b, self.shape.key_len, self.fences, "fence offset")?;
        let end = offset(1, self.shape.key_len, at, "fence offset")?;
        Ok(at..end)
    }

    /// Blocks whose fences a search for `key` reads: the group `summary` (in memory) picks, so
    /// no disk read; spans at most two pages of fences
    pub(crate) fn group_of(&self, summary: &[u8], key: &[u8]) -> Result<Range<usize>> {
        let key_len = self.shape.key_len;
        let groups = summary.len().checked_div(key_len).unwrap_or(0);
        let entry = |g: usize| {
            let at = offset(g, key_len, 0, "summary entry")?;
            let end = offset(1, key_len, at, "summary entry")?;
            summary
                .get(at..end)
                .ok_or(SegmentError::Internal("summary entry past the summary"))
        };
        let group = last_at_most(groups, entry, key)?;
        let low = offset(group, self.shape.group_fences, 0, "fence group")?;
        let blocks = self.shape.blocks(self.records as u64)?;
        Ok(low..low.saturating_add(self.shape.group_fences).min(blocks))
    }

    /// Block a key could sit in: the last whose fence ≤ `key` (block 0 when below every fence),
    /// searched inside [`group_of`](Self::group_of)
    pub(crate) fn block_of<'a>(
        &self,
        summary: &[u8],
        fence: impl Fn(usize) -> Result<&'a [u8]>,
        key: &[u8],
    ) -> Result<usize> {
        let group = self.group_of(summary, key)?;
        let within = last_at_most(group.len(), |b| fence(group.start.saturating_add(b)), key)?;
        Ok(group.start.saturating_add(within))
    }

    /// Slots of block `b`
    pub(crate) fn slots(&self, b: usize) -> Range<usize> {
        let rows = self.shape.block_rows;
        let first = b.saturating_mul(rows).min(self.records);
        first..first.saturating_add(rows).min(self.records)
    }

    /// Bytes of block `b`'s records
    pub(crate) fn rows(&self, b: usize) -> Result<Range<usize>> {
        let slots = self.slots(b);
        let stride = self.shape.stride;
        Ok(offset(slots.start, stride, 0, "block offset")?
            ..offset(slots.end, stride, 0, "block offset")?)
    }
}

/// Index of the last of `count` ascending entries ≤ `key` (0 when every entry is above it)
fn last_at_most<'a>(
    count: usize,
    entry: impl Fn(usize) -> Result<&'a [u8]>,
    key: &[u8],
) -> Result<usize> {
    let (mut low, mut high) = (0, count);
    while low < high {
        let mid = low.midpoint(high);
        match entry(mid)?.cmp(key) {
            Ordering::Greater => high = mid,
            _ => low = mid.saturating_add(1),
        }
    }
    Ok(low.saturating_sub(1))
}

/// Segment's navigation sections, built while its records stream out in key order
///
/// - fences + filter fingerprints via [`Spill`]s (memory bounded whatever the segment's size)
/// - in memory: only the summary (1/100 of the fences) + the filter's shard table
pub(crate) struct Navigation {
    segment: u32,
    shape: Shape,
    records: u64,
    fences: u64,
    spilled_fences: Spill,
    summary: Vec<u8>,
    filter: FilterWriter<Spill>,
    previous: Vec<u8>,
}

impl Navigation {
    /// - `records` = most it will be pushed (sizes the filter's shards; a merge cancelling pairs
    ///   pushes fewer)
    /// - scratch files, if any, beside segment `id` in `dir`
    pub(crate) fn new(
        shape: Shape,
        records: u64,
        fs: &Arc<dyn Fs>,
        dir: &Path,
        id: u32,
    ) -> Result<Self> {
        let spill = |part| Spill::new(Arc::clone(fs), scratch_path(dir, id, part));
        let summary = shape.groups(records)?.saturating_mul(shape.key_len);
        Ok(Self {
            segment: id,
            shape,
            records: 0,
            fences: 0,
            spilled_fences: spill("fences"),
            summary: Vec::with_capacity(summary),
            filter: FilterWriter::new(records, spill("filter")),
            previous: Vec::with_capacity(shape.key_len),
        })
    }

    /// Encoded record, strictly after the one before (else `Contract`: a duplicate or unsorted
    /// batch)
    ///
    /// - filter takes each distinct filtered prefix once (keys sharing one arrive together)
    pub(crate) fn push(&mut self, row: &[u8]) -> Result<()> {
        let key = match row.len() == self.shape.stride {
            true => row.get(..self.shape.key_len),
            false => None,
        }
        .ok_or(SegmentError::Internal("record off its stride"))?;
        let first = self.records == 0;
        if !first && self.previous.as_slice() >= key {
            return Err(SegmentError::Contract {
                segments: vec![self.segment],
                breach: Breach::Unsorted,
            });
        }
        if self.records.is_multiple_of(self.shape.block_rows as u64) {
            if self.fences.is_multiple_of(self.shape.group_fences as u64) {
                self.summary.extend_from_slice(key);
            }
            self.spilled_fences.push(key)?;
            self.fences = self.fences.saturating_add(1);
        }
        let filtered = key.get(..self.shape.filtered).unwrap_or(key);
        let previous = self
            .previous
            .get(..self.shape.filtered)
            .unwrap_or(&self.previous);
        if first || previous != filtered {
            self.filter
                .push(filtered)
                .map_err(|error| self.filter_error(error))?;
        }
        self.previous.clear();
        self.previous.extend_from_slice(key);
        self.records = self.records.saturating_add(1);
        Ok(())
    }

    fn filter_error(&self, error: FilterError) -> SegmentError {
        match error {
            FilterError::Build(reason) => SegmentError::Filter {
                segment: self.segment,
                reason,
            },
            FilterError::Io(error) => SegmentError::Io(error),
        }
    }

    /// Records pushed so far
    pub(crate) fn records(&self) -> u64 {
        self.records
    }

    /// `fences ‖ summary ‖ filter`, appended to `out` after the records (scratch files removed)
    pub(crate) fn finish(self, out: &mut PagedFile) -> Result<()> {
        let segment = self.segment;
        self.spilled_fences.append_to(out)?;
        out.append(&self.summary)?;
        let (head, fingerprints) = self.filter.finish().map_err(|error| match error {
            FilterError::Build(reason) => SegmentError::Filter { segment, reason },
            FilterError::Io(error) => SegmentError::Io(error),
        })?;
        out.append(&head)?;
        fingerprints.append_to(out)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(deletes: bool) -> Shape {
        let width = |n| NonZeroU32::new(n).expect("non-zero");
        Shape::of("rows", width(8), width(4), deletes, Tuning::default()).expect("shape")
    }

    /// - `key ‖ value` without deletes; `key ‖ value ‖ flag` with (tombstone value = zeros)
    /// - each decodes back; an unknown flag, a non-zero tombstone value or a short row decodes to
    ///   `None`
    #[test]
    fn rows_encode_to_golden_bytes_and_a_corrupt_flag_decodes_to_none() {
        let (plain, removable) = (shape(false), shape(true));
        let key = [7u8; 8];
        let cases: [(Shape, Row<'_>, &[u8]); 3] = [
            (
                plain,
                Row::Value(&[1, 2, 3, 4]),
                &[7, 7, 7, 7, 7, 7, 7, 7, 1, 2, 3, 4],
            ),
            (
                removable,
                Row::Value(&[1, 2, 3, 4]),
                &[7, 7, 7, 7, 7, 7, 7, 7, 1, 2, 3, 4, 0],
            ),
            (
                removable,
                Row::Tombstone,
                &[7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 0, 1],
            ),
        ];
        for (shape, row, golden) in cases {
            let mut out = Vec::new();
            encode_row(&shape, &key, row, &mut out).expect("encode");
            assert_eq!(out, golden, "{row:?} encoded");
            assert_eq!(out.len(), shape.stride, "{row:?} = one stride");
            assert_eq!(
                decode_row(&shape, &out),
                Some((&key[..], row)),
                "{row:?} decoded"
            );
        }
        let corrupt: [&[u8]; 3] = [
            &[7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 0, 2],
            &[7, 7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 9, 1],
            &[7, 7, 7],
        ];
        for row in corrupt {
            assert_eq!(decode_row(&removable, row), None, "{row:?}");
        }
    }

    /// Rows off the shape and tombstones without deletes are errors, never panics
    #[test]
    fn rows_off_their_shape_are_refused() {
        let mut out = Vec::new();
        let plain = shape(false);
        assert!(encode_row(&plain, &[7; 7], Row::Value(&[0; 4]), &mut out).is_err());
        assert!(encode_row(&plain, &[7; 8], Row::Value(&[0; 3]), &mut out).is_err());
        assert!(encode_row(&plain, &[7; 8], Row::Tombstone, &mut out).is_err());
    }

    /// Short keys allowed (any scope up to the key); a scope past the key refused
    #[test]
    fn shapes_take_any_key_width_and_refuse_a_scope_past_it() {
        let width = |n| NonZeroU32::new(n).expect("non-zero");
        let tiny =
            Shape::of("tiny", width(2), width(1), false, Tuning::default()).expect("2-byte key");
        assert_eq!((tiny.stride, tiny.filtered, tiny.probed), (3, 2, true));
        let scoped = Tuning {
            scope: 13,
            cache_writes: false,
        };
        let refused = Shape::of("wide", width(12), width(8), false, scoped);
        assert!(
            matches!(&refused, Err(SegmentError::Shape { reason, .. }) if reason.contains("scope 13")),
            "{refused:?}"
        );
    }
}
