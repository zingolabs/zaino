//! Zaino's engine: sequences as positional files, maps as LSM segment sets, one manifest
//!
//! ```text
//! <dir>/
//!   MANIFEST            durable tip ‖ each sequence's seals ‖ each map's segment list
//!   <sequence>.dat      (+ <sequence>.idx when Variable)        `sequence.rs`
//!   <map>/<id>.seg      one sorted segment per batch or merge   `lsm`
//! ```
//!
//! - apply → [`WriteBuffer`] (RAM, unsorted)
//! - commit → every table written + sealed in parallel → manifest (commit point) → readable
//! - failed commit poisons the store: recovery = reopen (retried fsync may "succeed" on pages the
//!   kernel already marked clean: <https://wiki.postgresql.org/wiki/Fsync_Errors>)

use std::{
    io,
    num::{NonZeroU64, NonZeroUsize},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use rayon::prelude::*;
use zaino_primitives::types::BlockRef;

use crate::{
    changes::BlockChanges,
    error::{CommitFailed, StoreError},
    fs::{Durability, Fs},
    index_dir::{self, Depth, IndexDir},
    lsm::{
        self, decode_list, encode_list, Fanout, LsmConfig, SegmentMeta, SegmentSet, SegmentsView,
        Slots,
    },
    manifest::{self, BodyReader, DurableTip, Identity, ManifestError},
    overlay::{AppliedLayer, OverlayView},
    pages::{footprint, scrub, PageError, Sealed},
    port::{MapRead, PersistenceEngine, PinnedView, SequenceRead, Store, Verification, View},
    schema::{table_slot, MapId, Schema, SequenceId},
    sequence::{self, Seals, SequenceFile, SequencePages},
    write_buffer::{AppliedView, WriteBuffer},
};

/// Files for sequences, LSM for maps, on one filesystem (clones share the merge slots)
#[derive(Debug, Clone)]
pub struct DiskEngine {
    fs: Arc<dyn Fs>,
    fanout: Fanout,
    slots: Arc<Slots>,
}

impl DiskEngine {
    pub fn new(fs: Arc<dyn Fs>, lsm: LsmConfig) -> Self {
        let bytes_per_sec = u64::from(lsm.merge_mib_per_sec.get()) << 20;
        let bytes_per_sec = NonZeroU64::new(bytes_per_sec).expect("non-zero MiB << 20 != 0");
        let slots = Arc::new(Slots::new(lsm.merge_slots.get(), bytes_per_sec));
        Self { fs, fanout: lsm.fanout, slots }
    }
}

/// Index directory's writer (holds its `LOCK`)
///
/// - `durability` = how its commits make sequences durable, probed once at open
#[derive(Debug)]
pub struct DiskStore {
    dir: IndexDir,
    schema: Schema,
    sequences: Vec<SequenceFile>,
    maps: Vec<SegmentSet>,
    view: DiskView,
    buffer: WriteBuffer,
    write_buffer: NonZeroUsize,
    durability: Durability,
    failed: bool,
}

/// Durable state of a [`DiskStore`] (shared by clones)
#[derive(Debug, Clone)]
pub struct DiskView {
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    schema: Schema,
    tip: Option<BlockRef>,
    sequences: Vec<SequencePages>,
    maps: Vec<Arc<SegmentsView>>,
}

/// Manifest body: what one commit made durable
#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    durable: DurableTip,
    sequences: Vec<Seals>,
    maps: Vec<Vec<SegmentMeta>>,
}

/// `schema`'s store under `fanout` never outgrows its manifest slot: every map at the stall bound
/// (+ the segment landing) in every tier a `u64` record count reaches
pub fn manifest_fits(schema: &Schema, fanout: Fanout) -> bool {
    let fanout_width = u64::try_from(fanout.get()).expect("usize fits u64");
    let tiers = usize::try_from(u64::MAX.ilog(fanout_width)).expect("< 64 tiers") + 1;
    let segment = SegmentMeta { id: 0, records: 1, tombstones: 0, sealed: Sealed::EMPTY };
    let mut worst = Body::empty(schema);
    worst.maps.fill(vec![segment; tiers * (lsm::stall_at(fanout) + 1)]);
    manifest::fits(identity(schema), worst.encode().len())
}

impl Body {
    fn empty(schema: &Schema) -> Self {
        Self {
            durable: DurableTip::EMPTY,
            sequences: schema.sequences().iter().map(|table| Seals::empty(table.record)).collect(),
            maps: vec![Vec::new(); schema.maps().len()],
        }
    }

