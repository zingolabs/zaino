//! One listed segment, mapped: what readers and merges hold

use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

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

/// - `shard_checked[s]` = shard `s`'s filter pages CRC-checked (a probe reads a whole shard, up to
///   ~290 pages: one flag instead of a bitmap walk per probe)
#[derive(Debug)]
pub(crate) struct SegmentFile {
    pub(crate) meta: SegmentMeta,
    pages: Pages,
    sections: Sections,
    shard_checked: Box<[AtomicBool]>,
}

impl SegmentFile {
    /// Maps `meta`'s segment (lengths checked; pages checked as reads touch them)
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
        let shards = sections.filter.as_ref().map_or(0, |(_, layout)| layout.shards());
        let shard_checked = (0..shards).map(|_| AtomicBool::new(false)).collect();
        Ok(Self { meta: *meta, pages, sections, shard_checked })
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

    /// First slot whose key is `>= needle` (the record count when every key is below it)
    ///
    /// - fences name the one block; the search stays inside it (≈ one page)
    pub(crate) fn seek(&self, needle: &[u8]) -> usize {
        let rows = self.sections.shape.block_rows;
        let b = self.sections.block_of(|b| self.pages.read(self.sections.fence(b)), needle);
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

    /// `false` = certainly absent (always `true` for an unprobed set)
    ///
    /// - first probe of a shard = checked read of all of it (Release); later = unchecked (Acquire)
    pub(crate) fn may_contain(&self, key: &[u8]) -> bool {
        let Some((at, layout)) = &self.sections.filter else {
            return true;
        };
        let shard = layout.shard_of(key);
        if !self.shard_checked[shard].load(Ordering::Acquire) {
            for range in layout.shard_ranges(shard) {
                self.pages.read(at + range.start..at + range.end);
            }
            self.shard_checked[shard].store(true, Ordering::Release);
        }
        layout.may_contain(|range| self.pages.read_unchecked(at + range.start..at + range.end), key)
    }
}
