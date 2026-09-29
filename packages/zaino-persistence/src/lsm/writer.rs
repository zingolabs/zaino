//! Batches → segments, segments → merged segments
//!
//! - neither commits: a returned [`SegmentMeta`] reaches readers once the owner's manifest lists it

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
    layout::{Navigation, Shape},
    record::Record,
    Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::{sums_path, PagedFile, Sealed},
};

/// Records buffered to this before each append
const CHUNK: usize = 1 << 20;

/// O(rows) read-back of every sealed segment (RocksDB `paranoid_file_checks`, TigerBeetle
/// `constants.verify`): tests and index test suites only
const VERIFY: bool = cfg!(any(test, feature = "testing"));

/// A merge input's next record: `(key, source, slot)`, min-first
type Head<'a> = Reverse<(&'a [u8], usize, usize)>;

/// Writes segments into one directory; ids allocated by the caller (one writer per id)
#[derive(Debug, Clone)]
pub(crate) struct SegmentWriter {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
}

/// One segment being written: records streamed out in key order, its navigation built alongside
///
/// - `digest` = CRC-32 over every row pushed ([`SegmentWriter::verify`] recomputes it from disk)
struct SegmentOut {
    id: u32,
    file: PagedFile,
    records: u64,
    navigation: Navigation,
    chunk: Vec<u8>,
    digest: crc32fast::Hasher,
}

impl SegmentOut {
    fn push(&mut self, row: &[u8]) -> Result<()> {
        let id = self.id;
        self.navigation.push(row).map_err(|reason| SegmentError::Filter { segment: id, reason })?;
        self.digest.update(row);
        self.chunk.extend_from_slice(row);
        if self.chunk.len() >= CHUNK {
            self.file.append(&self.chunk)?;
            self.chunk.clear();
        }
        Ok(())
    }

    /// Records, then navigation, sealed (fsynced): the meta a manifest would list + the rows' digest
    fn finish(mut self) -> Result<(SegmentMeta, u32)> {
        self.file.append(&self.chunk)?;
        let navigation = self
            .navigation
            .finish(self.records)
            .map_err(|reason| SegmentError::Filter { segment: self.id, reason })?;
        self.file.append(&navigation)?;
        let meta = SegmentMeta { id: self.id, records: self.records, sealed: self.file.seal()? };
        Ok((meta, self.digest.finalize()))
    }
}

impl SegmentWriter {
    pub(crate) fn open(fs: Arc<dyn Fs>, dir: &Path) -> Self {
        Self { fs, dir: dir.to_path_buf() }
    }

    /// Sorts `records` and writes them as segment `id`, sealed; `None` for an empty batch
    ///
    /// - uncommitted until listed in a manifest, and unlinked until [`sync_dir`](Self::sync_dir)
    /// - duplicate keys panic (a batch projects distinct rows by construction)
    /// - unique keys → an unstable sort is exact; parallel on the CPU pool
    pub(crate) fn write<R: Record>(
        &self,
        id: u32,
        mut records: Vec<R>,
    ) -> Result<Option<SegmentMeta>> {
        if records.is_empty() {
            return Ok(None);
        }
        records.par_sort_unstable_by_key(|record| record.key());

        let mut out = self.create::<R>(id, records.len() as u64)?;
        let mut row = Vec::with_capacity(R::STRIDE);
        for record in &records {
            row.clear();
            record.encode(&mut row);
            out.push(&row)?;
        }
        self.sealed::<R>(out).map(Some)
    }

    /// K-way merge of `inputs` into segment `id`, sealed (record count = inputs' sum); `None` once
    /// `cancel` is set (partial file unlisted → removed at open)
    ///
    /// - inputs read through their page checksums (a corrupt input dies, never propagates)
    /// - caller then commits a manifest listing it in their place, then [`remove`](Self::remove)s
    ///   the inputs
    /// - streams: memory = one chunk + the navigation being built, whatever the segments' size
    pub(crate) fn merge<R: Record>(
        &self,
        id: u32,
        inputs: &[SegmentMeta],
        cancel: &AtomicBool,
    ) -> Result<Option<SegmentMeta>> {
        assert!(inputs.len() >= 2, "merge of {} segments", inputs.len());
        let shape = Shape::of::<R>();
        let sources = inputs
            .iter()
            .map(|meta| {
                SegmentFile::open(self.fs.as_ref(), &self.dir, meta, shape, Access::Sequential)
            })
            .collect::<Result<Vec<_>>>()?;
        let total: u64 = inputs.iter().map(|meta| meta.records).sum();

        let head = |source: usize, slot: usize| -> Option<Head<'_>> {
            let file = &sources[source];
            (slot < file.records()).then(|| Reverse((file.key(slot), source, slot)))
        };
        let mut heads: BinaryHeap<Head<'_>> =
            (0..sources.len()).filter_map(|source| head(source, 0)).collect();

        let mut out = self.create::<R>(id, total)?;
        while let Some(Reverse((_, source, slot))) = heads.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            out.push(sources[source].row(slot))?;
            heads.extend(head(source, slot + 1));
        }
        // `finish` asserts every input record was emitted once
        self.sealed::<R>(out).map(Some)
    }

    /// `out` finished, then (under [`VERIFY`]) read back through its page checksums
    fn sealed<R: Record>(&self, out: SegmentOut) -> Result<SegmentMeta> {
        let (meta, digest) = out.finish()?;
        if VERIFY {
            self.verify::<R>(&meta, digest)?;
        }
        Ok(meta)
    }

    /// Sealed segment re-read from disk: record count, strictly ascending keys, no filter false
    /// negative, same row digest as written (TigerBeetle pair assertion: checked writing + reading)
    fn verify<R: Record>(&self, meta: &SegmentMeta, written: u32) -> Result<()> {
        let file = SegmentFile::open(
            self.fs.as_ref(),
            &self.dir,
            meta,
            Shape::of::<R>(),
            Access::Sequential,
        )?;
        assert_eq!(file.records() as u64, meta.records, "segment {}: record count", meta.id);
        let mut digest = crc32fast::Hasher::new();
        for slot in 0..file.records() {
            let key = file.key(slot);
            if slot > 0 {
                assert!(file.key(slot - 1) < key, "segment {} slot {slot}: keys ascend", meta.id);
            }
            assert!(file.may_contain(key), "segment {} slot {slot}: filter finds its key", meta.id);
            digest.update(file.row(slot));
        }
        assert_eq!(digest.finalize(), written, "segment {}: rows read back as written", meta.id);
        Ok(())
    }

    /// Makes segments written since the last call durably linked
    pub(crate) fn sync_dir(&self) -> Result<()> {
        Ok(self.fs.sync_dir(&self.dir)?)
    }

    /// Unlinks segments (and their checksums) no committed manifest lists any more
    ///
    /// - no dir sync: an unlink lost to a crash leaves an unlisted segment, removed at open
    pub(crate) fn remove(&self, segments: &[SegmentMeta]) -> Result<()> {
        for segment in segments {
            let path = self.dir.join(file_name(segment.id));
            self.fs.remove(&sums_path(&path))?;
            self.fs.remove(&path)?;
        }
        Ok(())
    }

    fn create<R: Record>(&self, id: u32, records: u64) -> Result<SegmentOut> {
        let file = PagedFile::open(self.fs.as_ref(), &self.dir.join(file_name(id)), Sealed::EMPTY)?;
        Ok(SegmentOut {
            id,
            file,
            records,
            navigation: Navigation::new(Shape::of::<R>(), records),
            chunk: Vec::with_capacity(CHUNK + R::STRIDE),
            digest: crc32fast::Hasher::new(),
        })
    }
}
