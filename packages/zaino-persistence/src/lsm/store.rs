//! One LSM-backed index directory: `MANIFEST` + one sub-directory of segments per set
//!
//! ```text
//! MANIFEST body:  committed extent ‖ tip hash ‖ one segment list per set (in `SETS` order)
//! ```
//!
//! - an index = a marker type implementing [`LsmIndex`] (identity, tunings, row types)

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::NetworkType;

use super::{
    decode_list, encode_list, segment_files, Record, SegmentError, SegmentLog, SegmentMeta,
    SegmentSet,
};
use crate::{
    dir::IndexDir,
    fs::Fs,
    manifest::{self, BodyReader, Committed, Identity, IndexKind, ManifestError},
    pages::CommittedFiles,
    StoreError,
};

/// An index stored as size-tiered segment sets: `impl` on a marker type, then use [`LsmStore`]
pub trait LsmIndex: 'static {
    const KIND: IndexKind;

    /// On-disk layout version (bumped on any change to the segments or the manifest body)
    const FORMAT: u16;

    /// Same-tier segments merged into one (write amplification `log_FANOUT(rows)`)
    const FANOUT: usize = 8;

    /// Sub-directory per segment set, in manifest order
    const SETS: &'static [&'static str];

    /// `SegmentLog<Row>` per set: one alone, or a pair
    type Logs: SegmentLogs;

    /// Index-specific agreement between the committed tip and the segment lists
    fn check(_committed: &Committed, _lists: &[Vec<SegmentMeta>]) -> Result<(), ManifestError> {
        Ok(())
    }
}

/// An index's segment logs, one per set (see the impls: `SegmentLog<R>`, `(SegmentLog<A>,
/// SegmentLog<B>)`)
pub trait SegmentLogs: Send + Sized + 'static {
    const COUNT: usize;

    /// Read handles, one per set
    type Sets: Clone + Send + Sync + 'static;

    /// One commit's rows, one `Vec` per set
    type Rows: Send;

    fn open(
        fs: &Arc<dyn Fs>,
        dirs: &[PathBuf],
        lists: &[Vec<SegmentMeta>],
        fanout: usize,
    ) -> Result<Self, SegmentError>;

    fn sets(&self) -> Self::Sets;

    /// Rows as one new segment per set (+ finished merges): the lists the next manifest carries
    fn batch(&mut self, rows: Self::Rows) -> Result<Vec<Vec<SegmentMeta>>, SegmentError>;

    /// Manifest durable → each set publishes its staged list
    fn committed(&mut self) -> Result<(), SegmentError>;

    /// A background merge has finished and waits for the next commit to land it
    fn merge_finished(&self) -> bool;
}

impl<R> SegmentLogs for SegmentLog<R>
where
    R: Record + 'static,
    R::Key: Sync + 'static,
{
    const COUNT: usize = 1;
    type Sets = SegmentSet<R::Key>;
    type Rows = Vec<R>;

    fn open(
        fs: &Arc<dyn Fs>,
        dirs: &[PathBuf],
        lists: &[Vec<SegmentMeta>],
        fanout: usize,
    ) -> Result<Self, SegmentError> {
        let set = SegmentSet::open::<R>(Arc::clone(fs), &dirs[0], &lists[0])?;
        SegmentLog::open(set, fanout)
    }

    fn sets(&self) -> Self::Sets {
        self.set().clone()
    }

    fn batch(&mut self, rows: Vec<R>) -> Result<Vec<Vec<SegmentMeta>>, SegmentError> {
        Ok(vec![SegmentLog::batch(self, rows)?])
    }

    fn committed(&mut self) -> Result<(), SegmentError> {
        SegmentLog::committed(self)
    }

    fn merge_finished(&self) -> bool {
        SegmentLog::merge_finished(self)
    }
}

impl<A, B> SegmentLogs for (SegmentLog<A>, SegmentLog<B>)
where
    A: Record + 'static,
    A::Key: Sync + 'static,
    B: Record + 'static,
    B::Key: Sync + 'static,
{
    const COUNT: usize = 2;
    type Sets = (SegmentSet<A::Key>, SegmentSet<B::Key>);
    type Rows = (Vec<A>, Vec<B>);

    fn open(
        fs: &Arc<dyn Fs>,
        dirs: &[PathBuf],
        lists: &[Vec<SegmentMeta>],
        fanout: usize,
    ) -> Result<Self, SegmentError> {
        let a = SegmentSet::open::<A>(Arc::clone(fs), &dirs[0], &lists[0])?;
        let b = SegmentSet::open::<B>(Arc::clone(fs), &dirs[1], &lists[1])?;
        Ok((SegmentLog::open(a, fanout)?, SegmentLog::open(b, fanout)?))
    }

    fn sets(&self) -> Self::Sets {
        (self.0.set().clone(), self.1.set().clone())
    }

    fn batch(&mut self, (a, b): (Vec<A>, Vec<B>)) -> Result<Vec<Vec<SegmentMeta>>, SegmentError> {
        Ok(vec![self.0.batch(a)?, self.1.batch(b)?])
    }

    fn committed(&mut self) -> Result<(), SegmentError> {
        self.0.committed()?;
        self.1.committed()
    }

    fn merge_finished(&self) -> bool {
        self.0.merge_finished() || self.1.merge_finished()
    }
}

