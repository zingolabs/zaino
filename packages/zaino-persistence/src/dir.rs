//! One index directory: its lock, its manifest, and the files under it

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    fs::{FileHandle, Fs, LockGuard},
    manifest::{self, Identity, ManifestError},
};

const LOCK: &str = "LOCK";
pub(crate) const MANIFEST: &str = "MANIFEST";

/// A manifest being created: zeroed and synced here, then renamed to [`MANIFEST`] whole
const CREATING: &str = "MANIFEST.creating";

/// Locked for the life of this value; see `docs/design/durability.md` §2
///
/// - `seq` = the last committed manifest's sequence number (0 = nothing committed yet)
#[derive(Debug)]
pub(crate) struct IndexDir {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    identity: Identity,
    manifest: Arc<dyn FileHandle>,
    seq: u64,
    _lock: LockGuard,
}

/// A freshly opened directory: its committed manifest body, or `None` when never committed
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) dir: IndexDir,
    pub(crate) body: Option<Vec<u8>>,
}

impl IndexDir {
    /// Creates `path` if absent (durably in its parent), locks it, and reads its manifest
    ///
    /// - a missing manifest is created as two zeroed slots under a temporary name, synced, then
    ///   renamed in: `MANIFEST` only ever exists whole, and every later commit overwrites blocks
    ///   that already exist (a creation a crash interrupted = a leftover temporary, removed here)
    pub(crate) fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        identity: Identity,
    ) -> Result<Opened, ManifestError> {
        fs.create_dir_all(path)?;
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs.sync_dir(parent)?;
        }
        let lock = fs.lock(&path.join(LOCK))?;

        if fs.open_existing(&path.join(CREATING))?.is_some() {
            fs.remove(&path.join(CREATING))?;
        }
        let manifest = match fs.open_existing(&path.join(MANIFEST))? {
            Some(manifest) => manifest,
            None => {
                let creating = fs.open(&path.join(CREATING))?;
                creating.write_all_at(&vec![0; manifest::FILE_LEN as usize], 0)?;
                creating.sync_data()?;
                fs.rename(&path.join(CREATING), &path.join(MANIFEST))?;
                creating
            }
        };
        fs.sync_dir(path)?;

        let bytes = manifest.read_all()?;
        let committed = manifest::latest(identity, &bytes)?;
        let (seq, body) = committed.map_or((0, None), |(seq, body)| (seq, Some(body.to_vec())));
        let dir = Self { fs, path: path.to_path_buf(), identity, manifest, seq, _lock: lock };
        Ok(Opened { dir, body })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// `relative` created and durably linked into this directory
    pub(crate) fn subdir(&self, relative: &str) -> io::Result<PathBuf> {
        let dir = self.path.join(relative);
        self.fs.create_dir_all(&dir)?;
        self.fs.sync_dir(&self.path)?;
        Ok(dir)
    }

    /// Makes `body` the committed state: written over the slot the last commit did not use, then
    /// `fdatasync`ed
    ///
    /// - a crash before the sync completes leaves the other slot, the last commit, intact
    /// - no rename, no truncate, no directory sync: the write lands in existing blocks below EOF,
    ///   so the sync flushes this file only and never commits the filesystem journal
    pub(crate) fn commit(&mut self, body: &[u8]) -> io::Result<()> {
        let seq = self.seq + 1;
        let slot = manifest::encode(self.identity, seq, body);
        self.manifest.write_all_at(&slot, manifest::slot_offset(seq))?;
        self.manifest.sync_data()?;
        self.seq = seq;
        Ok(())
    }

    /// Fresh-directory check: `relative` absent or empty, else it holds uncommitted data
    pub(crate) fn ensure_empty(&self, relative: &str) -> Result<(), ManifestError> {
        let path = self.path.join(relative);
        let unmanifested = || ManifestError::Unmanifested { path: path.display().to_string() };
        if let Some(file) = self.fs.open_existing(&path)? {
            if !file.is_empty()? {
                return Err(unmanifested());
            }
        }
        Ok(())
    }

    /// Fresh-directory check for a subdirectory: absent, or holding only empty files (a crash
    /// between creating them and the first commit)
    pub(crate) fn ensure_empty_dir(&self, relative: &str) -> Result<(), ManifestError> {
        let names = match self.fs.list(&self.path.join(relative)) {
            Ok(names) => names,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for name in names {
            self.ensure_empty(&format!("{relative}/{name}"))?;
        }
        Ok(())
    }
}

