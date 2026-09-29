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
const STAGED: &str = "MANIFEST.next";

/// Locked for the life of this value; see `docs/design/durability.md` §2
#[derive(Debug)]
pub struct IndexDir {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    identity: Identity,
    _lock: LockGuard,
}

/// A freshly opened directory: its committed manifest body, or `None` when never committed
#[derive(Debug)]
pub struct Opened {
    pub dir: IndexDir,
    pub body: Option<Vec<u8>>,
}

impl IndexDir {
    /// Creates `path` if absent (durably in its parent), locks it, drops a staged manifest
    pub fn open(fs: Arc<dyn Fs>, path: &Path, identity: Identity) -> Result<Opened, ManifestError> {
        fs.create_dir_all(path)?;
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs.sync_dir(parent)?;
        }
        let lock = fs.lock(&path.join(LOCK))?;

        if fs.open_existing(&path.join(STAGED))?.is_some() {
            fs.remove(&path.join(STAGED))?;
        }
        fs.sync_dir(path)?;

        let body = match fs.open_existing(&path.join(MANIFEST))? {
            Some(file) => Some(manifest::decode(identity, &file.read_all()?)?.to_vec()),
            None => None,
        };

        Ok(Opened { dir: Self { fs, path: path.to_path_buf(), identity, _lock: lock }, body })
    }

    pub fn fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self, relative: &str) -> io::Result<Arc<dyn FileHandle>> {
        self.fs.open(&self.path.join(relative))
    }

    /// `relative` created and durably linked into this directory
    pub fn subdir(&self, relative: &str) -> io::Result<PathBuf> {
        let dir = self.path.join(relative);
        self.fs.create_dir_all(&dir)?;
        self.fs.sync_dir(&self.path)?;
        Ok(dir)
    }

    /// Makes `body` the committed state: staged file → fsync → rename → directory fsync
    pub fn commit(&self, body: &[u8]) -> io::Result<()> {
        let staged = self.path.join(STAGED);
        let file = self.fs.open(&staged)?;
        let bytes = manifest::encode(self.identity, body);
        file.set_len(0)?;
        file.write_all_at(&bytes, 0)?;
        file.sync_data()?;
        self.fs.rename(&staged, &self.path.join(MANIFEST))?;
        self.fs.sync_dir(&self.path)
    }

    /// Fresh-directory check: `relative` absent or empty, else it holds uncommitted data
    pub fn ensure_empty(&self, relative: &str) -> Result<(), ManifestError> {
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
    pub fn ensure_empty_dir(&self, relative: &str) -> Result<(), ManifestError> {
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

    /// Fresh → commit → reopen reads it back; a second opener is locked out; a staged manifest
    /// and a foreign network never count as committed state
    #[test]
    fn commits_survive_reopen_the_lock_excludes_and_identity_is_enforced() {
        let fs = SimFs::new();
        let path = Path::new("/data/idx");

        let opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open fresh");
        assert_eq!(opened.body, None);
        opened.dir.ensure_empty("blocks.dat").expect("absent = empty");
        opened.dir.commit(&[1, 2, 3]).expect("commit");

        let locked = IndexDir::open(fs.clone(), path, IDENTITY).expect_err("locked");
        let would_block =
            matches!(&locked, ManifestError::Io(e) if e.kind() == io::ErrorKind::WouldBlock);
        assert!(would_block, "{locked}");

        opened.dir.file("blocks.dat").expect("file").write_all_at(&[9], 0).expect("write");
        let written = opened.dir.ensure_empty("blocks.dat");
        assert!(matches!(written, Err(ManifestError::Unmanifested { .. })));
        drop(opened);

        // commit crashed before its rename: staged manifest removed, committed one kept
        fs.open(&path.join(STAGED))
            .expect("stage")
            .write_all_at(&manifest::encode(IDENTITY, &[4]), 0)
            .expect("write staged");
        let reopened = IndexDir::open(fs.clone(), path, IDENTITY).expect("reopen");
        assert_eq!(reopened.body.as_deref(), Some(&[1, 2, 3][..]));
        assert!(fs.contents(&path.join(STAGED)).is_none(), "staged manifest removed");
        drop(reopened);

        let foreign = IndexDir::open(fs, path, Identity { network: NetworkType::Main, ..IDENTITY });
        assert!(matches!(foreign, Err(ManifestError::Network { .. })));
    }

    /// Every crash state of a commit reopens to the old body or the new one, never neither
    #[test]
    fn a_commit_is_atomic_under_every_crash_state() {
        let fs = SimFs::recording();
        let path = Path::new("/idx");
        let opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open");
        opened.dir.commit(&[1]).expect("first");
        fs.set_tag(1);
        opened.dir.commit(&[2]).expect("second");
        fs.set_tag(2);
        drop(opened);

        let states = fs.crash_states();
        assert!(states.len() > 5, "enumerated {} crash states", states.len());
        for state in states {
            let body = IndexDir::open(state.fs, path, IDENTITY)
                .unwrap_or_else(|error| panic!("{}: {error}", state.label))
                .body;
            let allowed: &[Option<Vec<u8>>] = match state.tag {
                0 => &[None, Some(vec![1])],
                1 => &[Some(vec![1]), Some(vec![2])],
                _ => &[Some(vec![2])],
            };
            assert!(allowed.contains(&body), "{}: {body:?}", state.label);
        }
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
