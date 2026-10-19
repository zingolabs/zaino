//! Index directory: lock, manifest, files under it

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

/// Manifest mid-creation: zeroed + synced here, then renamed to [`MANIFEST`] whole
const CREATING: &str = "MANIFEST.creating";

/// Locked for the life of this value; see `docs/durability.md` §2
///
/// - `seq` = last committed manifest's sequence number (0 = nothing committed yet)
#[derive(Debug)]
pub(crate) struct IndexDir {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    identity: Identity,
    manifest: Arc<dyn FileHandle>,
    seq: u64,
    _lock: LockGuard,
}

/// Freshly opened directory + committed manifest body (`None` = never committed)
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) dir: IndexDir,
    pub(crate) body: Option<Vec<u8>>,
}

impl IndexDir {
    /// `path` created if absent (durably in its parent), locked, manifest read
    ///
    /// - missing manifest → two zeroed slots under `CREATING`, synced, renamed in (`MANIFEST`
    ///   only ever whole; later commits overwrite existing blocks)
    /// - leftover `CREATING` = creation a crash interrupted → removed
    pub(crate) fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        identity: Identity,
    ) -> Result<Opened, ManifestError> {
        fs.create_dir_all(path)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
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
        let committed = manifest::latest(&identity, &bytes)?;
        let (seq, body) = committed.map_or((0, None), |(seq, body)| (seq, Some(body.to_vec())));
        let dir = Self {
            fs,
            path: path.to_path_buf(),
            identity,
            manifest,
            seq,
            _lock: lock,
        };
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

    /// `body` committed: written over the slot the last commit did not use, then `fdatasync`ed
    ///
    /// - crash before the sync → other slot (last commit) intact
    /// - no rename / truncate / directory sync (write below EOF: sync flushes this file only, never
    ///   the filesystem journal)
    /// - a body past the slot = `Full`, nothing written
    pub(crate) fn commit(&mut self, body: &[u8]) -> Result<(), ManifestError> {
        let seq = self
            .seq
            .checked_add(1)
            .ok_or(ManifestError::Body("commit sequence past u64"))?;
        let slot = manifest::encode(&self.identity, seq, body)?;
        self.manifest
            .write_all_at(&slot, manifest::slot_offset(seq))?;
        self.manifest.sync_data()?;
        self.seq = seq;
        Ok(())
    }

    /// Fresh-directory check: `relative` absent or empty (else uncommitted data)
    pub(crate) fn ensure_empty(&self, relative: &str) -> Result<(), ManifestError> {
        let path = self.path.join(relative);
        let unmanifested = || ManifestError::Unmanifested {
            path: path.display().to_string(),
        };
        if let Some(file) = self.fs.open_read_only(&path)? {
            if !file.is_empty()? {
                return Err(unmanifested());
            }
        }
        Ok(())
    }

    /// Fresh-directory check for a subdirectory: absent, or empty files only (crash between
    /// creating them and the first commit)
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

/// Bytes of every file under `path`, subdirectories included (plain `stat`s, no lock)
///
/// - file removed mid-walk = gone
pub fn disk_bytes(path: &Path) -> io::Result<u64> {
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let bytes = match entry.metadata() {
            Ok(meta) if meta.is_dir() => disk_bytes(&entry.path()),
            Ok(meta) => Ok(meta.len()),
            Err(error) => Err(error),
        };
        match bytes {
            Ok(bytes) => total = total.saturating_add(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests;
