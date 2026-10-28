//! Batches → segments, segments → merged segments
//!
//! - neither commits: returned [`SegmentMeta`] reaches readers once the owner's manifest lists it

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use rayon::slice::ParallelSliceMut as _;

use super::{
    file::SegmentFile,
    file_name,
    layout::{encode_row, Navigation, Row, Shape},
    slots::Slot,
    spill::Scratch,
    Breach, Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::{sums_path, FileKind, PagedFile, Sealed},
};

/// Records buffered to this before each append
const CHUNK: usize = 1 << 20;

/// Merge input's next record: `(key, source, slot)`, min-first
type Head<'a> = Reverse<(&'a [u8], usize, usize)>;

/// A merge's I/O, charged to its bandwidth slot a chunk at a time; stops once `cancel` is set
struct Paced<'a, 'b> {
    slot: &'a Slot<'b>,
    cancel: &'a AtomicBool,
    uncharged: usize,
}

impl Paced<'_, '_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn charge(&mut self, bytes: usize) {
        self.uncharged = self.uncharged.saturating_add(bytes);
        if self.uncharged >= CHUNK {
            self.settle();
        }
    }

    fn settle(&mut self) {
        self.slot.charge(std::mem::take(&mut self.uncharged));
    }
}

/// What a finished merge wrote: its segment (`None` = every row cancelled) + the value/tombstone
/// pairs it cancelled (output rows + 2 × cancelled = input rows) + the scratch files it could
/// not remove
#[derive(Debug)]
pub(super) struct Merged {
    pub(crate) output: Option<SegmentMeta>,
    pub(crate) cancelled: u64,
    pub(crate) scratch: Vec<Scratch>,
}

/// A sealed segment + the scratch files its write could not remove (the owner retries them)
#[derive(Debug)]
pub(super) struct Written {
    pub(super) meta: SegmentMeta,
    pub(super) scratch: Vec<Scratch>,
}

/// Writes one map's segments into its directory; ids allocated by the caller (one writer per id)
#[derive(Debug, Clone)]
pub(super) struct SegmentWriter {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    shape: Shape,
}

/// Segment mid-write: records streamed out in key order, its navigation built alongside
///
/// - `digest` = CRC-32 over every row pushed ([`SegmentWriter::verify`] recomputes it from disk)
struct SegmentOut {
    id: u32,
    filtered: u32,
    file: PagedFile,
    navigation: Navigation,
    chunk: Vec<u8>,
    digest: crc32fast::Hasher,
}

impl SegmentOut {
    fn push(&mut self, row: &[u8]) -> Result<()> {
        self.navigation.push(row)?;
        self.digest.update(row);
        self.chunk.extend_from_slice(row);
        if self.chunk.len() >= CHUNK {
            self.file.append(&self.chunk)?;
            self.chunk.clear();
        }
        Ok(())
    }

    /// Records, then navigation, sealed (fsynced) → meta a manifest would list + kept scratch
    /// files, rows' digest, the file; `None` = `pace` stopped it (cancelled)
    fn finish(
        mut self,
        pace: &mut dyn FnMut(u64) -> bool,
    ) -> Result<Option<(Written, u32, PagedFile)>> {
        self.file.append(&self.chunk)?;
        let records = self.navigation.records();
        let Some(scratch) = self.navigation.finish(&mut self.file, pace)? else {
            return Ok(None);
        };
        let meta = SegmentMeta {
            id: self.id,
            records,
            filtered: self.filtered,
            sealed: self.file.seal().map_err(SegmentError::Sync)?,
        };
        let written = Written { meta, scratch };
        Ok(Some((written, self.digest.finalize(), self.file)))
    }
}

impl SegmentWriter {
    pub(crate) fn open(fs: Arc<dyn Fs>, dir: &Path, shape: Shape) -> Self {
        Self {
            fs,
            dir: dir.to_path_buf(),
            shape,
        }
    }

    /// Sorts `rows` (`(key, value)`, `None` = tombstone) and writes them as segment `id`, sealed;
    /// `None` for none
    ///
    /// - every row checked against the shape first (widths, tombstones only with deletes)
    /// - uncommitted until listed in a manifest, unlinked until [`sync_dir`](Self::sync_dir)
    /// - a key twice = `Contract` (the buffer holds one row per key: a cancelled pair's value is
    ///   one segment's row, its tombstone a newer segment's)
    /// - unique keys → unstable sort exact; parallel on the CPU pool
    pub(crate) fn write(
        &self,
        id: u32,
        mut rows: Vec<(&[u8], Option<&[u8]>)>,
    ) -> Result<Option<Written>> {
        if rows.is_empty() {
            return Ok(None);
        }
        for (key, value) in &rows {
            let fits = key.len() == self.shape.key_len
                && match value {
                    Some(value) => value.len() == self.shape.value_len,
                    None => self.shape.deletes,
                };
            if !fits {
                return Err(SegmentError::Internal("a batch row off its map's shape"));
            }
        }
        rows.par_sort_unstable_by_key(|(key, _)| *key);

        let mut out = self.create(id, rows.len() as u64)?;
        let mut row = Vec::with_capacity(self.shape.stride);
        for (key, value) in rows {
            row.clear();
            encode_row(
                &self.shape,
                key,
                value.map_or(Row::Tombstone, Row::Value),
                &mut row,
            )?;
            out.push(&row)?;
        }
        self.sealed(out, None)
    }

