//! In-memory [`Fs`] that knows what a power loss would keep
//!
//! - file content: durable bytes + writes pending since the last `sync_data`
//! - namespace: durable entries + changes pending since the parent's last `sync_dir`
//! - crash points recorded around persistence points only (just before and after each
//!   `sync_data` / `sync_dir`, after each `rename` / `remove` / `set_read_only`; CrashMonkey:
//!   bugs surface there)
//! - `Image::crash_states` = what a crash at that point could leave (ALICE model: unsynced
//!   writes dropped, prefix-applied, reordered, torn (in half, and on 512 B sector boundaries,
//!   sectors out of order, a mid-sector write's first sector garbled whole on a device without
//!   sector atomicity), zero- or garbage-filled, lost in one file while kept in the rest, several
//!   files varying at once (seeded samples); unsynced entries lost or kept, all of a directory's
//!   or some)
//! - a failed `sync_data` loses that file's pending writes for good while reads still see them
//!   (Linux: pages marked clean after a writeback error)
//! - read-only flags are kept through every crash state (chmod modelled as durable at once)
//! - [`SimFs::fail_from`] = `EIO` from the nth mutating call on (Pebble `errorfs`);
//!   `SimFs::fail_reads_from` = same for positional reads
//! - `SimFs::power_loss` / [`SimFs::restarted`] = crash / process exit at any instant

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt, io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use bytes::Bytes;

use super::{too_long, FileHandle, Fs, LockGuard, Mapping};

type FileId = usize;

/// Device's atomic write unit (torn writes tear on these boundaries)
const SECTOR: usize = 512;

/// Crash states per persistence point with several files varying at once
const MIXED_SAMPLES: usize = 4;

/// A file's crash variants: what each leaves, labelled
type Variants = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone)]
enum Op {
    Write { at: u64, bytes: Vec<u8> },
    SetLen(u64),
}

impl Op {
    fn apply(&self, data: &mut Vec<u8>) {
        match self {
            Op::Write { at, bytes } => {
                let at = usize::try_from(*at).expect("sim offsets fit usize");
                if data.len() < at + bytes.len() {
                    data.resize(at + bytes.len(), 0);
                }
                data[at..at + bytes.len()].copy_from_slice(bytes);
            }
            Op::SetLen(len) => {
                data.resize(usize::try_from(*len).expect("sim lengths fit usize"), 0)
            }
        }
    }
}

/// `durable` = what the device holds; `pending` = writes it may or may not hold yet; `current`
/// = what reads see (the page cache: `durable` with every `pending` op applied, plus any write a
/// failed writeback lost, which no later sync persists: Linux after a failed `fsync`)
#[derive(Debug, Clone, Default)]
struct Content {
    durable: Vec<u8>,
    pending: Vec<Op>,
    current: Vec<u8>,
}

impl Content {
    /// Nothing pending: what a crashed image holds
    fn settled(bytes: Vec<u8>) -> Self {
        Self {
            durable: bytes.clone(),
            pending: Vec::new(),
            current: bytes,
        }
    }

    fn push(&mut self, op: Op) {
        op.apply(&mut self.current);
        self.pending.push(op);
    }

    fn sync(&mut self) {
        self.durable = self.with(&self.pending);
        self.pending.clear();
    }

    /// A failed writeback: the pending writes never reach the device (pages marked clean, still
    /// cached: reads keep seeing them until the process exits)
    fn writeback_failed(&mut self) {
        self.pending.clear();
    }

    /// The most a crash can keep: every pending write applied (never one a failed writeback lost)
    fn kept_at_most(&self) -> Vec<u8> {
        self.with(&self.pending)
    }

    fn with(&self, ops: &[Op]) -> Vec<u8> {
        let mut data = self.durable.clone();
        for op in ops {
            op.apply(&mut data);
        }
        data
    }

