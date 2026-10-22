//! The on-disk engine: sequences as positional files, maps as LSM segment sets, one manifest
//!
//! ```text
//! <dir>/
//!   MANIFEST            namespaces ‖ committed tip ‖ each namespace's seals or segment list
//!   <sequence>.dat      (+ <sequence>.idx when Variable)        `sequence.rs`
//!   <map>/<id>.seg      one sorted segment per batch or merge   `lsm`
//! ```
//!
//! - commits → [`Buffer`] (RAM) → [`Engine::write`]: every namespace written + sealed in
//!   parallel → manifest (commit point) → readable
//! - a failed write poisons the backend (`docs/durability.md` §5): recovery = reopen

use std::{path::Path, sync::Arc, time::Instant};

use rayon::prelude::*;
use zaino_persistence::{
    Namespace, NamespaceFootprint, NamespaceKind, NamespaceSpec, Unit, Verification, Width,
};
use zaino_primitives::types::BlockRef;

use crate::{
    buffer::{Buffer, Row},
    dir::{IndexDir, MANIFEST},
    error::StoreError,
    fs::Fs,
    lsm::{
        decode_list, encode_list, file_name, report_leftover, SegmentLog, SegmentMeta, Shape,
        Slots, Snapshot, Staged,
    },
    lsm::{LsmConfig, Tuning},
    manifest::{self, BodyReader, Committed, Identity, ManifestError},
    pages::{footprint, scrub, Sealed},
    sequence::{self, Seals, SequenceFile, SequencePages},
};

/// Names the directory reserves for itself (never a namespace path component)
const RESERVED: [&str; 3] = [MANIFEST, "LOCK", "MANIFEST.creating"];

/// Index of `namespace` in `specs` (`None` = undeclared)
pub(crate) fn position(specs: &[NamespaceSpec], namespace: Namespace) -> Option<usize> {
    specs.iter().position(|spec| spec.namespace == namespace)
}