/// Bytes every file under `path` takes, subdirectories included (plain `stat`s, no lock); a file
/// removed mid-walk counts as gone
pub fn disk_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let bytes = match entry.metadata() {
            Ok(meta) if meta.is_dir() => disk_bytes(&entry.path()),
            Ok(meta) => Ok(meta.len()),
            Err(error) => Err(error),
        };
        match bytes {
            Ok(bytes) => total += bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{fs::SimFs, manifest::IndexKind};

    const IDENTITY: Identity =
        Identity { kind: IndexKind::CompactBlock, format: 1, network: NetworkType::Regtest };

    /// Fresh → commit → reopen reads it back, and commits after a reopen continue the sequence
    /// (never overwrite the live slot); the manifest never grows past its two slots; a second
    /// opener is locked out; a foreign network never counts as committed state
    #[test]
    fn commits_survive_reopen_the_lock_excludes_and_identity_is_enforced() {
        let fs = SimFs::new();
        let path = Path::new("/data/idx");

        let mut opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open fresh");
        assert_eq!(opened.body, None);
        let manifest_len = || fs.contents(&path.join(MANIFEST)).expect("manifest").len() as u64;
        assert_eq!(manifest_len(), manifest::FILE_LEN, "both slots exist before any commit");
        opened.dir.ensure_empty("blocks.dat").expect("absent = empty");
        opened.dir.commit(&[1, 2, 3]).expect("commit");

        let locked = IndexDir::open(fs.clone(), path, IDENTITY).expect_err("locked");
        let would_block =
            matches!(&locked, ManifestError::Io(e) if e.kind() == io::ErrorKind::WouldBlock);
        assert!(would_block, "{locked}");

        fs.open(&path.join("blocks.dat")).expect("file").write_all_at(&[9], 0).expect("write");
        let written = opened.dir.ensure_empty("blocks.dat");
        assert!(matches!(written, Err(ManifestError::Unmanifested { .. })));
        drop(opened);

        for (body, then) in [([1, 2, 3], [4, 5, 6]), ([4, 5, 6], [7, 8, 9])] {
            let mut reopened = IndexDir::open(fs.clone(), path, IDENTITY).expect("reopen");
            assert_eq!(reopened.body.as_deref(), Some(&body[..]));
            reopened.dir.commit(&then).expect("commit after reopen");
        }
        let reopened = IndexDir::open(fs.clone(), path, IDENTITY).expect("reopen");
        assert_eq!(reopened.body.as_deref(), Some(&[7, 8, 9][..]), "the newest of three");
        assert_eq!(manifest_len(), manifest::FILE_LEN, "every commit overwrote a slot in place");
        drop(reopened);

        let foreign = IndexDir::open(fs, path, Identity { network: NetworkType::Main, ..IDENTITY });
        assert!(matches!(foreign, Err(ManifestError::Network { .. })));
    }

    /// Every crash state of a commit reopens to the old body or the new one, never neither; the
    /// third commit is the first to overwrite a used slot (commit 1's)
    #[test]
    fn a_commit_is_atomic_under_every_crash_state() {
        let fs = SimFs::recording();
        let path = Path::new("/idx");
        let mut opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open");
        for commit in 1u8..=3 {
            opened.dir.commit(&[commit]).expect("commit");
            fs.set_tag(u64::from(commit));
        }
        drop(opened);

        let states = fs.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let body = IndexDir::open(state.fs, path, IDENTITY)
                .unwrap_or_else(|error| panic!("{}: {error}", state.label))
                .body;
            let acknowledged = u8::try_from(state.tag).expect("tag = commits");
            let allowed = [acknowledged, acknowledged + 1]
                .map(|commit| (commit > 0).then(|| vec![commit.min(3)]));
            assert!(allowed.contains(&body), "{}: {body:?}", state.label);
        }
    }

    /// The offline reader (`zainod verify`, read only, no lock) sees what `IndexDir` sees:
    /// nothing before a manifest exists or before its first commit, then the newest commit; an
    /// older layout is `InvalidData`
    #[test]
    fn the_offline_reader_answers_the_newest_commit() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("idx");
        let fs = crate::fs::RealFs::shared();
        let read = || manifest::read(fs.as_ref(), &path, IDENTITY);
        assert_eq!(read().expect("no directory"), None);

        let mut opened = IndexDir::open(fs.clone(), &path, IDENTITY).expect("open").dir;
        assert_eq!(read().expect("zeroed slots"), None, "created, never committed");
        for body in [[1u8], [2], [3]] {
            opened.commit(&body).expect("commit");
            assert_eq!(read().expect("committed"), Some(body.to_vec()));
        }
        drop(opened);

        std::fs::write(path.join(MANIFEST), [1; 40]).expect("older layout");
        let older = read().expect_err("refused");
        assert_eq!(older.kind(), io::ErrorKind::InvalidData, "{older}");
    }

    /// Every file counts, nested ones included; a missing directory is an error
    #[test]
    fn disk_bytes_sums_every_file_under_the_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("MANIFEST"), [0; 10]).expect("manifest");
        std::fs::create_dir(root.path().join("by_hash")).expect("subdir");
        std::fs::write(root.path().join("by_hash/0.seg"), [0; 4096]).expect("segment");
        std::fs::write(root.path().join("by_hash/1.seg"), []).expect("empty segment");

        assert_eq!(disk_bytes(root.path()).expect("walk"), 4106);
        let missing = disk_bytes(&root.path().join("absent")).expect_err("absent");
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }
}