    /// Every content a crash could leave, labelled
    fn crash_variants(&self) -> Vec<(String, Vec<u8>)> {
        let mut variants = Vec::new();
        for kept in 0..=self.pending.len() {
            variants.push((
                format!("{kept}/{} pending ops", self.pending.len()),
                self.with(&self.pending[..kept]),
            ));
        }
        // out-of-order writeback: one op lost, every later one kept (last lost = a prefix above)
        for lost in 0..self.pending.len().saturating_sub(1) {
            let mut kept = self.pending.clone();
            kept.remove(lost);
            variants.push((format!("op {lost} lost, later ops kept"), self.with(&kept)));
        }
        for (at, op) in self.pending.iter().enumerate() {
            let Op::Write { at: offset, bytes } = op else {
                continue;
            };
            let prior = &self.pending[..at];
            let torn = |label: &str, bytes: Vec<u8>| {
                let mut data = self.with(prior);
                Op::Write { at: *offset, bytes }.apply(&mut data);
                (format!("op {at} {label}"), data)
            };
            variants.push(torn("torn", bytes[..bytes.len() / 2].to_vec()));
            variants.push(torn("zero-filled", vec![0; bytes.len()]));
            variants.push(torn("garbage", vec![0xa5; bytes.len()]));
            // a device without sector atomicity: the sector holding a mid-sector write's start
            // garbled whole, bytes before the write included
            let start = usize::try_from(*offset).expect("sim offsets fit usize");
            if start % SECTOR != 0 {
                let sector = start / SECTOR * SECTOR;
                let end = (sector + SECTOR).min(start + bytes.len());
                let mut data = self.with(prior);
                Op::Write {
                    at: sector as u64,
                    bytes: vec![0x5a; end - sector],
                }
                .apply(&mut data);
                variants.push((format!("op {at} its first sector garbled whole"), data));
            }
            // device: sector atomicity only, sectors persisted in any order → torn at sector
            // boundaries, every other sector kept
            if bytes.len() > SECTOR {
                variants.push(torn("first sector only", bytes[..SECTOR].to_vec()));
                let last = (bytes.len() - 1) / SECTOR * SECTOR;
                variants.push(torn("all but the last sector", bytes[..last].to_vec()));
                let mut data = self.with(prior);
                for (sector, chunk) in bytes.chunks(SECTOR).enumerate().step_by(2) {
                    let at = offset + (sector * SECTOR) as u64;
                    Op::Write {
                        at,
                        bytes: chunk.to_vec(),
                    }
                    .apply(&mut data);
                }
                variants.push((format!("op {at} every other sector"), data));
            }
        }
        variants
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    Dir,
    File(FileId),
}

/// Whole filesystem state (cloned per recorded crash point)
#[derive(Debug, Clone, Default)]
struct Image {
    files: Vec<Content>,
    volatile: BTreeMap<PathBuf, Node>,
    durable: BTreeMap<PathBuf, Node>,
    /// Files made read-only (inode metadata: kept through every crash state)
    read_only: BTreeSet<FileId>,
    /// Per directory, children whose entry changed since its last `sync_dir`
    unsynced: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

fn is_root(path: &Path) -> bool {
    path.as_os_str().is_empty() || path == Path::new("/")
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, path.display().to_string())
}

impl Image {
    fn is_dir(&self, path: &Path) -> bool {
        is_root(path) || self.volatile.get(path) == Some(&Node::Dir)
    }

