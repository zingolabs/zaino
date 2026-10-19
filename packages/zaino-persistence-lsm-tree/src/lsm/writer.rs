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
    Breach, Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::{sums_path, FileKind, PagedFile, Sealed},
};

/// Records buffered to this before each append
const CHUNK: usize = 1 << 20;

/// O(rows) read-back of every sealed segment (RocksDB `paranoid_file_checks`, TigerBeetle
/// `constants.verify`): tests and test suites only
const VERIFY: bool = cfg!(any(test, feature = "testing"));

/// Merge input's next record: `(key, source, slot)`, min-first
type Head<'a> = Reverse<(&'a [u8], usize, usize)>;

/// What a finished merge wrote: its segment (`None` = every row cancelled) + the value/tombstone
/// pairs it cancelled (output rows + 2 × cancelled = input rows)
#[derive(Debug, Clone, Copy)]
pub(crate) struct Merged {
    pub(crate) output: Option<SegmentMeta>,
    pub(crate) cancelled: u64,
}

/// Writes one map's segments into its directory; ids allocated by the caller (one writer per id)
#[derive(Debug, Clone)]
pub(crate) struct SegmentWriter {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    shape: Shape,
}

/// Segment mid-write: records streamed out in key order, its navigation built alongside
///
/// - `digest` = CRC-32 over every row pushed ([`SegmentWriter::verify`] recomputes it from disk)
struct SegmentOut {
    id: u32,
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

    /// Records, then navigation, sealed (fsynced) → meta a manifest would list + rows' digest +
    /// the file
    fn finish(mut self) -> Result<(SegmentMeta, u32, PagedFile)> {
        self.file.append(&self.chunk)?;
        let records = self.navigation.records();
        self.navigation.finish(&mut self.file)?;
        let meta = SegmentMeta {
            id: self.id,
            records,
            sealed: self.file.seal()?,
        };
        Ok((meta, self.digest.finalize(), self.file))
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
    /// - a key twice = `Contract` (the buffer holds one row per key: a cancelled pair writes
    ///   neither)
    /// - unique keys → unstable sort exact; parallel on the CPU pool
    pub(crate) fn write(
        &self,
        id: u32,
        mut rows: Vec<(&[u8], Option<&[u8]>)>,
    ) -> Result<Option<SegmentMeta>> {
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
        self.sealed(out).map(Some)
    }

    /// K-way merge of `inputs` into segment `id`, sealed; `None` once `cancel` is set (partial file
    /// unlisted → removed at open)
    ///
    /// - a key's value + its tombstone, both inputs → neither written (insert-once / remove-once:
    ///   no other row of that key exists anywhere); a lone tombstone → written (its value is in a
    ///   segment outside the merge); any other duplicate → [`SegmentError::Contract`]
    /// - no row left → no file, `output: None` (the inputs retire with nothing in their place)
    /// - inputs read through their page checksums (corrupt input = an error, never propagates)
    /// - caller then commits a manifest listing it in their place, then
    ///   [`remove`](Self::remove)s the inputs
    /// - streams: memory = a few 1 MiB buffers + summary + filter's shard table, whatever the
    ///   segments' size (fences + fingerprints spill to scratch files, `spill.rs`)
    pub(crate) fn merge(
        &self,
        id: u32,
        inputs: &[SegmentMeta],
        cancel: &AtomicBool,
        slot: &Slot<'_>,
    ) -> Result<Option<Merged>> {
        if inputs.len() < 2 {
            return Err(SegmentError::Internal("a merge of fewer than two segments"));
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
        // read + written per row, charged to the engine's merge bandwidth a chunk at a time
        let mut uncharged: usize = 0;
        while let Some(Reverse((key, at, row_at))) = heads.pop() {
            if cancel.load(Ordering::Relaxed) {
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
            uncharged = uncharged.saturating_add(group.len().saturating_mul(self.shape.stride));
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
                uncharged = uncharged.saturating_add(row.len());
            }
            if uncharged >= CHUNK {
                slot.charge(std::mem::take(&mut uncharged));
            }
        }
        slot.charge(uncharged);
        let output = out.map(|out| self.sealed(out)).transpose()?;
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
        Ok(Some(Merged { output, cancelled }))
    }

    /// `out` finished, then (under [`VERIFY`]) read back through its page checksums, then out of
    /// page cache unless its map caches writes (after the read-back: it faults every page back
    /// in), then made read-only (data + checksums: never written again)
    fn sealed(&self, out: SegmentOut) -> Result<SegmentMeta> {
        let (meta, digest, file) = out.finish()?;
        if VERIFY {
            self.verify(&meta, digest)?;
        }
        if !self.shape.cache_writes {
            file.drop_cache()?;
        }
        drop(file);
        let path = self.dir.join(file_name(meta.id));
        self.fs.set_read_only(&path)?;
        self.fs.set_read_only(&sums_path(&path))?;
        Ok(meta)
    }

    /// Sealed segment re-read from disk: record count, strictly ascending keys, no filter false
    /// negative, every row decoding, same row digest as written (TigerBeetle pair assertion:
    /// checked writing + reading)
    fn verify(&self, meta: &SegmentMeta, written: u32) -> Result<()> {
        let file = SegmentFile::open(
            self.fs.as_ref(),
            &self.dir,
            meta,
            self.shape,
            Access::Sequential,
        )?;
        let failed = |reason: String| SegmentError::SelfCheck {
            segment: meta.id,
            reason,
        };
        if file.records() as u64 != meta.records {
            let found = file.records();
            return Err(failed(format!("{found} records, {} listed", meta.records)));
        }
        file.warm_filter()?;
        let mut digest = crc32fast::Hasher::new();
        let mut previous: Option<&[u8]> = None;
        for slot in 0..file.records() {
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
        Ok(())
    }

    /// Segments written since the last call → durably linked
    pub(crate) fn sync_dir(&self) -> Result<()> {
        Ok(self.fs.sync_dir(&self.dir)?)
    }

    /// Unlinks segments (and their checksums) no committed manifest lists any more
    ///
    /// - no dir sync: unlink lost to a crash = unlisted segment, removed at open
    pub(crate) fn remove(&self, segments: &[SegmentMeta]) -> Result<()> {
        for segment in segments {
            let path = self.dir.join(file_name(segment.id));
            self.fs.remove(&sums_path(&path))?;
            self.fs.remove(&path)?;
        }
        Ok(())
    }

    fn create(&self, id: u32, records: u64) -> Result<SegmentOut> {
        let path = self.dir.join(file_name(id));
        let file = PagedFile::open(self.fs.as_ref(), &path, Sealed::EMPTY, FileKind::Segment)?;
        Ok(SegmentOut {
            id,
            file,
            navigation: Navigation::new(self.shape, records, &self.fs, &self.dir, id)?,
            chunk: Vec::with_capacity(CHUNK.saturating_add(self.shape.stride)),
            digest: crc32fast::Hasher::new(),
        })
    }
}
