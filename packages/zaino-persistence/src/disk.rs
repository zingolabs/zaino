//! The engine Zaino runs: sequences as positional files, maps as LSM segment sets, one manifest
//!
//! ```text
//! <dir>/
//!   MANIFEST            committed tip ‖ each sequence's seals ‖ each map's segment list
//!   <sequence>.dat      (+ <sequence>.idx when Variable)        `sequence.rs`
//!   <map>/<id>.seg      one sorted segment per batch or merge   `lsm`
//! ```
//!
//! - commit = append + seal every table, then the manifest (the commit point), then readable
//! - a failed commit poisons the store (`docs/design/durability.md` §6): recovery = reopen

use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use zaino_primitives::types::BlockRef;

use crate::{
    dir::IndexDir,
    fs::Fs,
    lsm::{decode_list, encode_list, file_name, SegmentLog, SegmentMeta, Snapshot},
    manifest::{self, BodyReader, Committed, Identity, ManifestError},
    pages::{scrub, Sealed},
    port::{
        Changes, MapId, MapRead, PersistenceEngine, Schema, SequenceId, SequenceRead, Store,
        Verification, View,
    },
    sequence::{self, Seals, SequenceFile, SequencePages},
    StoreError,
};

/// Same-tier segments merged into one (write amplification `log_FANOUT(rows)`)
const FANOUT: usize = 8;

/// Files for sequences, LSM for maps, on one filesystem
#[derive(Debug, Clone)]
pub struct DiskEngine {
    fs: Arc<dyn Fs>,
    fanout: usize,
}

impl DiskEngine {
    pub fn new(fs: Arc<dyn Fs>) -> Self {
        Self { fs, fanout: FANOUT }
    }

    /// Merges after `fanout` same-tier segments (small = merges in a handful of commits)
    #[cfg(any(test, feature = "testing"))]
    pub fn with_fanout(fs: Arc<dyn Fs>, fanout: usize) -> Self {
        Self { fs, fanout }
    }
}

/// One index directory's writer (holds its `LOCK`)
#[derive(Debug)]
pub struct DiskStore {
    dir: IndexDir,
    schema: Schema,
    tip: Option<BlockRef>,
    sequences: Vec<SequenceFile>,
    maps: Vec<SegmentLog>,
    view: DiskView,
    failed: bool,
}

/// One committed state of a [`DiskStore`] (a clone shares it)
#[derive(Debug, Clone)]
pub struct DiskView {
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    tip: Option<BlockRef>,
    sequences: Vec<SequencePages>,
    maps: Vec<Arc<Snapshot>>,
}