    /// K-way merge of `inputs` into segment `id`, sealed; `None` once `cancel` is set (partial file
    /// unlisted → removed at open)
    ///
    /// - a key's value + its tombstone, both inputs → neither written (insert-once / remove-once:
    ///   no other row of that key exists anywhere); a lone tombstone → written (its value is in a
    ///   segment outside the merge); any other duplicate → [`SegmentError::Contract`]
    /// - no row left → no file, `output: None` (the inputs retire with nothing in their place)
    /// - one input = that segment rewritten under the map's current shape (its filter rebuilt
    ///   for a changed scope)
    /// - inputs read through their page checksums (corrupt input = an error, never propagates)
    /// - caller then commits a manifest listing it in their place, then removes the inputs
    /// - streams: memory = a few 1 MiB buffers + summary + filter's shard table, whatever the
    ///   segments' size (fences + fingerprints spill to scratch files, `spill.rs`)
    pub(crate) fn merge(
        &self,
        id: u32,
        inputs: &[SegmentMeta],
        cancel: &AtomicBool,
        slot: &Slot<'_>,
    ) -> Result<Option<Merged>> {
        if inputs.is_empty() {
            return Err(SegmentError::Internal("a merge of no segments"));
        }
        let sources = inputs
            .iter()
            .map(|meta| {
                SegmentFile::open(
                    self.fs.as_ref(),
                    &self.dir,
                    meta,
                    self.shape,
                    Access::Sequential,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let source = |at: usize| {
            sources
                .get(at)
                .ok_or(SegmentError::Internal("merge source past its inputs"))
        };
        let total = inputs
            .iter()
            .fold(0u64, |sum, meta| sum.saturating_add(meta.records));
        let repeated = |group: &[(usize, usize)]| SegmentError::Contract {
            segments: group
                .iter()
                .filter_map(|(at, _)| inputs.get(*at))
                .map(|meta| meta.id)
                .collect(),
            breach: Breach::Repeated { held: group.len() },
        };

        let head = |at: usize, slot: usize| -> Result<Option<Head<'_>>> {
            let file = source(at)?;
            match slot < file.records() {
                true => Ok(Some(Reverse((file.key(slot)?, at, slot)))),
                false => Ok(None),
            }
        };
        let mut heads: BinaryHeap<Head<'_>> = BinaryHeap::with_capacity(sources.len());
        for at in 0..sources.len() {
            heads.extend(head(at, 0)?);
        }

        let mut out: Option<SegmentOut> = None;
        let mut cancelled: u64 = 0;
        // read + written per row
        let mut paced = Paced {
            slot,
            cancel,
            uncharged: 0,
        };
        while let Some(Reverse((key, at, row_at))) = heads.pop() {
            if paced.cancelled() {
                return Ok(None);
            }
            heads.extend(head(at, row_at.saturating_add(1))?);
            let mut group = vec![(at, row_at)];
            while let Some(&Reverse((next, at, row_at))) = heads.peek() {
                if next != key {
                    break;
                }
                heads.pop();
                heads.extend(head(at, row_at.saturating_add(1))?);
                group.push((at, row_at));
            }
            paced.charge(group.len().saturating_mul(self.shape.stride));
            let row = match group.as_slice() {
                [(at, row_at)] => Some(source(*at)?.row(*row_at)?),
                [(a, a_at), (b, b_at)] if self.shape.deletes => {
                    let contents = [source(*a)?.content(*a_at)?.1, source(*b)?.content(*b_at)?.1];
                    let pair = matches!(contents, [Row::Value(_), Row::Tombstone])
                        || matches!(contents, [Row::Tombstone, Row::Value(_)]);
                    if !pair {
                        return Err(repeated(&group));
                    }
                    cancelled = cancelled.saturating_add(1);
                    None
                }
                many => return Err(repeated(many)),
            };
            if let Some(row) = row {
                let writing = match &mut out {
                    Some(out) => out,
                    None => out.insert(self.create(id, total)?),
                };
                writing.push(row)?;
                paced.charge(row.len());
            }
        }
        paced.settle();
        let (output, scratch) = match out {
            Some(out) => match self.sealed(out, Some(&mut paced))? {
                Some(written) => (Some(written.meta), written.scratch),
                None => return Ok(None),
            },
            None => (None, Vec::new()),
        };
        let written = output.map_or(0, |output| output.records);
        let read = sources
            .iter()
            .fold(0u64, |sum, file| sum.saturating_add(file.records() as u64));
        if written.saturating_add(cancelled.saturating_mul(2)) != read {
            return Err(SegmentError::SelfCheck {
                segment: id,
                reason: format!("{written} rows + {cancelled} cancelled pairs from {read} inputs"),
            });
        }
        Ok(Some(Merged {
            output,
            cancelled,
            scratch,
        }))
    }

    /// `out` finished, read back through its page checksums (O(rows); RocksDB
    /// `paranoid_file_checks`), out of page cache unless its map caches writes (after the
    /// read-back: it faults every page back in), then made read-only (never written again)
    ///
    /// - a merge's read-back is `paced` (`None` once cancelled); a batch's never is
    fn sealed(
        &self,
        out: SegmentOut,
        mut paced: Option<&mut Paced<'_, '_>>,
    ) -> Result<Option<Written>> {
        let mut pace = |bytes: u64| match paced.as_deref_mut() {
            Some(paced) => {
                paced.charge(usize::try_from(bytes).unwrap_or(usize::MAX));
                !paced.cancelled()
            }
            None => true,
        };
        let Some((written, digest, file)) = out.finish(&mut pace)? else {
            return Ok(None);
        };
        let id = written.meta.id;
        let verified = self
            .verify(&written.meta, digest, paced)
            .map_err(|source| SegmentError::ReadBack {
                segment: id,
                source: Box::new(source),
            })?;
        if !verified {
            return Ok(None);
        }
        if !self.shape.cache_writes {
            file.drop_cache();
        }
        drop(file);
        let path = self.dir.join(file_name(id));
        self.fs.set_read_only(&path)?;
        self.fs.set_read_only(&sums_path(&path))?;
        Ok(Some(written))
    }

    /// Sealed segment re-read from disk: its layout opening at the record count, strictly
    /// ascending keys, no filter false negative, every row decoding, same row digest as written
    /// (TigerBeetle pair assertion: checked writing + reading); `false` = cancelled
    fn verify(
        &self,
        meta: &SegmentMeta,
        written: u32,
        mut paced: Option<&mut Paced<'_, '_>>,
    ) -> Result<bool> {
        let file = SegmentFile::open(
            self.fs.as_ref(),
            &self.dir,
            meta,
            self.shape,
            Access::Sequential,
        )?
        .probed()?;
        let failed = |reason: String| SegmentError::SelfCheck {
            segment: meta.id,
            reason,
        };
        // a record count off the file's length fails its open (sections laid out by the count)
        // index sections read at open (summary, whole filter); the rows as the loop reads them
        if let Some(paced) = paced.as_deref_mut() {
            let rows = file.records().saturating_mul(self.shape.stride) as u64;
            paced.charge(
                usize::try_from(meta.sealed.len.saturating_sub(rows)).unwrap_or(usize::MAX),
            );
        }
        let mut digest = crc32fast::Hasher::new();
        let mut previous: Option<&[u8]> = None;
        for slot in 0..file.records() {
            if let Some(paced) = paced.as_deref_mut() {
                if paced.cancelled() {
                    return Ok(false);
                }
                paced.charge(self.shape.stride);
            }
            let key = file.key(slot)?;
            if previous.is_some_and(|previous| previous >= key) {
                return Err(failed(format!("slot {slot}: keys not ascending")));
            }
            let filtered = key.get(..file.filtered()).unwrap_or(key);
            if !file.may_contain(filtered)? {
                return Err(failed(format!("slot {slot}: the filter misses its key")));
            }
            file.content(slot)?;
            digest.update(file.row(slot)?);
            previous = Some(key);
        }
        if digest.finalize() != written {
            return Err(failed("rows differ from those written".to_owned()));
        }
        if let Some(paced) = paced {
            paced.settle();
        }
        Ok(true)
    }

    /// Segments written since the last call → durably linked
    pub(crate) fn sync_dir(&self) -> Result<()> {
        self.fs.sync_dir(&self.dir).map_err(SegmentError::Sync)
    }

    fn create(&self, id: u32, records: u64) -> Result<SegmentOut> {
        let path = self.dir.join(file_name(id));
        let file = PagedFile::open(self.fs.as_ref(), &path, Sealed::EMPTY, FileKind::Segment)?;
        Ok(SegmentOut {
            id,
            filtered: u32::try_from(self.shape.filtered)
                .map_err(|_| SegmentError::Overflow("filter prefix"))?,
            file,
            navigation: Navigation::new(self.shape, records, &self.fs, &self.dir, id)?,
            chunk: Vec::with_capacity(CHUNK.saturating_add(self.shape.stride)),
            digest: crc32fast::Hasher::new(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests;
