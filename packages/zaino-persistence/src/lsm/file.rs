//! One listed segment, mapped: what readers and merges hold

use std::{ops::Range, path::Path};

use super::{
    file_name,
    filter::FilterLayout,
    layout::{Sections, Shape},
    Result, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::Pages,
};

/// The two reads a seek makes, in order: its fence group, then its block of records
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Prefetch {
    Fences,
    Records,
}

/// - `summary` = the summary section, copied into memory at open (one key per page of fences)
#[derive(Debug)]
pub(crate) struct SegmentFile {
    pub(crate) meta: SegmentMeta,
    pages: Pages,
    sections: Sections,
    summary: Box<[u8]>,
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
        let sections = Sections::new(shape, meta.records, len, |at| {
            FilterLayout::parse(|range| pages.read(at + range.start..at + range.end), len - at)
        });
        let summary = pages.read(sections.summary.clone()).into();
        Ok(Self { meta: *meta, pages, sections, summary })
    }

    /// Reads and checks the whole filter section up front, so no probe ever faults a cold filter
    /// page in or checks one (a reader's copy; merges never probe)
    pub(crate) fn warm_filter(&self) {
        if let Some((at, layout)) = &self.sections.filter {
            let range = *at..*at + layout.len();
            self.pages.will_need(range.clone());
            self.pages.read(range);
        }
    }

    pub(crate) fn records(&self) -> usize {
        self.sections.records
    }

    pub(crate) fn row(&self, slot: usize) -> &[u8] {
        let stride = self.sections.shape.stride;
        self.pages.read(slot * stride..(slot + 1) * stride)
    }

    pub(crate) fn key(&self, slot: usize) -> &[u8] {
        &self.row(slot)[..self.sections.shape.key_len]
    }

    /// Key bytes the filter covers (0 = no filter)
    pub(crate) fn filtered(&self) -> usize {
        self.sections.shape.filtered
    }

    /// Bytes a [`seek`](Self::seek) for `needle` reads in `step`, for readahead ahead of it
    ///
    /// - [`Prefetch::Fences`] = the fence group the in-memory summary picks (no read here)
    /// - [`Prefetch::Records`] = the block of records those fences pick (reads the fences: ask
    ///   for it once their readahead is in flight)
    pub(crate) fn prefetch_range(&self, step: Prefetch, needle: &[u8]) -> Range<usize> {
        match step {
            Prefetch::Fences => {
                let group = self.sections.group_of(&self.summary, needle);
                match group.is_empty() {
                    true => 0..0,
                    false => {
                        self.sections.fence(group.start).start
                            ..self.sections.fence(group.end - 1).end
                    }
                }
            }
            Prefetch::Records => {
                let fence = |b| self.pages.read(self.sections.fence(b));
                self.sections.rows(self.sections.block_of(&self.summary, fence, needle))
            }
        }
    }

    /// `MADV_WILLNEED` over `range`: the kernel queues its reads, nothing waits (advisory)
    pub(crate) fn will_need(&self, range: Range<usize>) {
        self.pages.will_need(range);
    }

    /// First slot whose key is `>= needle` (the record count when every key is below it)
    ///
    /// - the in-memory summary picks a page of fences, a fence picks the block, and the search
    ///   stays inside that block (≈ one page)
    pub(crate) fn seek(&self, needle: &[u8]) -> usize {
        let rows = self.sections.shape.block_rows;
        let fence = |b| self.pages.read(self.sections.fence(b));
        let b = self.sections.block_of(&self.summary, fence, needle);
        let (mut low, mut high) = (b * rows, ((b + 1) * rows).min(self.records()));
        while low < high {
            let mid = low + (high - low) / 2;
            match self.key(mid) < needle {
                true => low = mid + 1,
                false => high = mid,
            }
        }
        low
    }

    /// `false` = no key starting with `prefix` (the [`filtered`](Self::filtered) bytes) is in this
    /// segment; always `true` for an unfiltered set
    ///
    /// - reads only pages [`warm_filter`](Self::warm_filter) already checked
    pub(crate) fn may_contain(&self, prefix: &[u8]) -> bool {
        let Some((at, layout)) = &self.sections.filter else {
            return true;
        };
        layout.may_contain(
            |range| self.pages.read_unchecked(at + range.start..at + range.end),
            prefix,
        )
    }
}