    fn file(&self, path: &Path) -> io::Result<Option<FileId>> {
        match self.volatile.get(path) {
            Some(Node::File(id)) => Ok(Some(*id)),
            Some(Node::Dir) => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                path.display().to_string(),
            )),
            None => Ok(None),
        }
    }

    /// Only entries whose every ancestor is a directory in `namespace`
    fn reachable(namespace: &BTreeMap<PathBuf, Node>) -> BTreeMap<PathBuf, Node> {
        namespace
            .iter()
            .filter(|(path, _)| {
                path.ancestors()
                    .skip(1)
                    .all(|up| is_root(up) || namespace.get(up) == Some(&Node::Dir))
            })
            .map(|(path, node)| (path.clone(), *node))
            .collect()
    }

    /// Crashed image: `namespace` survives, each file holds `content(id)`, nothing pending
    fn settle(
        &self,
        namespace: &BTreeMap<PathBuf, Node>,
        content: impl Fn(FileId) -> Vec<u8>,
    ) -> Image {
        let namespace = Self::reachable(namespace);
        let files = (0..self.files.len())
            .map(|id| Content::settled(content(id)))
            .collect();
        Image {
            files,
            durable: namespace.clone(),
            volatile: namespace,
            read_only: self.read_only.clone(),
            unsynced: BTreeMap::new(),
        }
    }

    /// `path`'s entry changed: its parent's next `sync_dir` persists it
    fn touched(&mut self, path: &Path) {
        self.unsynced
            .entry(parent(path).to_path_buf())
            .or_default()
            .insert(path.to_path_buf());
    }

    /// `id` writable, or `PermissionDenied` (a read-write open of a read-only file)
    fn writable(&self, id: FileId, path: &Path) -> io::Result<()> {
        match self.read_only.contains(&id) {
            true => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is read-only", path.display()),
            )),
            false => Ok(()),
        }
    }

    /// Only what was synced: durable entries, durable bytes
    fn power_loss(&self) -> Image {
        self.settle(&self.durable, |id| self.files[id].durable.clone())
    }

    /// Every state a crash here could leave, labelled: power loss, nothing lost, each directory's
    /// pending entries alone, each file's pending writes alone
    fn crash_states(&self) -> Vec<(String, Image)> {
        let mut states = vec![
            ("nothing pending survives".to_owned(), self.power_loss()),
            (
                "everything pending survives".to_owned(),
                self.settle(&self.volatile, |id| self.files[id].kept_at_most()),
            ),
        ];
        states.extend(self.directory_states());
        states.extend(self.content_states());
        states.extend(self.mixed_states());
        states
    }

    /// Per directory with pending entries: all of them persisted (every file's bytes durable or
    /// kept), and each one alone, or all but it (a directory's entries need not persist together)
    fn directory_states(&self) -> Vec<(String, Image)> {
        let dirs: BTreeSet<&Path> = self
            .volatile
            .keys()
            .chain(self.durable.keys())
            .map(|path| parent(path))
            .collect();
        let mut states = Vec::new();
        for dir in dirs {
            let changed = self.changed_children(dir);
            if changed.is_empty() {
                continue;
            }
            let all = self.with_entries_persisted(dir, &|_| true);
            let (durable, kept) = (
                self.settle(&all, |id| self.files[id].durable.clone()),
                self.settle(&all, |id| self.files[id].kept_at_most()),
            );
            let dir_name = dir.display();
            states.push((
                format!("{dir_name} entries persisted, bytes durable"),
                durable,
            ));
            states.push((format!("{dir_name} entries persisted, bytes kept"), kept));
            if changed.len() < 2 {
                continue;
            }
            for child in &changed {
                for (which, persisted) in [
                    (
                        "alone",
                        &(|path: &Path| path == child) as &dyn Fn(&Path) -> bool,
                    ),
                    ("all but it", &|path: &Path| path != child),
                ] {
                    let namespace = self.with_entries_persisted(dir, persisted);
                    let image = self.settle(&namespace, |id| self.files[id].kept_at_most());
                    let label = format!("{dir_name}: {} entry {which} persisted", child.display());
                    states.push((label, image));
                }
            }
        }
        states
    }

    /// `dir`'s children whose entry differs between `volatile` and `durable`
    fn changed_children(&self, dir: &Path) -> Vec<PathBuf> {
        let under = |namespace: &BTreeMap<PathBuf, Node>| -> BTreeSet<PathBuf> {
            namespace
                .keys()
                .filter(|path| parent(path) == dir)
                .cloned()
                .collect()
        };
        under(&self.volatile)
            .union(&under(&self.durable))
            .filter(|path| self.volatile.get(*path) != self.durable.get(*path))
            .cloned()
            .collect()
    }

    /// `durable` with the changes to `dir`'s entries that `persisted` picks taken from `volatile`
    fn with_entries_persisted(
        &self,
        dir: &Path,
        persisted: &dyn Fn(&Path) -> bool,
    ) -> BTreeMap<PathBuf, Node> {
        let mut namespace = self.durable.clone();
        for child in self.changed_children(dir) {
            if !persisted(&child) {
                continue;
            }
            match self.volatile.get(&child) {
                Some(node) => namespace.insert(child, *node),
                None => namespace.remove(&child),
            };
        }
        namespace
    }

    /// Several files varying at once (a pending write kept in one, torn in another, lost in a
    /// third): [`MIXED_SAMPLES`] combinations of each file's crash variants, chosen by a generator
    /// seeded from the image (deterministic: the same run samples the same states)
    fn mixed_states(&self) -> Vec<(String, Image)> {
        let pending: Vec<(FileId, Variants)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, content)| !content.pending.is_empty())
            .map(|(id, content)| (id, content.crash_variants()))
            .collect();
        if pending.len() < 2 {
            return Vec::new();
        }
        let mut seed = self.fingerprint();
        let mut next = || {
            // splitmix64
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) as usize
        };
        let mut states = Vec::new();
        for sample in 0..MIXED_SAMPLES {
            let mut chosen: BTreeMap<FileId, Vec<u8>> = BTreeMap::new();
            let mut labels = Vec::new();
            for (id, variants) in &pending {
                let (label, bytes) = &variants[next() % variants.len()];
                chosen.insert(*id, bytes.clone());
                labels.push(format!("{}: {label}", self.path_of(*id)));
            }
            let namespace = match sample % 2 {
                0 => &self.durable,
                _ => &self.volatile,
            };
            let image = self.settle(namespace, |id| match chosen.get(&id) {
                Some(bytes) => bytes.clone(),
                None => self.files[id].durable.clone(),
            });
            states.push((
                format!("mixed sample {sample}: {}", labels.join("; ")),
                image,
            ));
        }
        states
    }

    /// Per file with pending writes, under the durable and the current namespace:
    ///
    /// - each crash variant of it, every other file durable
    /// - it losing its pending writes, every other file keeping theirs (e.g. writeback reached
    ///   data + manifest, not the checksums)
    fn content_states(&self) -> Vec<(String, Image)> {
        let mut states = Vec::new();
        let spaces = [("durable", &self.durable), ("current", &self.volatile)];
        for (id, content) in self
            .files
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.pending.is_empty())
        {
            let path = self.path_of(id);
            for (variant, bytes) in content.crash_variants() {
                for (space, namespace) in spaces {
                    let image = self.settle(namespace, |other| match other == id {
                        true => bytes.clone(),
                        false => self.files[other].durable.clone(),
                    });
                    states.push((format!("{path}: {variant}, {space} entries"), image));
                }
            }
            for (space, namespace) in spaces {
                let image = self.settle(namespace, |other| match other == id {
                    true => self.files[other].durable.clone(),
                    false => self.files[other].kept_at_most(),
                });
                let label =
                    format!("{path}: pending lost, every other file's kept, {space} entries");
                states.push((label, image));
            }
        }
        states
    }

    /// Where file `id` is linked (current name first), for crash labels
    fn path_of(&self, id: FileId) -> String {
        let linked = |namespace: &BTreeMap<PathBuf, Node>| {
            namespace
                .iter()
                .find(|(_, node)| **node == Node::File(id))
                .map(|(path, _)| path.display().to_string())
        };
        linked(&self.volatile)
            .or_else(|| linked(&self.durable))
            .unwrap_or_else(|| format!("unlinked file #{id}"))
    }

    /// Identity of a settled image: the reachable namespace and each file's bytes
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for (path, node) in &self.volatile {
            path.hash(&mut hasher);
            match node {
                Node::Dir => 0u8.hash(&mut hasher),
                Node::File(id) => self.files[*id].current.hash(&mut hasher),
            }
        }
        hasher.finish()
    }
}