/// The manifest body: what one commit made durable
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
            sequences: vec![Seals::EMPTY; schema.sequences.len()],
            maps: vec![Vec::new(); schema.maps.len()],
        }
    }

    /// `committed ‖ seals per sequence ‖ segment list per map`, in schema order
    fn encode(&self, schema: &Schema) -> Vec<u8> {
        let mut out = Vec::new();
        self.committed.encode(&mut out);
        for (table, seals) in schema.sequences.iter().zip(&self.sequences) {
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
            .sequences
            .iter()
            .map(|table| Seals::decode(table.record, &mut body))
            .collect::<Result<_, _>>()?;
        let maps = schema.maps.iter().map(|_| decode_list(&mut body)).collect::<Result<_, _>>()?;
        body.finish()?;
        Ok(Self { committed, sequences, maps })
    }

    /// Every file this body seals, by path under the index directory
    fn files(&self, schema: &Schema) -> Vec<(String, Sealed)> {
        let sequences = schema
            .sequences
            .iter()
            .zip(&self.sequences)
            .flat_map(|(table, seals)| sequence::files(table, seals));
        let maps = schema.maps.iter().zip(&self.maps).flat_map(|(table, list)| {
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

    fn open(&self, path: &Path, schema: &Schema) -> Result<DiskStore, StoreError> {
        let opened = IndexDir::open(Arc::clone(&self.fs), path, identity(schema))?;
        let mut dir = opened.dir;
        let body = match &opened.body {
            Some(bytes) => Body::decode(bytes, schema)?,
            None => {
                for table in &schema.sequences {
                    for name in SequenceFile::names(table) {
                        dir.ensure_empty(&name)?;
                    }
                }
                for table in &schema.maps {
                    dir.ensure_empty_dir(&table.name)?;
                }
                Body::empty(schema)
            }
        };

        for parent in schema.sequences.iter().filter_map(|table| parent(&table.name)) {
            dir.subdir(parent)?;
        }
        let sequences = schema
            .sequences
            .iter()
            .zip(&body.sequences)
            .map(|(table, seals)| SequenceFile::open(self.fs.as_ref(), dir.path(), table, *seals))
            .collect::<Result<Vec<_>, _>>()?;
        let map_dirs: Vec<PathBuf> =
            schema.maps.iter().map(|table| dir.subdir(&table.name)).collect::<io::Result<_>>()?;
        if opened.body.is_none() {
            dir.commit(&body.encode(schema))?;
        }

        let mut maps = Vec::with_capacity(schema.maps.len());
        for ((table, list), map_dir) in schema.maps.iter().zip(&body.maps).zip(&map_dirs) {
            let fs = Arc::clone(&self.fs);
            maps.push(SegmentLog::open(fs, map_dir, table, list, self.fanout)?);
        }
        let state = State {
            tip: body.committed.tip,
            sequences: sequences.iter().map(|file| file.pages(None)).collect::<io::Result<_>>()?,
            maps: maps.iter().map(|log| Arc::clone(log.snapshot())).collect(),
        };
        Ok(DiskStore {
            dir,
            schema: schema.clone(),
            tip: body.committed.tip,
            sequences,
            maps,
            view: DiskView { state: Arc::new(state) },
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
/// - a live writer's merge may retire a listed segment mid-scrub: a lost file under a manifest
///   that moved since = scrub again against the newer one
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
    /// Blocks until every running merge has finished (lands on the next commit)
    #[cfg(any(test, feature = "testing"))]
    pub fn settle(&self) {
        for log in &self.maps {
            log.settle();
        }
    }

    /// Appends + segments written and sealed, the manifest, then the new view
    fn write(&mut self, changes: &Changes) -> Result<DiskView, StoreError> {
        for (table, file) in self.schema.sequence_ids().zip(&mut self.sequences) {
            for record in changes.appends(table) {
                file.append(record)?;
            }
        }
        let mut lists = Vec::with_capacity(self.maps.len());
        for (table, log) in self.schema.map_ids().zip(&mut self.maps) {
            lists.push(log.batch(changes.inserts(table).collect())?);
        }
        let sequences =
            self.sequences.iter_mut().map(SequenceFile::seal).collect::<io::Result<_>>()?;

        let body =
            Body { committed: Committed { tip: Some(changes.tip()) }, sequences, maps: lists };
        self.dir.commit(&body.encode(&self.schema))?;
        self.tip = Some(changes.tip());
        for log in &mut self.maps {
            log.committed()?;
        }

        let previous = &self.view.state;
        let state = State {
            tip: self.tip,
            sequences: self
                .sequences
                .iter()
                .zip(&previous.sequences)
                .map(|(file, old)| file.pages(Some(old)))
                .collect::<io::Result<_>>()?,
            maps: self.maps.iter().map(|log| Arc::clone(log.snapshot())).collect(),
        };
        self.view = DiskView { state: Arc::new(state) };
        Ok(self.view.clone())
    }
}

impl Store for DiskStore {
    type View = DiskView;

    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn view(&self) -> DiskView {
        self.view.clone()
    }

    fn commit(&mut self, changes: Changes) -> Result<DiskView, StoreError> {
        assert!(!self.failed, "commit after a failed one (fsync errors are never retried)");
        assert_eq!(changes.schema(), &self.schema, "changes built for another schema");
        let (tip, committed) = (changes.tip().height, self.tip.map(|tip| tip.height));
        assert!(
            Some(tip) > committed,
            "commit to height {tip}, not above the committed {committed:?}"
        );
        self.write(&changes).inspect_err(|_| self.failed = true)
    }
}

// names no caller's method shares (inherent methods win resolution, even private ones)
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
}

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
