//! Listed segment, mapped: what readers and merges hold

use std::{ops::Range, path::Path, sync::OnceLock};

use bytes::Bytes;

use super::{
    file_name,
    filter::FilterLayout,
    layout::{decode_row, Row, Sections, Shape},
    Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::Pages,
};

/// Seek's two reads, in order: fence group, then block of records
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Prefetch {
    Fences,
    Records,
}

/// - `summary` = the summary section, copied into memory at open (one key per page of fences)
/// - `unlinked` = its file deleted while mapped: counted there until dropped
#[derive(Debug)]
pub(super) struct SegmentFile {
    pub(crate) meta: SegmentMeta,
    filtered: usize,
    pages: Pages,
    sections: Sections,
    summary: Box<[u8]>,
    unlinked: OnceLock<metrics::Gauge>,
}

impl Drop for SegmentFile {
    fn drop(&mut self) {
        if let Some(unlinked) = self.unlinked.get() {
            let bytes = self.meta.sealed.len as f64;
            crate::emit::in_drop(|| unlinked.decrement(bytes));
        }
    }
}

impl SegmentFile {
    /// Maps `meta`'s segment (lengths and checksum digest checked, the summary read into memory;
    /// every other page checked as reads touch it)
    pub(crate) fn open(
        fs: &dyn Fs,
        dir: &Path,
        meta: &SegmentMeta,
        shape: Shape,
        access: Access,
    ) -> Result<Self> {
        let pages = Pages::open(fs, &dir.join(file_name(meta.id)), meta.sealed, access)?;
        let len = pages.len();
        let sections = Sections::new(meta.id, shape, meta.records, |at| {
            let section = len.checked_sub(at).ok_or(SegmentError::Corrupt {
                segment: meta.id,
                reason: "sections past the file's end",
            })?;
            let read =
                |range: Range<usize>| -> Result<&[u8]> { Ok(pages.read(shifted(at, range)?)?) };
            FilterLayout::parse(read, section)
        })?;
        let summary = pages.read(sections.summary.clone())?.into();
        let filtered = usize::try_from(meta.filtered)
            .ok()
            .filter(|filtered| (1..=shape.key_len).contains(filtered))
            .ok_or(SegmentError::Corrupt {
                segment: meta.id,
                reason: "a filter prefix off the key",
            })?;
        Ok(Self {
            meta: *meta,
            filtered,
            pages,
            sections,
            summary,
            unlinked: OnceLock::new(),
        })
    }

    /// Its file deleted while still mapped: its bytes counted in `unlinked` until dropped
    pub(super) fn unlinked(&self, unlinked: &metrics::Gauge) {
        if self.unlinked.set(unlinked.clone()).is_ok() {
            unlinked.increment(self.meta.sealed.len as f64);
        }
    }

    /// Whole filter section read + checked up front: the only way to probe (no probe ever faults
    /// in or checks a cold filter page; merges never probe)
    pub(crate) fn probed(self) -> Result<Probed> {
        let (at, layout) = &self.sections.filter;
        let range = shifted(*at, 0..layout.len())?;
        self.pages.will_need(range.clone());
        let filter = self.pages.bytes(range)?;
        Ok(Probed { file: self, filter })
    }

    pub(crate) fn pages(&self) -> &Pages {
        &self.pages
    }

    pub(crate) fn records(&self) -> usize {
        self.sections.records
    }

    pub(crate) fn row(&self, slot: usize) -> Result<&[u8]> {
        Ok(self.pages.read(self.row_range(slot)?)?)
    }

    pub(crate) fn key(&self, slot: usize) -> Result<&[u8]> {
        let row = self.row(slot)?;
        row.get(..self.sections.shape.key_len)
            .ok_or(self.corrupt("a row shorter than its key"))
    }

    /// Row at `slot`: its key and its value or tombstone (`Corrupt`: a row that decodes as
    /// neither, page checksums passed, so a writer bug)
    pub(crate) fn content(&self, slot: usize) -> Result<(&[u8], Row<'_>)> {
        let shape = &self.sections.shape;
        decode_row(shape, self.row(slot)?).ok_or(self.corrupt("a bad row flag"))
    }

