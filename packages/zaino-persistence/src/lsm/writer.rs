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
    filter::FilterError,
    layout::{Navigation, Shape},
    slots::Slot,
    Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Fs},
    pages::{sums_path, FileKind, PagedFile, Sealed},
};

/// Records buffered to this before each append
const CHUNK: usize = 1 << 20;

/// O(rows) read-back of every sealed segment (RocksDB `paranoid_file_checks`, TigerBeetle
/// `constants.verify`): tests and index test suites only
const VERIFY: bool = cfg!(any(test, feature = "testing"));

/// Merge input's next record: `(key, source, slot)`, min-first
type Head<'a> = Reverse<(&'a [u8], usize, usize)>;

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
    records: u64,
    navigation: Navigation,
    chunk: Vec<u8>,
    digest: crc32fast::Hasher,
}

impl SegmentOut {
    fn push(&mut self, row: &[u8]) -> Result<()> {
        let id = self.id;
        self.navigation.push(row).map_err(|error| navigation_error(id, error))?;
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
        let id = self.id;
        self.navigation
            .finish(self.records, &mut self.file)
            .map_err(|error| navigation_error(id, error))?;
        let meta = SegmentMeta { id, records: self.records, sealed: self.file.seal()? };
        Ok((meta, self.digest.finalize(), self.file))
    }
}

fn navigation_error(segment: u32, error: FilterError) -> SegmentError {
    match error {
        FilterError::Build(reason) => SegmentError::Filter { segment, reason },
        FilterError::Io(error) => SegmentError::Io(error),
    }
}

impl SegmentWriter {
    pub(crate) fn open(fs: Arc<dyn Fs>, dir: &Path, shape: Shape) -> Self {
        Self { fs, dir: dir.to_path_buf(), shape }
    }

    /// Sorts `rows` (`(key, value)`) and writes them as segment `id`, sealed; `None` for none
    ///
    /// - uncommitted until listed in a manifest, unlinked until [`sync_dir`](Self::sync_dir)
    /// - duplicate keys panic (batch projects distinct rows by construction)
    /// - unique keys → unstable sort exact; parallel on the CPU pool
    pub(crate) fn write(
        &self,
        id: u32,
        mut rows: Vec<(&[u8], &[u8])>,
    ) -> Result<Option<SegmentMeta>> {
        if rows.is_empty() {
            return Ok(None);
        }
        rows.par_sort_unstable_by_key(|(key, _)| *key);

        let mut out = self.create(id, rows.len() as u64)?;
        let mut row = Vec::with_capacity(self.shape.stride);
        for (key, value) in rows {
            row.clear();
            row.extend_from_slice(key);
            row.extend_from_slice(value);
            out.push(&row)?;
        }
        self.sealed(out).map(Some)
    }

    /// K-way merge of `inputs` into segment `id`, sealed (record count = inputs' sum); `None` once
    /// `cancel` is set (partial file unlisted → removed at open)
    ///
    /// - inputs read through their page checksums (corrupt input dies, never propagates)
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
    ) -> Result<Option<SegmentMeta>> {
        assert!(inputs.len() >= 2, "merge of {} segments", inputs.len());
        let sources = inputs
            .iter()
            .map(|meta| {
                SegmentFile::open(self.fs.as_ref(), &self.dir, meta, self.shape, Access::Sequential)
            })
            .collect::<Result<Vec<_>>>()?;
        let total: u64 = inputs.iter().map(|meta| meta.records).sum();

        let head = |source: usize, slot: usize| -> Option<Head<'_>> {
            let file = &sources[source];
            (slot < file.records()).then(|| Reverse((file.key(slot), source, slot)))
        };
        let mut heads: BinaryHeap<Head<'_>> =
            (0..sources.len()).filter_map(|source| head(source, 0)).collect();

        let mut out = self.create(id, total)?;
        // read + written per row, charged to the engine's merge bandwidth a chunk at a time
        let mut uncharged = 0;
        while let Some(Reverse((_, source, at))) = heads.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let row = sources[source].row(at);
            out.push(row)?;
            uncharged += 2 * row.len();
            if uncharged >= CHUNK {
                slot.charge(std::mem::take(&mut uncharged));
            }
            heads.extend(head(source, at + 1));
        }
        slot.charge(uncharged);
        // `finish` asserts every input record was emitted once
        self.sealed(out).map(Some)
    }

    /// `out` finished, then (under [`VERIFY`]) read back through its page checksums, then out of
    /// page cache unless its table [`cache_writes`](crate::MapTable::cache_writes) (after the
    /// read-back: it faults every page back in)
    fn sealed(&self, out: SegmentOut) -> Result<SegmentMeta> {
        let (meta, digest, file) = out.finish()?;
        if VERIFY {
            self.verify(&meta, digest)?;
        }
        if !self.shape.cache_writes {
            file.drop_cache()?;
        }
        Ok(meta)
    }

    /// Sealed segment re-read from disk: record count, strictly ascending keys, no filter false
    /// negative, same row digest as written (TigerBeetle pair assertion: checked writing + reading)
    fn verify(&self, meta: &SegmentMeta, written: u32) -> Result<()> {
        let file =
            SegmentFile::open(self.fs.as_ref(), &self.dir, meta, self.shape, Access::Sequential)?;
        assert_eq!(file.records() as u64, meta.records, "segment {}: record count", meta.id);
        file.warm_filter();
        let mut digest = crc32fast::Hasher::new();
        for slot in 0..file.records() {
            let key = file.key(slot);
            if slot > 0 {
                assert!(file.key(slot - 1) < key, "segment {} slot {slot}: keys ascend", meta.id);
            }
            let filtered = &key[..file.filtered()];
            assert!(file.may_contain(filtered), "segment {} slot {slot}: filter finds it", meta.id);
            digest.update(file.row(slot));
        }
        assert_eq!(digest.finalize(), written, "segment {}: rows read back as written", meta.id);
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
            records,
            navigation: Navigation::new(self.shape, records, &self.fs, &self.dir, id),
            chunk: Vec::with_capacity(CHUNK + self.shape.stride),
            digest: crc32fast::Hasher::new(),
        })
    }
}
