//! [`IndexDir`]: `LOCK`, `MANIFEST` create / commit / fdatasync, durable manifest read

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    fs::{read_all, FileHandle, Fs, LockGuard, ReadHandle},
    manifest::{self, Identity, ManifestError},
};

const LOCK: &str = "LOCK";
const MANIFEST: &str = "MANIFEST";

/// Manifest mid-creation: zeroed + synced here, then renamed to [`MANIFEST`] whole
const CREATING: &str = "MANIFEST.creating";

/// Locked for the life of this value (second process's open-time truncation under this one's
/// mappings = SIGBUS)
///
/// - `seq` = last durable manifest's sequence number (0 = nothing durable yet)
#[derive(Debug)]
pub(crate) struct IndexDir {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    identity: Identity,
    manifest: Arc<dyn FileHandle>,
    seq: u64,
    _lock: LockGuard,
}

/// How deep [`IndexDir::ensure_empty`] looks: `Shallow` skips nested directories
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Depth {
    Shallow,
    Recursive,
}

/// Freshly opened directory + durable manifest body (`None` = no commit yet)
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) dir: IndexDir,
    pub(crate) body: Option<Vec<u8>>,
}

impl IndexDir {
    /// `path` created if absent (durably in its parent), locked, manifest read
    ///
    /// - missing manifest → two zeroed slots under `CREATING`, synced, renamed in (`MANIFEST`
    ///   only ever whole)
    /// - later commits overwrite existing blocks
    /// - leftover `CREATING` = creation a crash interrupted → removed
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

        if fs.size(&path.join(CREATING))?.is_some() {
            fs.remove(&path.join(CREATING))?;
        }
        let manifest = match fs.size(&path.join(MANIFEST))? {
            Some(_) => fs.open(&path.join(MANIFEST))?.file,
            None => {
                let creating = fs.open(&path.join(CREATING))?.file;
                creating.write_all_at(&vec![0; manifest::FILE_LEN as usize], 0)?;
                creating.sync_data()?;
                fs.rename(&path.join(CREATING), &path.join(MANIFEST))?;
                creating
            }
        };
        fs.sync_dir(path)?;

        let durable = durable_body(manifest.as_ref(), identity)?;
        let (seq, body) = durable.map_or((0, None), |(seq, body)| (seq, Some(body)));
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

    /// Commits `body`: written over the slot the last commit did not use, then `fdatasync`ed
    ///
    /// - crash before the sync → other slot (last commit) intact
    /// - no rename / truncate / directory sync (write below EOF: sync flushes this file only, never
    ///   the filesystem journal)
    pub(crate) fn commit(&mut self, body: &[u8]) -> io::Result<()> {
        let seq = self.seq + 1;
        let slot = manifest::encode(self.identity, seq, body);
        self.manifest.write_all_at(&slot, manifest::slot_offset(seq))?;
        self.manifest.sync_data()?;
        self.seq = seq;
        Ok(())
    }