    /// `(key, value)` at `slot`, zero-copy slices of the mapping (a tombstone's = zeroed bytes)
    pub(crate) fn entry(&self, slot: usize) -> Result<(Bytes, Bytes)> {
        let shape = &self.sections.shape;
        let mut key = self.pages.bytes(self.row_range(slot)?)?;
        if key.len() < shape.key_len {
            return Err(self.corrupt("a row shorter than its key"));
        }
        let mut value = key.split_off(shape.key_len);
        value.truncate(shape.value_len);
        Ok((key, value))
    }

    fn row_range(&self, slot: usize) -> Result<Range<usize>> {
        let stride = self.sections.shape.stride;
        let overflow = || SegmentError::Overflow("row offset");
        let at = slot.checked_mul(stride).ok_or_else(overflow)?;
        Ok(at..at.checked_add(stride).ok_or_else(overflow)?)
    }

    fn corrupt(&self, reason: &'static str) -> SegmentError {
        SegmentError::Corrupt {
            segment: self.meta.id,
            reason,
        }
    }

    /// Key bytes this segment's filter covers (the map's scope when it was written)
    pub(crate) fn filtered(&self) -> usize {
        self.filtered
    }

    /// Bytes a [`seek`](Self::seek) for `needle` reads in `step`, for readahead ahead of it
    ///
    /// - [`Prefetch::Fences`] = fence group the in-memory summary picks (no read here)
    /// - [`Prefetch::Records`] = block of records those fences pick (reads the fences: ask once
    ///   their readahead is in flight)
    pub(crate) fn prefetch_range(&self, step: Prefetch, needle: &[u8]) -> Result<Range<usize>> {
        match step {
            Prefetch::Fences => {
                let group = self.sections.group_of(&self.summary, needle)?;
                match group.end.checked_sub(1).filter(|_| !group.is_empty()) {
                    None => Ok(0..0),
                    Some(last) => {
                        Ok(self.sections.fence(group.start)?.start..self.sections.fence(last)?.end)
                    }
                }
            }
            Prefetch::Records => {
                let block = self.block_of(needle)?;
                self.sections.rows(block)
            }
        }
    }

    /// `MADV_WILLNEED` over `range`: kernel queues its reads, nothing waits (advisory)
    pub(crate) fn will_need(&self, range: Range<usize>) {
        self.pages.will_need(range);
    }

    fn block_of(&self, needle: &[u8]) -> Result<usize> {
        let fence = |b| -> Result<&[u8]> { Ok(self.pages.read(self.sections.fence(b)?)?) };
        self.sections.block_of(&self.summary, fence, needle)
    }

    /// First slot whose key `>= needle` (record count when every key is below it)
    ///
    /// - in-memory summary → page of fences → fence → block; search stays inside it (≈ one page)
    pub(crate) fn seek(&self, needle: &[u8]) -> Result<usize> {
        let block = self.sections.slots(self.block_of(needle)?);
        let (mut low, mut high) = (block.start, block.end);
        while low < high {
            let mid = low.midpoint(high);
            match self.key(mid)? < needle {
                true => low = mid.saturating_add(1),
                false => high = mid,
            }
        }
        Ok(low)
    }
}

/// A segment whose filter section was checked whole ([`SegmentFile::probed`]): probes read it
/// with no page check
#[derive(Debug)]
pub(super) struct Probed {
    file: SegmentFile,
    filter: Bytes,
}

impl std::ops::Deref for Probed {
    type Target = SegmentFile;

    fn deref(&self) -> &SegmentFile {
        &self.file
    }
}

impl Probed {
    /// `false` = no key starting with `prefix` (the [`filtered`](SegmentFile::filtered) bytes)
    /// here
    pub(crate) fn may_contain(&self, prefix: &[u8]) -> Result<bool> {
        let read = |range: Range<usize>| -> Result<&[u8]> {
            self.filter.get(range).ok_or(SegmentError::Corrupt {
                segment: self.file.meta.id,
                reason: "a filter shard past its section",
            })
        };
        self.file.sections.filter.1.may_contain(read, prefix)
    }
}

/// `range` moved up by `at`
fn shifted(at: usize, range: Range<usize>) -> Result<Range<usize>> {
    let start = at.checked_add(range.start);
    let end = at.checked_add(range.end);
    match (start, end) {
        (Some(start), Some(end)) => Ok(start..end),
        _ => Err(SegmentError::Overflow("filter offset")),
    }
}
