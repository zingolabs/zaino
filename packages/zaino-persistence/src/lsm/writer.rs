//! Batches → segments, segments → merged segments
//!
//! - neither commits: returned [`SegmentMeta`] reaches readers once the owner's manifest lists it

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::atomic::{AtomicBool, Ordering},
};

use rayon::slice::ParallelSliceMut as _;

use super::{
    dir::SegmentDir,
    file::SegmentFile,
    filter::FilterError,
    layout::{encode_row, Navigation, Row, Shape},
    slots::Slot,
    Result, SegmentError, SegmentMeta,
};
use crate::{
    fs::{Access, Durability},
    pages::{FileKind, PagedFile, Sealed},
};

/// Records buffered to this before each append
const CHUNK: usize = 1 << 20;

/// O(rows) read-back of every sealed segment (RocksDB `paranoid_file_checks`): tests and index
/// test suites only (writer bugs fail at write time; production skips the re-read)
const PARANOID_FILE_CHECKS: bool = cfg!(any(test, feature = "testing"));

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
    dir: SegmentDir,
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
        let (id, records) = (self.id, self.navigation.records());
        self.navigation.finish(&mut self.file).map_err(|error| navigation_error(id, error))?;
        // a new file: its blocks + length = metadata no other file's flush commits
        let meta = SegmentMeta { id, records, sealed: self.file.seal(Durability::PerFile)? };
        Ok((meta, self.digest.finalize(), self.file))
    }
}

/// `Contract` reason for `key`: each input's row of it, in input order (= the merge heap's order
/// for equal keys; each input holds a key at most once)
fn held_reason(sources: &[SegmentFile], key: &[u8]) -> String {
    let held: Vec<&str> = sources
        .iter()
        .filter_map(|file| {
            let slot = file.seek(key);
            (slot < file.records() && file.key(slot) == key).then(|| match file.content(slot) {
                Row::Value(_) => "value",
                Row::Tombstone => "tombstone",
            })
        })
        .collect();
    format!("key held {} times: {}", held.len(), held.join(", "))
}

fn navigation_error(segment: u32, error: FilterError) -> SegmentError {
    match error {
        FilterError::Build(reason) => SegmentError::Filter { segment, reason },
        FilterError::Io(error) => SegmentError::Io(error),
    }
}

impl SegmentWriter {
    pub(crate) fn new(dir: SegmentDir, shape: Shape) -> Self {
        Self { dir, shape }
    }

    pub(crate) fn dir(&self) -> &SegmentDir {
        &self.dir
    }

    pub(crate) fn shape(&self) -> Shape {
        self.shape
    }

    /// Sorts `rows` (`(key, value)`, `None` = tombstone) and writes them as segment `id`, sealed;
    /// `None` for none
    ///
    /// - not yet durable until listed in a manifest, unlinked until [`SegmentDir::sync`]
    /// - duplicate keys panic (the buffer holds one row per key: a cancelled pair writes neither)
    /// - unique keys → unstable sort exact; parallel on the CPU pool
    pub(crate) fn write(
        &self,
        id: u32,
        mut rows: Vec<(&[u8], Option<&[u8]>)>,
    ) -> Result<Option<SegmentMeta>> {
        if rows.is_empty() {
            return Ok(None);
        }
        rows.par_sort_unstable_by_key(|(key, _)| *key);

        let mut out = self.create(id, rows.len() as u64)?;
        let mut row = Vec::with_capacity(self.shape.stride);
        for (key, value) in rows {
            row.clear();
            encode_row(&self.shape, key, value.map_or(Row::Tombstone, Row::Value), &mut row);
            out.push(&row)?;
        }
        self.sealed(out).map(Some)
    }