/// Every name a safe path under the directory on any filesystem, no map's directory holding
/// another namespace
///
/// - `/`-separated parts of `[A-Za-z0-9_-]`, at most 255 bytes, none reserved
/// - compared ignoring case (case-insensitive filesystems: `Blocks` = `blocks`)
/// - a map owns its directory: no namespace's path inside it
pub(crate) fn check_names(specs: &[NamespaceSpec]) -> Result<(), StoreError> {
    let refused = |spec: &NamespaceSpec, reason| StoreError::Declared {
        name: spec.namespace.to_string(),
        reason,
    };
    let folded: Vec<String> = specs
        .iter()
        .map(|spec| spec.namespace.as_str().to_ascii_lowercase())
        .collect();
    let mut seen = std::collections::HashSet::with_capacity(folded.len());
    for (spec, name) in specs.iter().zip(&folded) {
        if name.len() > usize::from(u8::MAX) {
            return Err(refused(spec, "a name past 255 bytes"));
        }
        let parts_safe = spec.namespace.as_str().split('/').all(|part| {
            !part.is_empty()
                && !RESERVED
                    .iter()
                    .any(|reserved| reserved.eq_ignore_ascii_case(part))
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        });
        if !parts_safe {
            return Err(refused(
                spec,
                "a name not of `/`-separated [A-Za-z0-9_-] parts, or a reserved one",
            ));
        }
        if !seen.insert(name.as_str()) {
            return Err(refused(spec, "a name equal to another ignoring case"));
        }
    }
    for (map, owned) in specs
        .iter()
        .zip(&folded)
        .filter(|(spec, _)| matches!(spec.kind, NamespaceKind::Map { .. }))
    {
        let inside = folded.iter().any(|other| {
            other
                .strip_prefix(owned.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        });
        if inside {
            return Err(refused(
                map,
                "a map's directory nested with another namespace's path",
            ));
        }
    }
    Ok(())
}

/// One namespace's write side
#[derive(Debug)]
enum Files {
    Sequence(SequenceFile),
    Map(SegmentLog),
}

/// Index directory's writer (holds its `LOCK`)
#[derive(Debug)]
pub(crate) struct Engine {
    dir: IndexDir,
    name: String,
    specs: Arc<[NamespaceSpec]>,
    files: Vec<Files>,
    view: DiskView,
    alert_bytes: u64,
}

/// Committed state: what readers read below the buffer
#[derive(Debug, Clone)]
pub(crate) struct DiskView {
    tip: Option<BlockRef>,
    held: Vec<Durable>,
}

/// One namespace's committed read side
#[derive(Debug, Clone)]
pub(crate) enum Durable {
    Sequence(SequencePages),
    Map(Arc<Snapshot>),
}

/// What one namespace's files hold at a commit
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stored {
    Sequence(Seals),
    Map(Vec<SegmentMeta>),
}

/// Manifest body: what one commit made durable
#[derive(Debug, Clone, PartialEq, Eq)]
struct Body {
    committed: Committed,
    stored: Vec<Stored>,
}

impl Body {
    fn empty(specs: &[NamespaceSpec]) -> Self {
        let stored = specs
            .iter()
            .map(|spec| match spec.kind {
                NamespaceKind::Sequence { .. } => Stored::Sequence(Seals::EMPTY),
                NamespaceKind::Map { .. } => Stored::Map(Vec::new()),
            })
            .collect();
        Self {
            committed: Committed::EMPTY,
            stored,
        }
    }

    /// `namespaces ‖ committed ‖ per namespace: its seals or segment list`, declaration order
    fn encode(&self, specs: &[NamespaceSpec]) -> Result<Vec<u8>, StoreError> {
        let mut out = Vec::new();
        encode_specs(specs, &mut out)?;
        self.committed.encode(&mut out);
        for (spec, stored) in specs.iter().zip(&self.stored) {
            match (spec.kind, stored) {
                (NamespaceKind::Sequence { record }, Stored::Sequence(seals)) => {
                    seals.encode(record, &mut out)
                }
                (NamespaceKind::Map { .. }, Stored::Map(list)) => encode_list(list, &mut out)?,
                _ => return Err(StoreError::Internal("a body entry of another kind")),
            }
        }
        Ok(out)
    }

    /// Body committed with exactly `specs` (else `Specs`)
    fn decode(bytes: &[u8], specs: &[NamespaceSpec]) -> Result<Self, StoreError> {
        let mut body = BodyReader::new(bytes);
        let stored = decode_specs(&mut body)?;
        let declared: Vec<(&str, NamespaceKind)> = specs
            .iter()
            .map(|spec| (spec.namespace.as_str(), spec.kind))
            .collect();
        let same = stored.len() == declared.len()
            && stored
                .iter()
                .zip(&declared)
                .all(|((name, kind), (other, other_kind))| name == other && kind == other_kind);
        if !same {
            return Err(StoreError::Specs(format!(
                "stored {stored:?}, declared {declared:?}"
            )));
        }
        let committed = Committed::decode(&mut body)?;
        let stored = specs
            .iter()
            .map(|spec| match spec.kind {
                NamespaceKind::Sequence { record } => {
                    Seals::decode(record, &mut body).map(Stored::Sequence)
                }
                NamespaceKind::Map { .. } => decode_list(&mut body).map(Stored::Map),
            })
            .collect::<Result<_, _>>()?;
        body.finish()?;
        Ok(Self { committed, stored })
    }

    /// Every file this body seals, by path under the index directory
    fn files(&self, specs: &[NamespaceSpec]) -> Vec<(String, Sealed)> {
        specs
            .iter()
            .zip(&self.stored)
            .flat_map(|(spec, stored)| {
                let name = spec.namespace.as_str();
                match (spec.kind, stored) {
                    (NamespaceKind::Sequence { record }, Stored::Sequence(seals)) => {
                        sequence::files(name, record, seals)
                    }
                    (_, Stored::Map(list)) => list
                        .iter()
                        .map(|segment| {
                            (format!("{name}/{}", file_name(segment.id)), segment.sealed)
                        })
                        .collect(),
                    (NamespaceKind::Map { .. }, Stored::Sequence(_)) => Vec::new(),
                }
            })
            .collect()
    }
}

/// `count u32 ‖ (name_len u8 ‖ name ‖ kind)*`: sequence `0 ‖ width u32` (0 = variable), map
/// `1 ‖ key u32 ‖ value u32 ‖ deletes u8`
fn encode_specs(specs: &[NamespaceSpec], out: &mut Vec<u8>) -> Result<(), StoreError> {
    let count = u32::try_from(specs.len()).map_err(|_| StoreError::Overflow("namespace count"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for spec in specs {
        let name = spec.namespace.as_str();
        let len = u8::try_from(name.len()).map_err(|_| StoreError::Declared {
            name: name.to_owned(),
            reason: "a name past 255 bytes",
        })?;
        out.push(len);
        out.extend_from_slice(name.as_bytes());
        match spec.kind {
            NamespaceKind::Sequence { record } => {
                out.push(0);
                let width = match record {
                    Width::Fixed(n) => n.get(),
                    Width::Variable => 0,
                };
                out.extend_from_slice(&width.to_le_bytes());
            }
            NamespaceKind::Map {
                key,
                value,
                deletes,
            } => {
                out.push(1);
                out.extend_from_slice(&key.get().to_le_bytes());
                out.extend_from_slice(&value.get().to_le_bytes());
                out.push(u8::from(deletes));
            }
        }
    }
    Ok(())
}

/// The namespaces a body was committed with
fn decode_specs(body: &mut BodyReader<'_>) -> Result<Vec<(String, NamespaceKind)>, ManifestError> {
    let malformed = || ManifestError::Body("malformed namespace declaration");
    let width = |raw: u32| std::num::NonZeroU32::new(raw).ok_or_else(malformed);
    let count = body.u32()?;
    let mut specs = Vec::new();
    for _ in 0..count {
        let [len] = body.array::<1>()?;
        let name =
            String::from_utf8(body.take(usize::from(len))?.to_vec()).map_err(|_| malformed())?;
        let kind = match body.array::<1>()? {
            [0] => NamespaceKind::Sequence {
                record: match body.u32()? {
                    0 => Width::Variable,
                    n => Width::Fixed(width(n)?),
                },
            },
            [1] => NamespaceKind::Map {
                key: width(body.u32()?)?,
                value: width(body.u32()?)?,
                deletes: match body.array::<1>()? {
                    [0] => false,
                    [1] => true,
                    _ => return Err(malformed()),
                },
            },
            _ => return Err(malformed()),
        };
        specs.push((name, kind));
    }
    Ok(specs)
}

/// Directory a namespace's files sit in (`a/b` → `a`), `None` = the index directory itself
fn parent(name: &str) -> Option<&str> {
    name.rsplit_once('/').map(|(parent, _)| parent)
}

impl Engine {
    /// `path` opened as index `identity` holding `specs` (each with its `tunings` entry, same
    /// order); a fresh directory committed empty
    pub(crate) fn open(
        fs: &Arc<dyn Fs>,
        path: &Path,
        identity: Identity,
        specs: Arc<[NamespaceSpec]>,
        tunings: &[Tuning],
        lsm: LsmConfig,
    ) -> Result<Self, StoreError> {
        check_names(&specs)?;
        let name = identity.name().to_owned();
        let opened = IndexDir::open(Arc::clone(fs), path, identity)?;
        let mut dir = opened.dir;
        let body = match &opened.body {
            Some(bytes) => Body::decode(bytes, &specs)?,
            None => {
                for spec in specs.iter() {
                    let name = spec.namespace.as_str();
                    match spec.kind {
                        NamespaceKind::Sequence { record } => {
                            for file in SequenceFile::names(name, record) {
                                dir.ensure_empty(&file)?;
                            }
                        }
                        NamespaceKind::Map { .. } => dir.ensure_empty_dir(name)?,
                    }
                }
                Body::empty(&specs)
            }
        };

        let bytes_per_sec = u64::from(lsm.merge_mib_per_sec.get()) << 20;
        let slots = Arc::new(Slots::new(lsm.merge_slots.get(), bytes_per_sec));
        let mut files = Vec::with_capacity(specs.len());
        for ((spec, stored), tuning) in specs.iter().zip(&body.stored).zip(tunings) {
            let name = spec.namespace.as_str();
            files.push(match (spec.kind, stored) {
                (NamespaceKind::Sequence { record }, Stored::Sequence(seals)) => {
                    if let Some(parent) = parent(name) {
                        dir.subdir(parent)?;
                    }
                    let file = SequenceFile::open(
                        fs.as_ref(),
                        dir.path(),
                        name,
                        record,
                        tuning.cache_writes,
                        *seals,
                    )?;
                    Files::Sequence(file)
                }
                (
                    NamespaceKind::Map {
                        key,
                        value,
                        deletes,
                    },
                    Stored::Map(list),
                ) => {
                    let map_dir = dir.subdir(name)?;
                    let shape = Shape::of(name, key, value, deletes, *tuning, lsm.spill_bytes)?;
                    let fs = Arc::clone(fs);
                    let slots = Arc::clone(&slots);
                    let log = SegmentLog::open(fs, &map_dir, name, shape, list, &lsm, slots)?;
                    Files::Map(log)
                }
                _ => return Err(StoreError::Internal("a body entry of another kind")),
            });
        }
        if opened.body.is_none() {
            dir.commit(&body.encode(&specs)?)?;
        }

        // a changed scope: rewritten segments committed in the old ones' place before serving
        let mut body = body;
        let mut rescoped = Vec::new();
        for (index, (files, stored)) in files.iter_mut().zip(&mut body.stored).enumerate() {
            if let (Files::Map(log), Stored::Map(list)) = (files, stored) {
                if let Some(staged) = log.rescope()? {
                    *list = staged.segments().to_vec();
                    rescoped.push((index, staged));
                }
            }
        }
        if !rescoped.is_empty() {
            dir.commit(&body.encode(&specs)?)?;
            for (index, staged) in rescoped {
                if let Some(Files::Map(log)) = files.get_mut(index) {
                    log.committed(staged);
                }
            }
            report_leftovers(&files, lsm.cleanup_alert_bytes);
        }
        let view = DiskView {
            tip: body.committed.tip,
            held: read_side(&files, None)?,
        };
        Ok(Self {
            dir,
            name,
            specs,
            files,
            view,
            alert_bytes: lsm.cleanup_alert_bytes,
        })
    }

    pub(crate) fn view(&self) -> &DiskView {
        &self.view
    }

    /// `buffer` written + sealed and the next view mapped, every namespace in parallel →
    /// manifest at its tip (the commit point) → new view swapped in (an empty buffer writes
    /// nothing)
    ///
    /// - every step that can fail runs before the commit point: an error = nothing committed
    pub(crate) fn write(&mut self, buffer: &Buffer) -> Result<(), StoreError> {
        let Some(tip) = buffer.tip() else {
            return Ok(());
        };
        let (bytes, started) = (buffer.item_bytes(), Instant::now());
        // finished merges joined before any file moves: a failed one fails this write with
        // nothing applied (`MergeFailed`)
        let mut rows: Vec<Option<MapRows<'_>>> = Vec::with_capacity(self.files.len());
        for (index, files) in self.files.iter_mut().enumerate() {
            rows.push(match files {
                Files::Map(log) => {
                    let (rows, cancelled) = buffer.map_rows(index);
                    let upcoming: Vec<u64> = [rows.len(), cancelled.len()]
                        .into_iter()
                        .filter(|count| *count > 0)
                        .map(|count| count as u64)
                        .collect();
                    log.prepare(&upcoming)?;
                    Some((rows, cancelled))
                }
                Files::Sequence(_) => None,
            });
        }

        let view = &self.view;
        let prepared = self
            .files
            .par_iter_mut()
            .zip(rows)
            .enumerate()
            .map(|(index, (files, rows))| match (files, rows) {
                (Files::Sequence(file), _) => {
                    for record in buffer.records(index).into_iter().flatten() {
                        file.append(record)?;
                    }
                    let seals = file.seal()?;
                    let old = match view.held.get(index) {
                        Some(Durable::Sequence(pages)) => Some(pages),
                        _ => None,
                    };
                    Ok(Prepared::Sequence(seals, file.pages(old)?))
                }
                (Files::Map(log), Some((rows, cancelled))) => {
                    Ok(Prepared::Map(log.batch(rows, cancelled)?))
                }
                (Files::Map(_), None) => Err(StoreError::Internal("a map without its rows")),
            })
            .collect::<Result<Vec<_>, StoreError>>()?;

        let body = Body {
            committed: Committed { tip: Some(tip) },
            stored: prepared.iter().map(Prepared::stored).collect(),
        };
        let held = prepared.iter().map(Prepared::durable).collect();
        self.dir.commit(&body.encode(&self.specs)?)?;

        for (files, prepared) in self.files.iter_mut().zip(prepared) {
            if let (Files::Map(log), Prepared::Map(staged)) = (files, prepared) {
                log.committed(staged);
            }
        }
        report_leftovers(&self.files, self.alert_bytes);
        self.view = DiskView {
            tip: Some(tip),
            held,
        };
        crate::emit::committed(&self.name, bytes, started.elapsed());
        Ok(())
    }
}

/// A map's rows to write and the cancelled keys whose tombstones follow them
type MapRows<'a> = (Vec<Row<'a>>, Vec<&'a [u8]>);

/// Every map's leftover files reported together: a warning, an error from `alert_bytes` on
/// (the disk may be failing)
fn report_leftovers(files: &[Files], alert_bytes: u64) {
    let maps = files.iter().filter_map(|files| match files {
        Files::Map(log) => Some(log.leftover()),
        Files::Sequence(_) => None,
    });
    if let Some((count, bytes, error)) = leftover_total(maps) {
        report_leftover(count, bytes, error, bytes >= alert_bytes);
    }
}

/// Every map's leftover `(files, bytes, last error)` summed; `None` = nothing left over
fn leftover_total<'a>(
    maps: impl Iterator<Item = (usize, u64, Option<&'a str>)>,
) -> Option<(usize, u64, &'a str)> {
    let (mut count, mut bytes, mut failure) = (0usize, 0u64, None);
    for (files, held, error) in maps {
        count = count.saturating_add(files);
        bytes = bytes.saturating_add(held);
        failure = failure.or(error);
    }
    failure
        .filter(|_| count > 0)
        .map(|error| (count, bytes, error))
}

/// One namespace written and sealed, its next read side mapped: awaiting the commit point
enum Prepared {
    Sequence(Seals, SequencePages),
    Map(Staged),
}

impl Prepared {
    fn stored(&self) -> Stored {
        match self {
            Self::Sequence(seals, _) => Stored::Sequence(*seals),
            Self::Map(staged) => Stored::Map(staged.segments().to_vec()),
        }
    }

    fn durable(&self) -> Durable {
        match self {
            Self::Sequence(_, pages) => Durable::Sequence(pages.clone()),
            Self::Map(staged) => Durable::Map(Arc::clone(staged.snapshot())),
        }
    }
}

/// Each namespace's read side at its last seal or commit (`previous`'s checked pages kept)
fn read_side(files: &[Files], previous: Option<&DiskView>) -> Result<Vec<Durable>, StoreError> {
    files
        .iter()
        .enumerate()
        .map(|(index, files)| match files {
            Files::Sequence(file) => {
                let old =
                    previous
                        .and_then(|view| view.held.get(index))
                        .and_then(|old| match old {
                            Durable::Sequence(pages) => Some(pages),
                            Durable::Map(_) => None,
                        });
                Ok(Durable::Sequence(file.pages(old)?))
            }
            Files::Map(log) => Ok(Durable::Map(Arc::clone(log.snapshot()))),
        })
        .collect()
}

impl DiskView {
    pub(crate) fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    /// Namespace `index`'s committed read side
    pub(crate) fn held(&self, index: usize) -> Option<&Durable> {
        self.held.get(index)
    }

    /// Each namespace's committed bytes and those in page cache, declaration order
    ///
    /// - one `mincore` pass over every mapped file: ~1 byte per 4 KiB page, no I/O
    pub(crate) fn footprint(&self, specs: &[NamespaceSpec]) -> Vec<NamespaceFootprint> {
        specs
            .iter()
            .zip(&self.held)
            .map(|(spec, held)| {
                let (bytes, cached) = match held {
                    Durable::Sequence(pages) => footprint(pages.pages()),
                    Durable::Map(snapshot) => footprint(snapshot.pages()),
                };
                NamespaceFootprint {
                    namespace: spec.namespace,
                    bytes,
                    cached,
                }
            })
            .collect()
    }
}

/// Every file the directory's committed manifest names, scrubbed (read only, no lock: safe beside
/// a live writer)
pub(crate) fn verify(
    fs: &dyn Fs,
    path: &Path,
    identity: &Identity,
    specs: &[NamespaceSpec],
) -> Result<Verification, StoreError> {
    verify_committed(fs, path, specs, || Ok(manifest::read(fs, path, identity)?))
}

/// Every file the manifest `committed` reads names, scrubbed
///
/// - live writer's merge may retire a listed segment mid-scrub: lost file + manifest moved since
///   = scrub again against the newer one
fn verify_committed(
    fs: &dyn Fs,
    path: &Path,
    specs: &[NamespaceSpec],
    mut committed: impl FnMut() -> Result<Option<Vec<u8>>, StoreError>,
) -> Result<Verification, StoreError> {
    let mut manifest = committed()?;
    loop {
        let Some(bytes) = &manifest else {
            return Ok(Verification::default());
        };
        let body = Body::decode(bytes, specs)?;
        let checked = body
            .files(specs)
            .into_iter()
            .map(|(name, sealed)| scrub(fs, path, &name, sealed))
            .collect::<Result<Vec<_>, _>>()?;
        if checked.iter().any(|unit| unit.lost.is_some()) {
            let newer = committed()?;
            if newer != manifest {
                manifest = newer;
                continue;
            }
        }
        let units: Vec<Unit> = checked.into_iter().map(|unit| unit.into_unit()).collect();
        return Ok(Verification { units });
    }
}

#[cfg(test)]
mod tests;
