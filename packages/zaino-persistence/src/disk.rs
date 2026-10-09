//! Zaino's engine: sequences as positional files, maps as LSM segment sets, one manifest
//!
//! ```text
//! <dir>/
//!   MANIFEST            committed tip ‖ each sequence's seals ‖ each map's segment list
//!   <sequence>.dat      (+ <sequence>.idx when Variable)        `sequence.rs`
//!   <map>/<id>.seg      one sorted segment per batch or merge   `lsm`
//! ```
//!
//! - apply → [`WriteBuffer`] (RAM, unsorted)
//! - commit → every table written + sealed in parallel → manifest (commit point) → readable
//! - failed commit poisons the store (`docs/design/durability.md` §6): recovery = reopen

use std::{
    io,
    num::NonZeroUsize,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use rayon::prelude::*;
use zaino_primitives::types::BlockRef;

use crate::{
    dir::IndexDir,
    fs::Fs,
    lsm::{
        decode_list, encode_list, file_name, LsmConfig, SegmentLog, SegmentMeta, Slots, Snapshot,
    },
    manifest::{self, BodyReader, Committed, Identity, ManifestError},
    overlay::OverlayView,
    pages::{scrub, Sealed},
    port::{
        BlockChanges, CommittedView, MapId, MapRead, PersistenceEngine, Schema, SequenceId,
        SequenceRead, Store, Verification, View,
    },
    sequence::{self, Seals, SequenceFile, SequencePages},
    write_buffer::{StagedView, WriteBuffer},
    StoreError,
};

/// Files for sequences, LSM for maps, on one filesystem (clones share the merge slots)
#[derive(Debug, Clone)]
pub struct DiskEngine {
    fs: Arc<dyn Fs>,
    fanout: usize,
    slots: Arc<Slots>,
}

impl DiskEngine {
    /// Panics: `lsm.fanout` < 2 (a merge of one segment = no merge)
    pub fn new(fs: Arc<dyn Fs>, lsm: LsmConfig) -> Self {
        assert!(lsm.fanout >= 2, "lsm fanout {} < 2", lsm.fanout);
        let slots = Arc::new(Slots::new(lsm.merge_slots.get()));
        Self { fs, fanout: lsm.fanout, slots }
    }

    /// Merges after `fanout` same-tier segments (small = merges in a handful of commits)
    #[cfg(test)]
    pub(crate) fn with_fanout(fs: Arc<dyn Fs>, fanout: usize) -> Self {
        Self::new(fs, LsmConfig { fanout, ..LsmConfig::default() })
    }
}

/// Index directory's writer (holds its `LOCK`)
#[derive(Debug)]
pub struct DiskStore {
    dir: IndexDir,
    schema: Schema,
    sequences: Vec<SequenceFile>,
    maps: Vec<SegmentLog>,
    view: DiskView,
    buffer: WriteBuffer,
    write_buffer: NonZeroUsize,
    failed: bool,
}

/// Committed state of a [`DiskStore`] (shared by clones)
#[derive(Debug, Clone)]
pub struct DiskView {
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    schema: Schema,
    tip: Option<BlockRef>,
    sequences: Vec<SequencePages>,
    maps: Vec<Arc<Snapshot>>,
}

/// Manifest body: what one commit made durable
#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    committed: Committed,
    sequences: Vec<Seals>,
    maps: Vec<Vec<SegmentMeta>>,
}

impl Body {
    fn empty(schema: &Schema) -> Self {
        Self {
            committed: Committed::EMPTY,
            sequences: vec![Seals::EMPTY; schema.sequences().len()],
            maps: vec![Vec::new(); schema.maps().len()],
        }
    }

    /// `committed ‖ seals per sequence ‖ segment list per map`, in schema order
    fn encode(&self, schema: &Schema) -> Vec<u8> {
        let mut out = Vec::new();
        self.committed.encode(&mut out);
        for (table, seals) in schema.sequences().iter().zip(&self.sequences) {
            seals.encode(table.record, &mut out);
        }
        for list in &self.maps {
            encode_list(list, &mut out);
        }
        out
    }

    fn decode(bytes: &[u8], schema: &Schema) -> Result<Self, ManifestError> {
        let mut body = BodyReader::new(bytes);
        let committed = Committed::decode(&mut body)?;
        let sequences = schema
            .sequences()
            .iter()
            .map(|table| Seals::decode(table.record, &mut body))
            .collect::<Result<_, _>>()?;
        let maps =
            schema.maps().iter().map(|_| decode_list(&mut body)).collect::<Result<_, _>>()?;
        body.finish()?;
        Ok(Self { committed, sequences, maps })
    }