#[derive(Debug, Default)]
struct Inner {
    image: Image,
    locks: HashSet<PathBuf>,
    tag: u64,
    recorded: Option<Vec<CrashPoint>>,
    mutations: u64,
    fail_from: Option<u64>,
    reads: u64,
    fail_reads_from: Option<u64>,
    fail_removes: bool,
    fail_creates: bool,
    fail_writes: bool,
}

/// Counts one call in `count`; `EIO` named by `what(at)` from call `from` on
fn injected(
    count: &mut u64,
    from: Option<u64>,
    what: impl FnOnce(u64) -> String,
) -> io::Result<()> {
    let at = *count;
    *count += 1;
    match from.is_some_and(|from| at >= from) {
        true => Err(io::Error::other(what(at))),
        false => Ok(()),
    }
}

impl Inner {
    /// Counts one positional read; `EIO` from `SimFs::fail_reads_from`'s read on
    fn read(&mut self, path: &Path) -> io::Result<()> {
        injected(&mut self.reads, self.fail_reads_from, |at| {
            format!("sim: injected read EIO at read {at}: {}", path.display())
        })
    }

    /// Counts one mutating call; `EIO` from [`SimFs::fail_from`]'s op on (disk gone, not flaky)
    fn mutate(&mut self, op: impl FnOnce() -> String) -> io::Result<()> {
        injected(&mut self.mutations, self.fail_from, |at| {
            format!("sim: injected EIO at op {at}: {}", op())
        })
    }

    fn record(&mut self, op: impl FnOnce() -> String) {
        if let Some(recorded) = &mut self.recorded {
            recorded.push(CrashPoint {
                tag: self.tag,
                op: op(),
                image: self.image.clone(),
            });
        }
    }
}

/// In-memory [`Fs`]; see the module docs
#[derive(Debug, Default)]
pub struct SimFs {
    inner: Arc<Mutex<Inner>>,
}

/// Image at one persistence point
#[derive(Debug, Clone)]
struct CrashPoint {
    tag: u64,
    op: String,
    image: Image,
}

/// Filesystem a crash could leave, ready to reopen
///
/// - `tag` = [`SimFs::set_tag`] value when the crash hit (tests: commits acknowledged by then)
pub struct CrashState {
    /// Tag when the crash hit
    pub tag: u64,
    /// Where it hit and what it kept
    pub label: String,
    /// The filesystem it left
    pub fs: Arc<SimFs>,
}

