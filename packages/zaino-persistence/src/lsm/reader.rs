//! Reading across a set of segments
//!
//! - one [`Snapshot`] pin per request (one atomic load)
//! - a merge landing mid-request changes nothing pinned (pre-merge segments hold the same rows)

use std::{
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use rayon::prelude::*;

use super::{
    file::SegmentFile,
    layout::Shape,
    parse_file_name,
    record::{Key, Record},
    Result, SegmentMeta,
};
use crate::fs::{Access, Fs};

/// Batch size from which [`Snapshot::get_many`] fans out on rayon (measured, 9 segments, 2.3M
/// rows: 2-16 keys 5-10× slower on rayon (wake-up), crossover ≈ 64, 256 keys 3× faster)
const PARALLEL_FROM: usize = 64;

/// One mapped segment, typed by its key
struct Mapped<K> {
    file: SegmentFile,
    _key: PhantomData<fn() -> K>,
}

impl<K: Key> Mapped<K> {
    /// The row keyed exactly `key` (filter first: a miss touches no record)
    fn get<R: Record<Key = K>>(&self, key: &[u8]) -> Option<R> {
        if !self.file.may_contain(key) {
            return None;
        }
        let slot = self.file.seek(key);
        (slot < self.file.records() && self.file.key(slot) == key)
            .then(|| decoded(&self.file, slot))
    }
}

/// How a reader's mapping is read: probed = point lookups on hash-like keys, else range scans
fn reads<R: Record>() -> Access {
    match Shape::of::<R>().probed {
        true => Access::Random,
        false => Access::Normal,
    }
}

/// Sealed records decode (a failure = a writer bug, never data)
fn decoded<R: Record>(file: &SegmentFile, slot: usize) -> R {
    R::decode(file.row(slot)).expect("a sealed segment's records decode")
}

/// A consistent view of every committed segment
pub struct Snapshot<K> {
    segments: Vec<Arc<Mapped<K>>>,
}

impl<K: Key> Snapshot<K> {
    /// Every record keyed `start` inclusive to `end` exclusive, ascending, across all segments
    pub fn range<R: Record<Key = K>>(&self, start: &K, end: &K) -> Vec<R> {
        self.range_at_most(start, end, usize::MAX).expect("no range holds usize::MAX rows")
    }

    /// [`range`](Self::range), or `None` once more than `limit` rows match across all segments
    ///
    /// - stops scanning at row `limit + 1` (a serve-path budget: cost bounded by `limit`, not by
    ///   how many rows the range holds)
    pub fn range_at_most<R: Record<Key = K>>(
        &self,
        start: &K,
        end: &K,
        limit: usize,
    ) -> Option<Vec<R>> {
        let (start, end) = (start.encode(), end.encode());
        let mut found = Vec::new();

        for mapped in &self.segments {
            let file = &mapped.file;
            for slot in file.seek(&start)..file.records() {
                if file.key(slot) >= end.as_slice() {
                    break;
                }
                if found.len() == limit {
                    return None;
                }
                found.push(decoded::<R>(file, slot));
            }
        }

        found.sort_by_key(|record| record.key());
        for pair in found.windows(2) {
            assert!(pair[0].key() < pair[1].key(), "a key listed in two committed segments");
        }
        Some(found)
    }

    /// The row keyed `key` (committed segments hold a key at most once; the owner's invariant)
    ///
    /// - probed sets: each segment's filter first, so a miss is a memory probe per segment
    pub fn get<R: Record<Key = K>>(&self, key: &K) -> Option<R> {
        self.find(&key.encode())
    }

    /// Rows for `keys`, in `keys`' order (`None` = absent)
    ///
    /// - probed in ascending key order: neighbouring keys share fence, filter and record pages
    /// - [`PARALLEL_FROM`] keys and up: contiguous runs on the rayon pool; below: this thread
    pub fn get_many<R: Record<Key = K> + Send>(&self, keys: &[K]) -> Vec<Option<R>>
    where
        K: Sync,
    {
        let mut sorted: Vec<(Vec<u8>, usize)> =
            keys.iter().enumerate().map(|(at, key)| (key.encode(), at)).collect();
        sorted.sort_unstable();
        let probe = |(key, at): &(Vec<u8>, usize)| (*at, self.find(key));
        let found: Vec<(usize, Option<R>)> = match sorted.len() < PARALLEL_FROM {
            true => sorted.iter().map(probe).collect(),
            false => sorted.par_iter().map(probe).collect(),
        };

        let mut rows: Vec<Option<R>> = std::iter::repeat_with(|| None).take(keys.len()).collect();
        for (at, row) in found {
            rows[at] = row;
        }
        rows
    }

    /// Newest segment first: list ≈ data age (batches append, a merge takes its oldest input's
    /// slot) and lookups skew recent; order never changes an answer (keys unique across segments)
    fn find<R: Record<Key = K>>(&self, key: &[u8]) -> Option<R> {
        self.segments.iter().rev().find_map(|mapped| mapped.get::<R>(key))
    }

    /// Committed segments in list order (the merge policy's input)
    pub fn segments(&self) -> Vec<SegmentMeta> {
        self.segments.iter().map(|mapped| mapped.file.meta).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

/// Directory of committed segments, published for reading (clones share one snapshot; readers
/// and the publishing writer never block each other)
pub struct SegmentSet<K> {
    fs: Arc<dyn Fs>,
    dir: PathBuf,
    snapshot: Arc<ArcSwap<Snapshot<K>>>,
}

impl<K> std::fmt::Debug for SegmentSet<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentSet")
            .field("dir", &self.dir)
            .field("segments", &self.snapshot.load().segments.len())
            .finish()
    }
}

impl<K> Clone for SegmentSet<K> {
    fn clone(&self) -> Self {
        Self {
            fs: Arc::clone(&self.fs),
            dir: self.dir.clone(),
            snapshot: Arc::clone(&self.snapshot),
        }
    }
}

impl<K: Key + Send + Sync + 'static> SegmentSet<K> {
    /// Opens `dir` holding exactly `listed`: every other segment file (and its checksums) removed,
    /// every listed one mapped (lengths only; pages checked as reads touch them)
    pub fn open<R: Record<Key = K>>(
        fs: Arc<dyn Fs>,
        dir: &Path,
        listed: &[SegmentMeta],
    ) -> Result<Self> {
        let mut removed = false;
        for name in fs.list(dir)? {
            let Some(id) = parse_file_name(&name) else {
                continue;
            };
            if !listed.iter().any(|segment| segment.id == id) {
                fs.remove(&dir.join(name))?;
                removed = true;
            }
        }
        if removed {
            fs.sync_dir(dir)?;
        }

        let segments = listed
            .iter()
            .map(|meta| {
                SegmentFile::open(fs.as_ref(), dir, meta, Shape::of::<R>(), reads::<R>())
                    .map(|file| Arc::new(Mapped { file, _key: PhantomData }))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            fs,
            dir: dir.to_path_buf(),
            snapshot: Arc::new(ArcSwap::from_pointee(Snapshot { segments })),
        })
    }

    /// Publishes a newly committed list; segments already mapped are shared, not remapped
    pub fn publish<R: Record<Key = K>>(&self, listed: &[SegmentMeta]) -> Result<()> {
        let current = self.snapshot.load();
        let mut segments = Vec::with_capacity(listed.len());
        for meta in listed {
            let reused =
                current.segments.iter().find(|mapped| mapped.file.meta == *meta).map(Arc::clone);
            segments.push(match reused {
                Some(mapped) => mapped,
                None => Arc::new(Mapped {
                    file: SegmentFile::open(
                        self.fs.as_ref(),
                        &self.dir,
                        meta,
                        Shape::of::<R>(),
                        reads::<R>(),
                    )?,
                    _key: PhantomData,
                }),
            });
        }
        self.snapshot.store(Arc::new(Snapshot { segments }));
        Ok(())
    }

    /// Current view (one atomic load: once per request)
    pub fn pin(&self) -> Arc<Snapshot<K>> {
        self.snapshot.load_full()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(super) fn fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }
}