    /// `durable ‖ seals per sequence ‖ segment list per map`, in schema order
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.durable.encode(&mut out);
        for seals in &self.sequences {
            seals.encode(&mut out);
        }
        for list in &self.maps {
            encode_list(list, &mut out);
        }
        out
    }

    /// `bytes` = a body [`manifest::latest`] returned (CRC + identity passed: misfit panics)
    fn decode(bytes: &[u8], schema: &Schema) -> Self {
        let mut body = BodyReader::new(bytes);
        let durable = DurableTip::decode(&mut body);
        let sequences =
            schema.sequences().iter().map(|table| Seals::decode(table.record, &mut body)).collect();
        let maps = schema.maps().iter().map(|_| decode_list(&mut body)).collect();
        body.finish();
        Self { durable, sequences, maps }
    }

    /// Every file this body seals, by path under the index directory
    fn files(&self, schema: &Schema) -> Vec<(String, Sealed)> {
        let sequences = schema
            .sequences()
            .iter()
            .zip(&self.sequences)
            .flat_map(|(table, seals)| sequence::files(table, seals));
        let maps =
            schema.maps().iter().zip(&self.maps).flat_map(|(table, list)| lsm::files(table, list));
        sequences.chain(maps).collect()
    }
}

fn identity(schema: &Schema) -> Identity {
    Identity { name: schema.name(), format: schema.format, scope: schema.scope() }
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
            Some(bytes) => Body::decode(bytes, schema),
            None => {
                for table in schema.sequences() {
                    for name in sequence::names(table) {
                        dir.ensure_empty(&name, Depth::Shallow)?;
                    }
                }
                for table in schema.maps() {
                    dir.ensure_empty(table.name, Depth::Recursive)?;
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
        // the files a `Barrier` commit writes back: every sequence's, checksums included (each
        // exists now)
        let written_back: Vec<PathBuf> = schema
            .sequences()
            .iter()
            .flat_map(sequence::names)
            .map(|name| dir.path().join(name))
            .collect();
        let durability = self.fs.durability(&written_back);
        tracing::info!(store = schema.name(), path = %path.display(), ?durability, "Commits");
        let map_dirs: Vec<PathBuf> =
            schema.maps().iter().map(|table| dir.subdir(table.name)).collect::<io::Result<_>>()?;
        if opened.body.is_none() {
            dir.commit(&body.encode())?;
        }

        let mut maps = Vec::with_capacity(schema.maps().len());
        for ((table, list), map_dir) in schema.maps().iter().zip(&body.maps).zip(&map_dirs) {
            let fs = Arc::clone(&self.fs);
            let slots = Arc::clone(&self.slots);
            let set =
                SegmentSet::open(fs, map_dir, schema.name(), table, list, self.fanout, slots)?;
            maps.push(set);
        }
        let state = State {
            schema: *schema,
            tip: body.durable.tip,
            sequences: sequences.iter().map(|file| file.pages(None)).collect::<io::Result<_>>()?,
            maps: maps.iter().map(|set| Arc::clone(set.segments_view())).collect(),
        };
        Ok(DiskStore {
            dir,
            schema: *schema,
            sequences,
            maps,
            view: DiskView { state: Arc::new(state) },
            buffer: WriteBuffer::empty(schema),
            write_buffer,
            durability,
            failed: false,
        })
    }

    /// Read only, no lock: safe beside a live writer
    fn verify(&self, path: &Path, schema: &Schema) -> Result<Verification, StoreError> {
        let fs = self.fs.as_ref();
        verify_durable(fs, path, schema, || index_dir::read_durable(fs, path, identity(schema)))
    }
}

/// Every file the manifest `read_manifest` returns names, scrubbed
///
/// - live writer's merge may retire a listed segment mid-scrub: lost file + manifest moved since
///   = scrub again against the newer one
fn verify_durable(
    fs: &dyn Fs,
    path: &Path,
    schema: &Schema,
    mut read_manifest: impl FnMut() -> Result<Option<Vec<u8>>, ManifestError>,
) -> Result<Verification, StoreError> {
    let mut manifest = read_manifest()?;
    loop {
        let Some(bytes) = &manifest else {
            return Ok(Verification { heights: 0, units: Vec::new() });
        };
        let body = Body::decode(bytes, schema);
        let units = body
            .files(schema)
            .into_iter()
            .map(|(name, sealed)| scrub(fs, path, &name, sealed))
            .collect::<Result<Vec<_>, PageError>>()?;
        if units.iter().any(|unit| unit.lost) {
            let newer = read_manifest()?;
            if newer != manifest {
                manifest = newer;
                continue;
            }
        }
        return Ok(Verification { heights: body.durable.count(), units });
    }
}

impl DiskStore {
    /// Heap the buffer holds (≈ RAM, >= its item bytes)
    ///
    /// - `write_buffer` of it = a commit
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.heap()
    }

    /// Panics after a failed commit (fsync errors are never retried: recovery = reopen)
    fn assert_unpoisoned(&self, call: &str) {
        assert!(!self.failed, "{call} on a store a failed commit poisoned (reopen it)");
    }

    /// WriteBuffer written + sealed, every table in parallel → (`Barrier`) one device flush →
    /// manifest at `tip` → new view
    fn write(&mut self, tip: BlockRef) -> Result<(), StoreError> {
        let (schema, buffer, durability) = (&self.schema, &self.buffer, self.durability);
        let (sequences, maps) = rayon::join(
            || write_sequences(&mut self.sequences, schema, buffer, durability),
            || write_maps(&mut self.maps, schema, buffer),
        );
        let (sequences, grown): (Vec<Seals>, Vec<bool>) = sequences?.into_iter().unzip();
        let maps = maps?;
        // written back, not durable: flushed through a file this commit wrote, before the
        // manifest names any of them
        if let (Durability::Barrier, Some(at)) = (durability, grown.iter().position(|&g| g)) {
            self.sequences[at].flush()?;
        }

        let body = Body { durable: DurableTip { tip: Some(tip) }, sequences, maps };
        self.dir.commit(&body.encode())?;
        for set in &mut self.maps {
            set.on_manifest_durable()?;
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
            maps: self.maps.iter().map(|set| Arc::clone(set.segments_view())).collect(),
        };
        self.view = DiskView { state: Arc::new(state) };
        Ok(())
    }
}