    /// K-way merge of `inputs` into segment `id`, sealed; `None` once `cancel` is set (partial file
    /// unlisted → removed at open); no row left → no file (`output: None`)
    /// - row rules / commit order + memory: `usage.md` "Deletes" / "On disk"
    pub(crate) fn merge(
        &self,
        id: u32,
        inputs: &[SegmentMeta],
        cancel: &AtomicBool,
        slot: &Slot<'_>,
    ) -> Result<Option<Merged>> {
        let sources = inputs
            .iter()
            .map(|meta| SegmentFile::open(&self.dir, meta, self.shape, Access::Sequential))
            .collect::<Result<Vec<_>>>()?;
        let total: u64 = inputs.iter().map(|meta| meta.records).sum();

        let head = |source: usize, slot: usize| -> Option<Head<'_>> {
            let file = &sources[source];
            (slot < file.records()).then(|| Reverse((file.key(slot), source, slot)))
        };
        let mut heads: BinaryHeap<Head<'_>> =
            (0..sources.len()).filter_map(|source| head(source, 0)).collect();

        let mut out: Option<SegmentOut> = None;
        let mut cancelled = 0;
        // read + written per row, charged to the engine's merge bandwidth a chunk at a time
        let mut uncharged = 0;
        while let Some(Reverse((key, source, at))) = heads.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Ok(None);
            }
            heads.extend(head(source, at + 1));
            let first = sources[source].row(at);
            let (mut values, mut tombstones) = (0, 0);
            let mut tally = |content| match content {
                Row::Value(_) => values += 1,
                Row::Tombstone => tombstones += 1,
            };
            tally(sources[source].content(at));
            while let Some(&Reverse((next, source, at))) = heads.peek() {
                if next != key {
                    break;
                }
                heads.pop();
                heads.extend(head(source, at + 1));
                tally(sources[source].content(at));
            }
            uncharged += (values + tombstones) * self.shape.stride;
            match (values, tombstones) {
                (1, 0) | (0, 1) => {
                    let writing = match &mut out {
                        Some(out) => out,
                        None => out.insert(self.create(id, total)?),
                    };
                    writing.push(first)?;
                    uncharged += first.len();
                }
                (1, 1) => cancelled += 1,
                _ => {
                    let reason = held_reason(&sources, key);
                    return Err(SegmentError::Contract { segment: id, reason });
                }
            }
            if uncharged >= CHUNK {
                slot.charge(std::mem::take(&mut uncharged));
            }
        }
        slot.charge(uncharged);
        let output = out.map(|out| self.sealed(out)).transpose()?;
        let written = output.map_or(0, |output| output.records);
        let inputs: u64 = sources.iter().map(|file| file.records() as u64).sum();
        assert_eq!(written + 2 * cancelled, inputs, "merge accounts for every input row");
        Ok(Some(Merged { output, cancelled }))
    }

    /// `out` finished, then (under [`PARANOID_FILE_CHECKS`]) read back through its page checksums,
    /// then out of page cache unless its table [`cache_writes`](crate::MapTable::cache_writes)
    /// (after the read-back: it faults every page back in)
    fn sealed(&self, out: SegmentOut) -> Result<SegmentMeta> {
        let (meta, digest, file) = out.finish()?;
        if PARANOID_FILE_CHECKS {
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
        let file = SegmentFile::open(&self.dir, meta, self.shape, Access::Sequential)?;
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
            file.content(slot);
            digest.update(file.row(slot));
        }
        assert_eq!(digest.finalize(), written, "segment {}: rows read back as written", meta.id);
        Ok(())
    }

    fn create(&self, id: u32, records: u64) -> Result<SegmentOut> {
        let path = self.dir.segment(id);
        let file = PagedFile::open(self.dir.fs(), &path, Sealed::EMPTY, FileKind::Segment)?;
        Ok(SegmentOut {
            id,
            file,
            navigation: Navigation::new(self.shape, records, &self.dir, id),
            chunk: Vec::with_capacity(CHUNK + self.shape.stride),
            digest: crc32fast::Hasher::new(),
        })
    }
}