impl SimFs {
    /// Empty, not recording
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records a `CrashPoint` around every persistence point (for [`SimFs::crash_states`])
    pub fn recording() -> Arc<Self> {
        let fs = Self::default();
        fs.lock_inner().recorded = Some(Vec::new());
        Arc::new(fs)
    }

    fn from_image(image: Image) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Mutex::new(Inner {
                image,
                ..Inner::default()
            })),
        })
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("sim fs mutex poisoned")
    }

    /// Stamped onto every later `CrashPoint` (tests: commits acknowledged so far)
    pub fn set_tag(&self, tag: u64) {
        self.lock_inner().tag = tag;
    }

    /// Every distinct state a crash at any recorded point could leave (deduplicated on
    /// `(tag, image)`: tag decides what a recovery may land on)
    pub fn crash_states(&self) -> Vec<CrashState> {
        let points = self
            .lock_inner()
            .recorded
            .clone()
            .expect("crash states need SimFs::recording");
        let mut seen = HashSet::new();
        let mut states = Vec::new();
        for point in points {
            for (label, image) in point.image.crash_states() {
                if seen.insert((point.tag, image.fingerprint())) {
                    states.push(CrashState {
                        tag: point.tag,
                        label: format!("after `{}`: {label}", point.op),
                        fs: SimFs::from_image(image),
                    });
                }
            }
        }
        states
    }

    /// Every persistence point recorded so far, as its op (`sync_data <path>`, `remove <path>`, …)
    #[cfg(test)]
    pub(crate) fn recorded_ops(&self) -> Vec<String> {
        let inner = self.lock_inner();
        let points = inner.recorded.iter().flatten();
        points.map(|point| point.op.clone()).collect()
    }

    /// Mutating call `op` (0-based, [`SimFs::mutations`] order) and every later one fail with
    /// `EIO`, nothing applied (Pebble `errorfs.OnIndex`: loop `op` until a workload succeeds)
    pub fn fail_from(&self, op: u64) {
        self.lock_inner().fail_from = Some(op);
    }

    /// Every removal fails with `EIO` while `on` (a disk refusing unlinks), everything else works
    #[cfg(test)]
    pub(crate) fn fail_removes(&self, on: bool) {
        self.lock_inner().fail_removes = on;
    }

    /// Every new file's creation fails with `EIO` while `on`, everything else works
    #[cfg(test)]
    pub(crate) fn fail_creates(&self, on: bool) {
        self.lock_inner().fail_creates = on;
    }

    /// Every write to a file fails with `EIO` while `on` (files are still created)
    #[cfg(test)]
    pub(crate) fn fail_writes(&self, on: bool) {
        self.lock_inner().fail_writes = on;
    }

    /// `EIO` from the nth positional read on (failing disk surfacing on read)
    ///
    /// - mapped reads = copies here, never fail (real disk: `SIGBUS` kills the process, untestable
    ///   in-process)
    #[cfg(test)]
    pub(crate) fn fail_reads_from(&self, read: u64) {
        self.lock_inner().fail_reads_from = Some(read);
    }

    /// Mutating calls so far (creates, writes, truncates, syncs, renames, removes)
    pub fn mutations(&self) -> u64 {
        self.lock_inner().mutations
    }

    /// Same image after a process exit (volatile state kept, no power loss), healthy again
    pub fn restarted(&self) -> Arc<SimFs> {
        SimFs::from_image(self.lock_inner().image.clone())
    }

    /// Image after a power loss right now (only synced entries + bytes; background threads
    /// mid-write included), healthy
    #[cfg(test)]
    pub(crate) fn power_loss(&self) -> Arc<SimFs> {
        SimFs::from_image(self.lock_inner().image.power_loss())
    }

    /// Current volatile content, `None` when absent
    #[cfg(test)]
    pub(crate) fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        let inner = self.lock_inner();
        match inner.image.volatile.get(path) {
            Some(Node::File(id)) => Some(inner.image.files[*id].current.clone()),
            _ => None,
        }
    }

    /// Durable rewrite (bit rot, operator)
    #[cfg(test)]
    pub(crate) fn corrupt(&self, path: &Path, edit: impl FnOnce(&mut Vec<u8>)) {
        let mut inner = self.lock_inner();
        let Some(Node::File(id)) = inner.image.volatile.get(path).copied() else {
            panic!("corrupt: {} is not a file", path.display());
        };
        let content = &mut inner.image.files[id];
        let mut bytes = content.current.clone();
        edit(&mut bytes);
        *content = Content::settled(bytes);
    }
}