/// Write side of an [`LsmIndex`] directory (single writer: holds the directory's `LOCK`)
///
/// - `failed`: a commit errored → every later one panics (fsync errors never retried,
///   `docs/design/durability.md` §6)
pub struct LsmStore<I: LsmIndex> {
    dir: IndexDir,
    committed: Committed,
    logs: I::Logs,
    failed: bool,
}

impl<I: LsmIndex> LsmStore<I> {
    /// Opens `path` at its committed state (every listed segment proven, every unlisted one
    /// removed); a fresh directory = empty sets, committed once
    pub fn open(fs: Arc<dyn Fs>, path: &Path, network: NetworkType) -> Result<Self, StoreError> {
        const { assert!(I::SETS.len() == <I::Logs as SegmentLogs>::COUNT, "one name per set") };

        let opened = IndexDir::open(Arc::clone(&fs), path, identity::<I>(network))?;
        let dir = opened.dir;
        let (committed, lists) = match &opened.body {
            Some(body) => decode::<I>(body)?,
            None => {
                for set in I::SETS {
                    dir.ensure_empty_dir(set)?;
                }
                (Committed::EMPTY, vec![Vec::new(); I::SETS.len()])
            }
        };
        let dirs = I::SETS.iter().map(|set| dir.subdir(set)).collect::<io::Result<Vec<_>>>()?;
        if opened.body.is_none() {
            dir.commit(&encode(&committed, &lists))?;
        }

        let logs = I::Logs::open(&fs, &dirs, &lists, I::FANOUT)?;
        Ok(Self { dir, committed, logs, failed: false })
    }

    pub fn committed(&self) -> Committed {
        self.committed
    }

    /// Read handles onto the committed segments (shared: readers never block the writer)
    pub fn sets(&self) -> <I::Logs as SegmentLogs>::Sets {
        self.logs.sets()
    }

    pub fn logs(&self) -> &I::Logs {
        &self.logs
    }

    /// A background merge has finished: the next commit lands it (its inputs stay listed, and
    /// readers keep visiting them, until then)
    pub fn merge_finished(&self) -> bool {
        self.logs.merge_finished()
    }

    /// `rows` as one segment per set, then the manifest listing them (the commit point), then
    /// readers see them (never a height not on disk)
    ///
    /// - `tip` above the committed one (asserted)
    /// - an `Err` = this store is done: drop it, reopen (recovery = the last durable manifest)
    pub fn commit(
        &mut self,
        rows: <I::Logs as SegmentLogs>::Rows,
        tip: BlockRef,
    ) -> Result<(), StoreError> {
        assert!(!self.failed, "LSM commit after a failed one (fsync errors are never retried)");
        assert!(
            Some(tip.height) > self.committed.height(),
            "LSM commit to height {}, not above the committed {:?}",
            tip.height,
            self.committed.height()
        );
        let committed = Committed { tip: Some(tip) };
        let lists = self.logs.batch(rows).inspect_err(|_| self.failed = true)?;
        I::check(&committed, &lists).expect("commit satisfies its own manifest check");
        self.dir.commit(&encode(&committed, &lists)).inspect_err(|_| self.failed = true)?;
        self.committed = committed;
        self.logs.committed().inspect_err(|_| self.failed = true)?;
        Ok(())
    }
}

/// Every file an [`LsmIndex`] directory's manifest seals (offline scrub; plain reads, no lock)
pub fn committed_files<I: LsmIndex>(
    path: &Path,
    network: NetworkType,
) -> io::Result<CommittedFiles> {
    let Some(body) = manifest::read(path, identity::<I>(network))? else {
        return Ok(CommittedFiles { tip: None, files: Vec::new() });
    };
    let (committed, lists) = decode::<I>(&body).map_err(io::Error::other)?;
    let files = I::SETS.iter().zip(&lists).flat_map(|(set, list)| segment_files(set, list));
    Ok(CommittedFiles { tip: committed.height(), files: files.collect() })
}

fn identity<I: LsmIndex>(network: NetworkType) -> Identity {
    Identity { kind: I::KIND, format: I::FORMAT, network }
}

fn encode(committed: &Committed, lists: &[Vec<SegmentMeta>]) -> Vec<u8> {
    let mut out = Vec::new();
    committed.encode(&mut out);
    for list in lists {
        encode_list(list, &mut out);
    }
    out
}

fn decode<I: LsmIndex>(bytes: &[u8]) -> Result<(Committed, Vec<Vec<SegmentMeta>>), ManifestError> {
    let mut body = BodyReader::new(bytes);
    let committed = Committed::decode(&mut body)?;
    let lists = I::SETS.iter().map(|_| decode_list(&mut body)).collect::<Result<Vec<_>, _>>()?;
    body.finish()?;
    I::check(&committed, &lists)?;
    Ok((committed, lists))
}
