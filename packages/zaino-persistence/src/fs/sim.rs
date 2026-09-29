//! In-memory [`Fs`] that knows what a power loss would keep
//!
//! - file content: durable bytes + writes pending since the last `sync_data`
//! - namespace: durable entries + changes pending since the parent's last `sync_dir`
//! - crash points recorded around persistence points only (just before and after each
//!   `sync_data` / `sync_dir`, after each `rename` / `remove`; CrashMonkey: bugs surface there)
//! - `Image::crash_states` = what a crash at that point could leave (ALICE model: unsynced
//!   writes dropped, prefix-applied, reordered, torn, zero- or garbage-filled; unsynced entries
//!   lost or kept)
//! - [`SimFs::fail_from`] = `EIO` from the nth mutating call on (Pebble `errorfs`)
//! - [`SimFs::power_loss`] / [`SimFs::restarted`] = a crash / a process exit at any instant

use std::{
    collections::{BTreeMap, HashSet},
    fmt, io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use bytes::Bytes;

use super::{FileHandle, Fs, LockGuard, Mapping};

type FileId = usize;

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

/// `current` = `durable` with every `pending` op applied (kept materialized: reads are hot)
#[derive(Debug, Clone, Default)]
struct Content {
    durable: Vec<u8>,
    pending: Vec<Op>,
    current: Vec<u8>,
}

impl Content {
    /// Nothing pending: what a crashed image holds
    fn settled(bytes: Vec<u8>) -> Self {
        Self { durable: bytes.clone(), pending: Vec::new(), current: bytes }
    }

    fn push(&mut self, op: Op) {
        op.apply(&mut self.current);
        self.pending.push(op);
    }

    fn sync(&mut self) {
        self.durable.clone_from(&self.current);
        self.pending.clear();
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
            Some(Node::Dir) => {
                Err(io::Error::new(io::ErrorKind::IsADirectory, path.display().to_string()))
            }
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
        let files = (0..self.files.len()).map(|id| Content::settled(content(id))).collect();
        Image { files, durable: namespace.clone(), volatile: namespace }
    }

    /// `durable` with `dir`'s direct children taken from `volatile`
    fn with_dir_persisted(&self, dir: &Path) -> BTreeMap<PathBuf, Node> {
        let mut namespace: BTreeMap<_, _> = self
            .durable
            .iter()
            .filter(|(path, _)| parent(path) != dir)
            .map(|(path, node)| (path.clone(), *node))
            .collect();
        namespace.extend(
            self.volatile
                .iter()
                .filter(|(path, _)| parent(path) == dir)
                .map(|(path, node)| (path.clone(), *node)),
        );
        namespace
    }

    /// Only what was synced: durable entries, durable bytes
    fn power_loss(&self) -> Image {
        self.settle(&self.durable, |id| self.files[id].durable.clone())
    }

    /// Every state a crash here could leave, labelled: power loss, nothing lost, then each
    /// directory's pending entries alone, then each file's pending writes alone
    fn crash_states(&self) -> Vec<(String, Image)> {
        let mut states = vec![
            ("nothing pending survives".to_owned(), self.power_loss()),
            (
                "everything pending survives".to_owned(),
                self.settle(&self.volatile, |id| self.files[id].current.clone()),
            ),
        ];
        states.extend(self.directory_states());
        states.extend(self.content_states());
        states
    }

    /// Per directory with pending entries: those persisted, every file's bytes durable or current
    fn directory_states(&self) -> Vec<(String, Image)> {
        let dirs: HashSet<&Path> =
            self.volatile.keys().chain(self.durable.keys()).map(|path| parent(path)).collect();
        let children = |namespace: &BTreeMap<PathBuf, Node>, dir: &Path| -> Vec<(PathBuf, Node)> {
            namespace
                .iter()
                .filter(|(path, _)| parent(path) == dir)
                .map(|(path, node)| (path.clone(), *node))
                .collect()
        };
        let mut states = Vec::new();
        for dir in dirs {
            if children(&self.volatile, dir) == children(&self.durable, dir) {
                continue;
            }
            let namespace = self.with_dir_persisted(dir);
            let (durable, current) = (
                self.settle(&namespace, |id| self.files[id].durable.clone()),
                self.settle(&namespace, |id| self.files[id].current.clone()),
            );
            states.push((format!("{} entries persisted, bytes durable", dir.display()), durable));
            states.push((format!("{} entries persisted, bytes current", dir.display()), current));
        }
        states
    }

    /// Per file with pending writes: each crash variant of it, every other file durable, under
    /// the durable and the current namespace
    fn content_states(&self) -> Vec<(String, Image)> {
        let mut states = Vec::new();
        for (id, content) in self.files.iter().enumerate().filter(|(_, c)| !c.pending.is_empty()) {
            let path = self.path_of(id);
            for (variant, bytes) in content.crash_variants() {
                for (space, namespace) in [("durable", &self.durable), ("current", &self.volatile)]
                {
                    let image = self.settle(namespace, |other| match other == id {
                        true => bytes.clone(),
                        false => self.files[other].durable.clone(),
                    });
                    states.push((format!("{path}: {variant}, {space} entries"), image));
                }
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
}

impl Inner {
    /// Counts one mutating call; `EIO` from [`SimFs::fail_from`]'s op on (disk gone, not flaky)
    fn mutate(&mut self, op: impl FnOnce() -> String) -> io::Result<()> {
        let at = self.mutations;
        self.mutations += 1;
        match self.fail_from.is_some_and(|from| at >= from) {
            true => Err(io::Error::other(format!("sim: injected EIO at op {at}: {}", op()))),
            false => Ok(()),
        }
    }

    fn record(&mut self, op: impl FnOnce() -> String) {
        if let Some(recorded) = &mut self.recorded {
            recorded.push(CrashPoint { tag: self.tag, op: op(), image: self.image.clone() });
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

/// One filesystem a crash could leave, ready to reopen
///
/// - `tag` = [`SimFs::set_tag`] value when the crash hit (tests: commits acknowledged by then)
pub struct CrashState {
    pub tag: u64,
    pub label: String,
    pub fs: Arc<SimFs>,
}

impl SimFs {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Records a [`CrashPoint`] around every persistence point (for [`SimFs::crash_states`])
    pub fn recording() -> Arc<Self> {
        let fs = Self::default();
        fs.lock_inner().recorded = Some(Vec::new());
        Arc::new(fs)
    }

    fn from_image(image: Image) -> Arc<Self> {
        Arc::new(Self { inner: Arc::new(Mutex::new(Inner { image, ..Inner::default() })) })
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().expect("sim fs mutex poisoned")
    }

    /// Stamped onto every later [`CrashPoint`] (tests: commits acknowledged so far)
    pub fn set_tag(&self, tag: u64) {
        self.lock_inner().tag = tag;
    }

    /// Every distinct state a crash at any recorded point could leave (deduplicated on
    /// `(tag, image)`: the tag decides what a recovery may land on)
    pub fn crash_states(&self) -> Vec<CrashState> {
        let points =
            self.lock_inner().recorded.clone().expect("crash states need SimFs::recording");
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

    /// Mutating call `op` (0-based, [`SimFs::mutations`] order) and every later one fail with
    /// `EIO`, nothing applied (Pebble `errorfs.OnIndex`: loop `op` until a workload succeeds)
    pub fn fail_from(&self, op: u64) {
        self.lock_inner().fail_from = Some(op);
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
    pub fn power_loss(&self) -> Arc<SimFs> {
        SimFs::from_image(self.lock_inner().image.power_loss())
    }

    /// Current volatile content, `None` when absent
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        let inner = self.lock_inner();
        match inner.image.volatile.get(path) {
            Some(Node::File(id)) => Some(inner.image.files[*id].current.clone()),
            _ => None,
        }
    }

    /// Rewrites a file durably, as bit rot or an operator would
    pub fn corrupt(&self, path: &Path, edit: impl FnOnce(&mut Vec<u8>)) {
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
            Some(id) => id,
            None => {
                inner.mutate(|| format!("create {}", path.display()))?;
                let id = inner.image.files.len();
                inner.image.files.push(Content::default());
                inner.image.volatile.insert(path.to_path_buf(), Node::File(id));
                id
            }
        };
        Ok(Arc::new(SimFile { inner: Arc::clone(&self.inner), id, path: path.to_path_buf() }))
    }

    fn open_existing(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        let id = self.lock_inner().image.file(path)?;
        Ok(id.map(|id| {
            Arc::new(SimFile { inner: Arc::clone(&self.inner), id, path: path.to_path_buf() })
                as Arc<dyn FileHandle>
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut inner = self.lock_inner();
        let id = inner.image.file(from)?.ok_or_else(|| not_found(from))?;
        if !inner.image.is_dir(parent(to)) {
            return Err(not_found(parent(to)));
        }
        inner.mutate(|| format!("rename {} -> {}", from.display(), to.display()))?;
        inner.image.volatile.remove(from);
        inner.image.volatile.insert(to.to_path_buf(), Node::File(id));
        inner.record(|| format!("rename {} -> {}", from.display(), to.display()));
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut inner = self.lock_inner();
        inner.image.file(path)?.ok_or_else(|| not_found(path))?;
        inner.mutate(|| format!("remove {}", path.display()))?;
        inner.image.volatile.remove(path);
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
        let mut inner = self.lock_inner();
        if !inner.image.is_dir(dir) {
            return Err(not_found(dir));
        }
        inner.mutate(|| format!("sync_dir {}", dir.display()))?;
        let persisted = inner.image.with_dir_persisted(dir);
        if persisted != inner.image.durable {
            inner.record(|| format!("before sync_dir {}", dir.display()));
            inner.image.durable = persisted;
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
            _held: Box::new(SimLock { inner: Arc::clone(&self.inner), path: path.to_path_buf() }),
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
        let inner = self.lock_inner();
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
        self.push(Op::Write { at, bytes: buf.to_vec() })
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.push(Op::SetLen(len))
    }

    fn sync_data(&self) -> io::Result<()> {
        let mut inner = self.lock_inner();
        inner.mutate(|| format!("sync_data {}", self.path.display()))?;
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

    fn map(&self) -> io::Result<Option<Mapping>> {
        let inner = self.lock_inner();
        let data = &inner.image.files[self.id].current;
        Ok((!data.is_empty())
            .then(|| Mapping { bytes: Bytes::copy_from_slice(data), advice: None }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Synced name + bytes always survive; unsynced writes as prefix, reordered and torn; an
    /// unsynced rename over the file whole or not at all
    #[test]
    fn crash_states_keep_what_was_synced_and_every_shape_of_what_was_not() {
        let fs = SimFs::recording();
        let dir = Path::new("/d");
        fs.create_dir_all(dir).expect("mkdir");
        let file = fs.open(&dir.join("a")).expect("create a");
        file.write_all_at(b"v1", 0).expect("write");
        file.sync_data().expect("sync a");
        fs.sync_dir(dir).expect("sync dir");
        fs.sync_dir(Path::new("/")).expect("sync root (else /d itself may vanish)");
        fs.set_tag(1);
        file.write_all_at(b"X", 0).expect("unsynced write 0");
        file.write_all_at(b"Y", 1).expect("unsynced write 1");
        let staged = fs.open(&dir.join("next")).expect("create next");
        staged.write_all_at(b"v2", 0).expect("write");
        staged.sync_data().expect("sync next");
        fs.rename(&dir.join("next"), &dir.join("a")).expect("rename over a");

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
        assert!(unsynced_name_lost, "no crash state drops a name never dir-synced");
        for expected in ["v1", "X1", "XY", "vY", "v2"] {
            assert!(seen.contains(expected), "no crash state holds {expected:?}: {seen:?}");
        }
        let torn: Vec<_> =
            seen.iter().filter(|a| !["v1", "X1", "XY", "vY", "v2"].contains(&a.as_str())).collect();
        assert!(
            !torn.is_empty(),
            "zero-filled / garbage variants of the unsynced writes: {seen:?}"
        );
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
            failed.to_string().contains("injected EIO at op 2: write 2B at 0 /d/a"),
            "{failed}"
        );
        assert!(file.sync_data().is_err() && fs.sync_dir(dir).is_err(), "later ops fail too");
        assert_eq!((fs.contents(&dir.join("a")), fs.mutations()), (Some(Vec::new()), 5));

        let healed = fs.restarted();
        let file = healed.open_existing(&dir.join("a")).expect("open").expect("a survives");
        file.write_all_at(b"v1", 0).expect("healthy");
        assert_eq!(healed.contents(&dir.join("a")), Some(b"v1".to_vec()));
        assert_eq!(fs.contents(&dir.join("a")), Some(Vec::new()), "original untouched");
    }
}