    /// Fresh-directory check
    ///
    /// - ok: `relative` absent, an empty file, or a directory of empty files (crash between
    ///   creating them and the first commit)
    /// - else → not yet durable data
    pub(crate) fn ensure_empty(&self, relative: &str, depth: Depth) -> Result<(), ManifestError> {
        let path = self.path.join(relative);
        let unmanifested = || ManifestError::Unmanifested { path: path.clone() };
        let names = match self.fs.size(&path) {
            Ok(None | Some(0)) => return Ok(()),
            Ok(Some(_)) => return Err(unmanifested()),
            Err(error) if error.kind() == io::ErrorKind::IsADirectory => self.fs.list(&path)?,
            Err(error) => return Err(error.into()),
        };
        for name in names {
            let child = format!("{relative}/{name}");
            if depth == Depth::Recursive {
                self.ensure_empty(&child, depth)?;
                continue;
            }
            match self.fs.size(&self.path.join(&child)) {
                Ok(Some(len)) if len > 0 => {
                    let path = self.path.join(&child);
                    return Err(ManifestError::Unmanifested { path });
                }
                Err(error) if error.kind() != io::ErrorKind::IsADirectory => {
                    return Err(error.into())
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `path`'s durable body, read offline (read only, no lock: safe beside a live writer)
///
/// - `None` = no commit yet
/// - manifest not decoding as `identity` = its `ManifestError`
pub(crate) fn read_durable(
    fs: &dyn Fs,
    path: &Path,
    identity: Identity,
) -> Result<Option<Vec<u8>>, ManifestError> {
    let Some(file) = fs.open_read(&path.join(MANIFEST))? else { return Ok(None) };
    Ok(durable_body(file.as_ref(), identity)?.map(|(_, body)| body))
}

/// `(sequence number, body)` of the manifest's last commit (`None` = no commit yet)
fn durable_body(
    manifest: &dyn ReadHandle,
    identity: Identity,
) -> Result<Option<(u64, Vec<u8>)>, ManifestError> {
    let bytes = read_all(manifest)?;
    Ok(manifest::latest(identity, &bytes)?.map(|(seq, body)| (seq, body.to_vec())))
}

#[cfg(test)]
mod tests {
    use zcash_protocol::consensus::NetworkType;

    use super::*;
    use crate::{fs::SimFs, schema::IndexKind};

    const IDENTITY: Identity =
        Identity { kind: IndexKind::CompactBlock, format: 1, network: NetworkType::Regtest };

    /// - fresh → commit → reopen reads it back
    /// - commits after a reopen continue the sequence (never over the live slot)
    /// - manifest never past its two slots
    #[test]
    fn commits_survive_reopen_and_overwrite_the_two_slots_in_place() {
        let fs = SimFs::new();
        let path = Path::new("/data/idx");
        let manifest_len = || fs.contents(&path.join(MANIFEST)).expect("manifest").len() as u64;

        let mut opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open fresh");
        assert_eq!(opened.body, None, "fresh: nothing durable");
        assert_eq!(manifest_len(), manifest::FILE_LEN, "both slots exist before any commit");
        opened.dir.commit(&[1]).expect("commit");
        drop(opened);
        for (durable, next) in [([1], [2]), ([2], [3])] {
            let mut reopened = IndexDir::open(fs.clone(), path, IDENTITY).expect("reopen");
            assert_eq!(reopened.body.as_deref(), Some(&durable[..]), "reopened at {durable:?}");
            reopened.dir.commit(&next).expect("commit after reopen");
        }

        let reopened = IndexDir::open(fs.clone(), path, IDENTITY).expect("reopen");
        assert_eq!(reopened.body.as_deref(), Some(&[3][..]), "the newest of three commits");
        assert_eq!(manifest_len(), manifest::FILE_LEN, "every commit overwrote a slot in place");
    }

    #[test]
    fn a_second_opener_is_locked_out_while_the_first_holds_the_directory() {
        let fs = SimFs::new();
        let path = Path::new("/data/idx");
        let first = IndexDir::open(fs.clone(), path, IDENTITY).expect("open");

        let second = IndexDir::open(fs.clone(), path, IDENTITY).map(|_| ()).expect_err("locked");

        let kind = match &second {
            ManifestError::Io(error) => Some(error.kind()),
            _ => None,
        };
        assert_eq!(kind, Some(io::ErrorKind::WouldBlock), "second opener: {second}");
        drop(first);
        IndexDir::open(fs, path, IDENTITY).expect("open once the first dropped");
    }

    /// - absent, empty file, directory of empty files = empty
    /// - written file anywhere it looks = not empty
    #[test]
    fn ensure_empty_refuses_a_written_file_at_the_depth_it_checks() {
        let fs = SimFs::new();
        let path = Path::new("/idx");
        let opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open");
        fs.create_dir_all(&path.join("map/nested")).expect("dirs");
        fs.open(&path.join("map/empty.seg")).expect("file");
        let nested = fs.open(&path.join("map/nested/written.seg")).expect("file").file;
        nested.write_all_at(&[9], 0).expect("write");
        let top = fs.open(&path.join("written.dat")).expect("file").file;
        top.write_all_at(&[9], 0).expect("write");

        #[rustfmt::skip]
        let cases = [
            ("absent.dat",    Depth::Shallow,   None),
            ("map/empty.seg", Depth::Shallow,   None),
            ("map",           Depth::Shallow,   None),
            ("map",           Depth::Recursive, Some("/idx/map/nested/written.seg")),
            ("written.dat",   Depth::Shallow,   Some("/idx/written.dat")),
        ];
        for (name, depth, written) in cases {
            let refused = opened.dir.ensure_empty(name, depth).err().map(|error| error.to_string());
            let expected =
                written.map(|path| format!("{path} holds data but the directory has no MANIFEST"));
            assert_eq!(refused, expected, "{name} checked {depth:?}");
        }
    }

    /// - every crash state of a commit reopens to the old body or the new one, never neither
    /// - commit 3 = first overwrite of a used slot (commit 1's)
    #[test]
    fn a_commit_is_atomic_under_every_crash_state() {
        let (fs, recording) = SimFs::recording();
        let path = Path::new("/idx");
        let mut opened = IndexDir::open(fs.clone(), path, IDENTITY).expect("open");
        for commit in 1u8..=3 {
            opened.dir.commit(&[commit]).expect("commit");
            fs.set_tag(u64::from(commit));
        }
        drop(opened);

        let states = recording.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let body = IndexDir::open(state.fs, path, IDENTITY)
                .unwrap_or_else(|error| panic!("{}: {error}", state.label))
                .body;
            let acknowledged = u8::try_from(state.tag).expect("tag = commits");
            let allowed = [acknowledged, acknowledged + 1]
                .map(|commit| (commit > 0).then(|| vec![commit.min(3)]));
            assert!(
                allowed.contains(&body),
                "{}: reopened to body {body:?}, want acknowledged or attempted {allowed:?}",
                state.label
            );
        }
    }

    /// - offline reader (`zainod verify`, read only, no lock) = what `IndexDir` sees
    /// - nothing before a manifest or its first commit, then the newest commit
    #[test]
    fn the_offline_reader_answers_the_newest_commit() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("idx");
        let fs = crate::fs::RealFs::shared();
        let read = || read_durable(fs.as_ref(), &path, IDENTITY).expect("read");
        assert_eq!(read(), None, "no directory");

        let mut opened = IndexDir::open(fs.clone(), &path, IDENTITY).expect("open").dir;
        assert_eq!(read(), None, "created, never committed");
        for body in [[1u8], [2], [3]] {
            opened.commit(&body).expect("commit");
            assert_eq!(read(), Some(body.to_vec()), "committed {body:?}");
        }
    }
}