    /// Every file this body seals, by path under the index directory
    fn files(&self, schema: &Schema) -> Vec<(String, Sealed)> {
        let sequences = schema
            .sequences()
            .iter()
            .zip(&self.sequences)
            .flat_map(|(table, seals)| sequence::files(table, seals));
        let maps = schema.maps().iter().zip(&self.maps).flat_map(|(table, list)| {
            list.iter().map(|segment| {
                (format!("{}/{}", table.name, file_name(segment.id)), segment.sealed)
            })
        });
        sequences.chain(maps).collect()
    }
}

fn identity(schema: &Schema) -> Identity {
    Identity { kind: schema.kind, format: schema.format, network: schema.network }
}

/// Directory a table's files sit in (`a/b` → `a`), `None` = the index directory itself
fn parent(name: &str) -> Option<&str> {
    name.rsplit_once('/').map(|(parent, _)| parent)
}

impl PersistenceEngine for DiskEngine {
    type Store = DiskStore;

    fn open(
        &self,
        path: &Path,
        schema: &Schema,
        write_buffer: NonZeroUsize,
    ) -> Result<DiskStore, StoreError> {
        let opened = IndexDir::open(Arc::clone(&self.fs), path, identity(schema))?;
        let mut dir = opened.dir;
        let body = match &opened.body {
            Some(bytes) => Body::decode(bytes, schema)?,
            None => {
                for table in schema.sequences() {
                    for name in SequenceFile::names(table) {
                        dir.ensure_empty(&name)?;
                    }
                }
                for table in schema.maps() {
                    dir.ensure_empty_dir(table.name)?;
                }
                Body::empty(schema)
            }
        };

        for parent in schema.sequences().iter().filter_map(|table| parent(table.name)) {
            dir.subdir(parent)?;
        }
        let sequences = schema
            .sequences()
            .iter()
            .zip(&body.sequences)
            .map(|(table, seals)| SequenceFile::open(self.fs.as_ref(), dir.path(), table, *seals))
            .collect::<Result<Vec<_>, _>>()?;
        let map_dirs: Vec<PathBuf> =
            schema.maps().iter().map(|table| dir.subdir(table.name)).collect::<io::Result<_>>()?;
        if opened.body.is_none() {
            dir.commit(&body.encode(schema))?;
        }

        let mut maps = Vec::with_capacity(schema.maps().len());
        for ((table, list), map_dir) in schema.maps().iter().zip(&body.maps).zip(&map_dirs) {
            let fs = Arc::clone(&self.fs);
            let slots = Arc::clone(&self.slots);
            maps.push(SegmentLog::open(fs, map_dir, table, list, self.fanout, slots)?);
        }
        let state = State {
            schema: *schema,
            tip: body.committed.tip,
            sequences: sequences.iter().map(|file| file.pages(None)).collect::<io::Result<_>>()?,
            maps: maps.iter().map(|log| Arc::clone(log.snapshot())).collect(),
        };
        Ok(DiskStore {
            dir,
            schema: *schema,
            sequences,
            maps,
            view: DiskView { state: Arc::new(state) },
            buffer: WriteBuffer::empty(schema),
            write_buffer,
            failed: false,
        })
    }

    /// Read only, no lock: safe beside a live writer
    fn verify(&self, path: &Path, schema: &Schema) -> Result<Verification, StoreError> {
        let fs = self.fs.as_ref();
        verify_committed(fs, path, schema, || manifest::read(fs, path, identity(schema)))
    }
}

/// Every file the manifest `committed` reads names, scrubbed
///
/// - live writer's merge may retire a listed segment mid-scrub: lost file + manifest moved since
///   = scrub again against the newer one
fn verify_committed(
    fs: &dyn Fs,
    path: &Path,
    schema: &Schema,
    mut committed: impl FnMut() -> io::Result<Option<Vec<u8>>>,
) -> Result<Verification, StoreError> {
    let mut manifest = committed()?;
    loop {
        let Some(bytes) = &manifest else {
            return Ok(Verification { heights: 0, units: Vec::new() });
        };
        let body = Body::decode(bytes, schema)?;
        let units = body
            .files(schema)
            .into_iter()
            .map(|(name, sealed)| scrub(fs, path, &name, sealed))
            .collect::<io::Result<Vec<_>>>()?;
        if units.iter().any(|unit| unit.lost) {
            let newer = committed()?;
            if newer != manifest {
                manifest = newer;
                continue;
            }
        }
        return Ok(Verification { heights: body.committed.count(), units });
    }
}