impl SimFs {
    fn handle(&self, id: FileId, path: &Path) -> Arc<dyn FileHandle> {
        Arc::new(SimFile {
            inner: Arc::clone(&self.inner),
            id,
            path: path.to_path_buf(),
        })
    }

    /// Whether `path` was made read-only
    #[cfg(test)]
    pub(crate) fn is_read_only(&self, path: &Path) -> bool {
        let inner = self.lock_inner();
        let id = inner.image.file(path).ok().flatten();
        id.is_some_and(|id| inner.image.read_only.contains(&id))
    }
}

impl Fs for SimFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let mut inner = self.lock_inner();
        for up in dir.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if is_root(up) {
                continue;
            }
            match inner.image.volatile.get(up) {
                Some(Node::Dir) => {}
                Some(Node::File(_)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        up.display().to_string(),
                    ))
                }
                None => {
                    inner.mutate(|| format!("mkdir {}", up.display()))?;
                    inner.image.volatile.insert(up.to_path_buf(), Node::Dir);
                    inner.image.touched(up);
                }
            }
        }
        Ok(())
    }

    fn open(&self, path: &Path) -> io::Result<Arc<dyn FileHandle>> {
        let mut inner = self.lock_inner();
        if !inner.image.is_dir(parent(path)) {
            return Err(not_found(parent(path)));
        }
        let id = match inner.image.file(path)? {
            Some(id) => {
                inner.image.writable(id, path)?;
                id
            }
            None => {
                if inner.fail_creates {
                    return Err(io::Error::other(format!(
                        "sim: injected create EIO: {}",
                        path.display()
                    )));
                }
                inner.mutate(|| format!("create {}", path.display()))?;
                let id = inner.image.files.len();
                inner.image.files.push(Content::default());
                inner
                    .image
                    .volatile
                    .insert(path.to_path_buf(), Node::File(id));
                inner.image.touched(path);
                id
            }
        };
        Ok(Arc::new(SimFile {
            inner: Arc::clone(&self.inner),
            id,
            path: path.to_path_buf(),
        }))
    }

    fn open_existing(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        let inner = self.lock_inner();
        let id = inner.image.file(path)?;
        if let Some(id) = id {
            inner.image.writable(id, path)?;
        }
        Ok(id.map(|id| self.handle(id, path)))
    }

    fn open_read_only(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        let id = self.lock_inner().image.file(path)?;
        Ok(id.map(|id| self.handle(id, path)))
    }

    /// No page cache here: the current bytes (one counted read)
    fn read_uncached(&self, path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let mut inner = self.lock_inner();
        let Some(id) = inner.image.file(path)? else {
            return Ok(None);
        };
        // the device: pending writes (a direct read flushes them first), never one a failed
        // writeback lost
        let device = inner.image.files[id].kept_at_most();
        let len = device.len() as u64;
        if len > limit {
            return Err(too_long(path, len, limit));
        }
        inner.read(path)?;
        Ok(Some(device))
    }

    fn set_read_only(&self, path: &Path) -> io::Result<()> {
        let mut inner = self.lock_inner();
        let id = inner.image.file(path)?.ok_or_else(|| not_found(path))?;
        inner.mutate(|| format!("set_read_only {}", path.display()))?;
        inner.image.read_only.insert(id);
        inner.record(|| format!("set_read_only {}", path.display()));
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut inner = self.lock_inner();
        let id = inner.image.file(from)?.ok_or_else(|| not_found(from))?;
        if !inner.image.is_dir(parent(to)) {
            return Err(not_found(parent(to)));
        }
        inner.mutate(|| format!("rename {} -> {}", from.display(), to.display()))?;
        inner.image.volatile.remove(from);
        inner
            .image
            .volatile
            .insert(to.to_path_buf(), Node::File(id));
        inner.image.touched(from);
        inner.image.touched(to);
        inner.record(|| format!("rename {} -> {}", from.display(), to.display()));
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut inner = self.lock_inner();
        inner.image.file(path)?.ok_or_else(|| not_found(path))?;
        if inner.fail_removes {
            return Err(io::Error::other(format!(
                "sim: injected remove EIO: {}",
                path.display()
            )));
        }
        inner.mutate(|| format!("remove {}", path.display()))?;
        inner.image.volatile.remove(path);
        inner.image.touched(path);
        inner.record(|| format!("remove {}", path.display()));
        Ok(())
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let inner = self.lock_inner();
        if !inner.image.is_dir(dir) {
            return Err(not_found(dir));
        }
        Ok(inner
            .image
            .volatile
            .keys()
            .filter(|path| parent(path) == dir)
            .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
            .collect())
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        // `.` = the directory relative paths start from: here, the root
        let dir = match dir == Path::new(".") {
            true => Path::new(""),
            false => dir,
        };
        let mut inner = self.lock_inner();
        if !inner.image.is_dir(dir) {
            return Err(not_found(dir));
        }
        inner.mutate(|| format!("sync_dir {}", dir.display()))?;
        let children = inner.image.unsynced.remove(dir).unwrap_or_default();
        let image = &inner.image;
        let changed: Vec<(PathBuf, Option<Node>)> = children
            .into_iter()
            .filter_map(|path| {
                let now = image.volatile.get(&path).copied();
                (image.durable.get(&path).copied() != now).then_some((path, now))
            })
            .collect();
        if !changed.is_empty() {
            inner.record(|| format!("before sync_dir {}", dir.display()));
            for (path, node) in changed {
                match node {
                    Some(node) => inner.image.durable.insert(path, node),
                    None => inner.image.durable.remove(&path),
                };
            }
            inner.record(|| format!("sync_dir {}", dir.display()));
        }
        Ok(())
    }

    fn lock(&self, path: &Path) -> io::Result<LockGuard> {
        self.open(path)?;
        let mut inner = self.lock_inner();
        if !inner.locks.insert(path.to_path_buf()) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is held", path.display()),
            ));
        }
        Ok(LockGuard {
            _held: Box::new(SimLock {
                inner: Arc::clone(&self.inner),
                path: path.to_path_buf(),
            }),
        })
    }
}