/// Each sequence's buffered records appended, then sealed per `durability` (one table per
/// thread) → its seals + whether it grew
fn write_sequences(
    files: &mut [SequenceFile],
    schema: &Schema,
    buffer: &WriteBuffer,
    durability: Durability,
) -> Result<Vec<(Seals, bool)>, StoreError> {
    let tables = files.par_iter_mut().zip(schema.sequences());
    let sealed = tables.map(|(file, &table)| {
        for record in buffer.records(table) {
            file.append(record)?;
        }
        Ok(file.seal(durability)?)
    });
    sealed.collect()
}

/// Each map's buffered rows as one segment (one table per thread)
fn write_maps(
    sets: &mut [SegmentSet],
    schema: &Schema,
    buffer: &WriteBuffer,
) -> Result<Vec<Vec<SegmentMeta>>, StoreError> {
    let tables = sets.par_iter_mut().zip(schema.maps());
    tables.map(|(set, &table)| Ok(set.batch(buffer.map_rows(table))?)).collect()
}

impl Store for DiskStore {
    type View = DiskView;

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn apply(&mut self, changes: BlockChanges) {
        self.assert_unpoisoned("apply");
        self.buffer.assert_accepts(&changes, self.view.tip());
        self.buffer.push(&changes);
        if self.buffer.heap() >= self.write_buffer.get() {
            if let Err(failed) = self.commit() {
                failed.crash();
            }
        }
    }

    fn commit(&mut self) -> Result<(), CommitFailed> {
        self.assert_unpoisoned("commit");
        let Some(tip) = self.buffer.tip() else { return Ok(()) };
        let (bytes, started) = (self.buffer.item_bytes(), Instant::now());
        if let Err(cause) = self.write(tip) {
            self.failed = true;
            return Err(CommitFailed::new(self.schema.name(), self.dir.path(), cause));
        }
        crate::metrics::committed(self.schema.name(), bytes, started.elapsed());
        self.buffer = WriteBuffer::empty(&self.schema);
        Ok(())
    }

    fn durable(&self) -> DiskView {
        self.view.clone()
    }

    fn applied(&self) -> AppliedView<'_, DiskView> {
        OverlayView::new(self.view.clone(), &self.buffer)
    }
}

/// One table's durable files: bytes on disk, bytes of them in page cache (`None` = not
/// knowable: a simulated filesystem, or the kernel refused)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableFootprint {
    pub table: &'static str,
    pub bytes: u64,
    pub cached: Option<u64>,
}

impl DiskView {
    /// Each table's [`TableFootprint`], schema order (sequences, then maps)
    ///
    /// - one `mincore` pass over every mapped file: ~1 byte per 4 KiB page, no I/O
    pub fn footprint(&self) -> Vec<TableFootprint> {
        let schema = &self.state.schema;
        let sequences = schema.sequences().iter().zip(&self.state.sequences);
        let sequences = sequences.map(|(table, pages)| (table.name, footprint(pages.pages())));
        let maps = schema.maps().iter().zip(&self.state.maps);
        let maps = maps.map(|(table, segments)| (table.name, footprint(segments.pages())));
        let tables = sequences.chain(maps);
        tables.map(|(table, (bytes, cached))| TableFootprint { table, bytes, cached }).collect()
    }

    // names != the read traits' `sequence` / `map` (inherent methods win resolution, even private)
    fn sequence_pages(&self, table: SequenceId) -> &SequencePages {
        table_slot(&self.state.sequences, table)
    }

    fn map_segments(&self, table: MapId) -> &SegmentsView {
        table_slot(&self.state.maps, table).as_ref()
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

impl PinnedView for DiskView {}

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
        self.map_segments(table).get(key)
    }

    fn values(&self, table: MapId, keys: &[&[u8]]) -> Vec<Option<Bytes>> {
        self.map_segments(table).get_many(keys)
    }

    fn range(
        &self,
        table: MapId,
        start: &[u8],
        end: &[u8],
        limit: usize,
    ) -> Option<Vec<(Bytes, Bytes)>> {
        self.map_segments(table).range(start, end, limit)
    }
}

#[cfg(test)]
mod model;
#[cfg(test)]
mod model_tests;
#[cfg(test)]
mod tests;