impl DiskStore {
    /// Blocks until every running merge finishes (lands on the next commit)
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        for log in &self.maps {
            log.settle();
        }
    }

    /// WriteBuffer written + sealed, every table in parallel → manifest at `tip` → new view
    fn write(&mut self, tip: BlockRef) -> Result<(), StoreError> {
        let (schema, buffer) = (&self.schema, &self.buffer);
        let (sequences, maps) = rayon::join(
            || write_sequences(&mut self.sequences, schema, buffer),
            || write_maps(&mut self.maps, schema, buffer),
        );

        let body =
            Body { committed: Committed { tip: Some(tip) }, sequences: sequences?, maps: maps? };
        self.dir.commit(&body.encode(&self.schema))?;
        for log in &mut self.maps {
            log.committed()?;
        }

        let previous = &self.view.state;
        let state = State {
            schema: self.schema,
            tip: Some(tip),
            sequences: self
                .sequences
                .iter()
                .zip(&previous.sequences)
                .map(|(file, old)| file.pages(Some(old)))
                .collect::<io::Result<_>>()?,
            maps: self.maps.iter().map(|log| Arc::clone(log.snapshot())).collect(),
        };
        self.view = DiskView { state: Arc::new(state) };
        Ok(())
    }
}

/// Each sequence's buffered records appended, then sealed (one table per thread)
fn write_sequences(
    files: &mut [SequenceFile],
    schema: &Schema,
    buffer: &WriteBuffer,
) -> Result<Vec<Seals>, StoreError> {
    let tables = files.par_iter_mut().zip(schema.sequences());
    let sealed = tables.map(|(file, &table)| {
        for record in buffer.records(table) {
            file.append(record)?;
        }
        Ok(file.seal()?)
    });
    sealed.collect()
}

/// Each map's buffered rows as one segment (one table per thread)
fn write_maps(
    logs: &mut [SegmentLog],
    schema: &Schema,
    buffer: &WriteBuffer,
) -> Result<Vec<Vec<SegmentMeta>>, StoreError> {
    let tables = logs.par_iter_mut().zip(schema.maps());
    tables.map(|(log, &table)| Ok(log.batch(buffer.map_rows(table))?)).collect()
}

impl Store for DiskStore {
    type View = DiskView;

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn apply(&mut self, changes: BlockChanges) {
        assert_eq!(changes.schema(), &self.schema, "changes built for another schema");
        let last = self.buffer.tip().or(self.view.tip()).map(|tip| tip.height);
        let tip = changes.tip().height;
        assert!(Some(tip) > last, "apply at height {tip}, not above the last applied {last:?}");
        self.buffer.push(&changes);
        if self.buffer.heap() >= self.write_buffer.get() {
            if let Err(error) = self.commit() {
                error.commit_failed(self.schema.kind.name(), self.dir.path());
            }
        }
    }

    fn buffered_bytes(&self) -> usize {
        self.buffer.heap()
    }

    fn commit(&mut self) -> Result<(), StoreError> {
        assert!(!self.failed, "commit after a failed one (fsync errors are never retried)");
        let Some(tip) = self.buffer.tip() else { return Ok(()) };
        self.write(tip).inspect_err(|_| self.failed = true)?;
        self.buffer = WriteBuffer::empty(&self.schema);
        Ok(())
    }

    fn committed(&self) -> DiskView {
        self.view.clone()
    }

    fn staged(&self) -> StagedView<'_, DiskView> {
        OverlayView::new(self.view.clone(), &self.buffer)
    }
}

// names unique vs callers' methods (inherent methods win resolution, even private ones)
impl DiskView {
    fn sequence_pages(&self, table: SequenceId) -> &SequencePages {
        let at = usize::from(table.0);
        self.state.sequences.get(at).unwrap_or_else(|| panic!("{table:?} not in the schema"))
    }

    fn map_snapshot(&self, table: MapId) -> &Snapshot {
        let at = usize::from(table.0);
        self.state.maps.get(at).unwrap_or_else(|| panic!("{table:?} not in the schema"))
    }
}

impl View for DiskView {
    fn tip(&self) -> Option<BlockRef> {
        self.state.tip
    }

    fn schema(&self) -> &Schema {
        &self.state.schema
    }
}

impl CommittedView for DiskView {}

impl SequenceRead for DiskView {
    fn len(&self, table: SequenceId) -> u64 {
        self.sequence_pages(table).len()
    }

    fn record(&self, table: SequenceId, at: u64) -> Option<Bytes> {
        self.sequence_pages(table).record(at)
    }

    fn records(&self, table: SequenceId, range: Range<u64>) -> Vec<Bytes> {
        self.sequence_pages(table).records(range)
    }
}

impl MapRead for DiskView {
    fn value(&self, table: MapId, key: &[u8]) -> Option<Bytes> {
        self.map_snapshot(table).get(key)
    }

    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        self.map_snapshot(table).get_many(keys)
    }

    fn range(
        &self,
        table: MapId,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<(Bytes, Bytes)>> {
        self.map_snapshot(table).range(start, end, limit)
    }
}

#[cfg(test)]
mod tests;