struct SimLock {
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
}

impl Drop for SimLock {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.locks.remove(&self.path);
        }
    }
}

struct SimFile {
    inner: Arc<Mutex<Inner>>,
    id: FileId,
    path: PathBuf,
}

impl fmt::Debug for SimFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SimFile").field(&self.path).finish()
    }
}

impl SimFile {
    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("sim fs mutex poisoned")
    }

    fn push(&self, op: Op) -> io::Result<()> {
        let mut inner = self.lock_inner();
        if inner.fail_writes && matches!(op, Op::Write { .. }) {
            return Err(io::Error::other(format!(
                "sim: injected write EIO: {}",
                self.path.display()
            )));
        }
        inner.mutate(|| match &op {
            Op::Write { at, bytes } => {
                format!("write {}B at {at} {}", bytes.len(), self.path.display())
            }
            Op::SetLen(len) => format!("set_len {len} {}", self.path.display()),
        })?;
        inner.image.files[self.id].push(op);
        Ok(())
    }
}

impl FileHandle for SimFile {
    fn len(&self) -> io::Result<u64> {
        Ok(self.lock_inner().image.files[self.id].current.len() as u64)
    }

    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        let mut inner = self.lock_inner();
        inner.read(&self.path)?;
        let data = &inner.image.files[self.id].current;
        let at = usize::try_from(at).map_err(io::Error::other)?;
        let bytes = data.get(at..at + buf.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{} read past end", self.path.display()),
            )
        })?;
        buf.copy_from_slice(bytes);
        Ok(())
    }

    fn write_all_at(&self, buf: &[u8], at: u64) -> io::Result<()> {
        self.push(Op::Write {
            at,
            bytes: buf.to_vec(),
        })
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.push(Op::SetLen(len))
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut inner = self.lock_inner();
        if let Err(error) = inner.mutate(|| format!("sync_data {}", self.path.display())) {
            inner.image.files[self.id].writeback_failed();
            return Err(error);
        }
        if inner.image.files[self.id].pending.is_empty() {
            return Ok(());
        }
        inner.record(|| format!("before sync_data {}", self.path.display()));
        inner.image.files[self.id].sync();
        inner.record(|| format!("sync_data {}", self.path.display()));
        Ok(())
    }

    fn write_behind(&self, _: Range<u64>) -> io::Result<()> {
        Ok(())
    }

    fn drop_cache(&self) -> io::Result<()> {
        Ok(())
    }

    fn map(&self, len: usize) -> io::Result<Option<Mapping>> {
        let inner = self.lock_inner();
        let data = &inner.image.files[self.id].current;
        let Some(bytes) = data.get(..len) else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "a file shorter than its mapping",
            ));
        };
        Ok((len > 0).then(|| Mapping {
            bytes: Bytes::copy_from_slice(bytes),
            advice: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// - synced name + bytes always survive; unsynced writes as prefix, reordered and torn
    /// - unsynced rename over the file: whole or not at all
    #[test]
    fn crash_states_keep_what_was_synced_and_every_shape_of_what_was_not() {
        let fs = SimFs::recording();
        let dir = Path::new("/d");
        fs.create_dir_all(dir).expect("mkdir");
        let file = fs.open(&dir.join("a")).expect("create a");
        file.write_all_at(b"v1", 0).expect("write");
        file.sync_data().expect("sync a");
        fs.sync_dir(dir).expect("sync dir");
        fs.sync_dir(Path::new("/"))
            .expect("sync root (else /d itself may vanish)");
        fs.set_tag(1);
        file.write_all_at(b"X", 0).expect("unsynced write 0");
        file.write_all_at(b"Y", 1).expect("unsynced write 1");
        let staged = fs.open(&dir.join("next")).expect("create next");
        staged.write_all_at(b"v2", 0).expect("write");
        staged.sync_data().expect("sync next");
        fs.rename(&dir.join("next"), &dir.join("a"))
            .expect("rename over a");

        // tag 0 = before `a`'s name and bytes were synced (losing either is allowed, and seen)
        let (mut seen, mut unsynced_name_lost) = (BTreeSet::new(), false);
        for state in fs.crash_states() {
            let a = state.fs.contents(&dir.join("a"));
            match (state.tag, a) {
                (0, None) => unsynced_name_lost = true,
                (0, Some(_)) => {}
                (_, None) => panic!("{}: synced name lost", state.label),
                (_, Some(a)) => {
                    assert!(a.len() >= 2, "{}: synced bytes lost: {a:?}", state.label);
                    seen.insert(String::from_utf8_lossy(&a).into_owned());
                }
            }
        }
        assert!(
            unsynced_name_lost,
            "no crash state drops a name never dir-synced"
        );
        for expected in ["v1", "X1", "XY", "vY", "v2"] {
            assert!(
                seen.contains(expected),
                "no crash state holds {expected:?}: {seen:?}"
            );
        }
        let torn: Vec<_> = seen
            .iter()
            .filter(|a| !["v1", "X1", "XY", "vY", "v2"].contains(&a.as_str()))
            .collect();
        assert!(
            !torn.is_empty(),
            "zero-filled / garbage variants of the unsynced writes: {seen:?}"
        );
    }

    /// A failed `sync_data` loses the file's pending writes (Linux marks the pages clean): reads
    /// still see them, the device never holds them, a direct read and a power loss find the synced
    /// bytes
    #[test]
    fn a_failed_sync_drops_the_files_pending_writes_from_the_device() {
        let fs = SimFs::new();
        let path = Path::new("/a");
        let file = fs.open(path).expect("create a");
        file.write_all_at(b"v1", 0).expect("write");
        file.sync_data().expect("sync a");
        fs.sync_dir(Path::new("/")).expect("sync root");
        file.write_all_at(b"XY", 0).expect("unsynced write");
        fs.fail_from(fs.mutations());
        file.sync_data().expect_err("the injected sync failure");

        assert_eq!(fs.contents(path), Some(b"XY".to_vec()), "still cached");
        let device = |fs: &SimFs| fs.read_uncached(path, 2).expect("read").expect("a");
        assert_eq!(device(&fs), b"v1", "never reached the device");
        assert_eq!(device(&fs.restarted()), b"v1", "nor after a restart");
        assert_eq!(fs.power_loss().contents(path), Some(b"v1".to_vec()));
    }

    /// `fail_from(2)`: calls 0-1 apply, 2 on = named `EIO` + nothing applied; `restarted()` = same
    /// volatile image, healthy
    #[test]
    fn injected_failures_apply_nothing_and_a_restart_heals() {
        let fs = SimFs::new();
        fs.fail_from(2);
        let dir = Path::new("/d");
        fs.create_dir_all(dir).expect("op 0: mkdir");
        let file = fs.open(&dir.join("a")).expect("op 1: create");
        let failed = file.write_all_at(b"v1", 0).expect_err("op 2 fails");
        assert!(
            failed
                .to_string()
                .contains("injected EIO at op 2: write 2B at 0 /d/a"),
            "{failed}"
        );
        assert!(
            file.sync_data().is_err() && fs.sync_dir(dir).is_err(),
            "later ops fail too"
        );
        assert_eq!(
            (fs.contents(&dir.join("a")), fs.mutations()),
            (Some(Vec::new()), 5)
        );

        let healed = fs.restarted();
        let file = healed
            .open_existing(&dir.join("a"))
            .expect("open")
            .expect("a survives");
        file.write_all_at(b"v1", 0).expect("healthy");
        assert_eq!(healed.contents(&dir.join("a")), Some(b"v1".to_vec()));
        assert_eq!(
            fs.contents(&dir.join("a")),
            Some(Vec::new()),
            "original untouched"
        );
    }
}
