//! One map's segment directory: every name in it, what stays at open, what a commit unlinks

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use super::SegmentMeta;
use crate::{
    fs::Fs,
    pages::{sums_of, sums_path, Sealed},
    schema::MapTable,
};

/// Directory holding one map's segments, their checksums and merges' scratch files
#[derive(Debug, Clone)]
pub(crate) struct SegmentDir {
    fs: Arc<dyn Fs>,
    path: PathBuf,
}

impl SegmentDir {
    pub(crate) fn new(fs: Arc<dyn Fs>, path: &Path) -> Self {
        Self { fs, path: path.to_path_buf() }
    }

    pub(crate) fn fs(&self) -> &dyn Fs {
        self.fs.as_ref()
    }

    pub(crate) fn shared_fs(&self) -> &Arc<dyn Fs> {
        &self.fs
    }

    pub(crate) fn segment(&self, id: u32) -> PathBuf {
        self.path.join(file_name(id))
    }

    /// Never listed: whatever is left of one at open was cut short
    pub(crate) fn scratch(&self, id: u32, part: &str) -> PathBuf {
        self.path.join(format!("{id:010}.{part}.scratch"))
    }

    /// Exactly `listed` kept: every other segment (+ checksums) and every scratch file removed
    pub(crate) fn retain(&self, listed: &[SegmentMeta]) -> io::Result<()> {
        let mut removed = false;
        for name in self.fs.list(&self.path)? {
            let unlisted = match segment_id(&name) {
                Some(id) => !listed.iter().any(|segment| segment.id == id),
                None => name.ends_with(".scratch"),
            };
            if unlisted {
                self.fs.remove(&self.path.join(name))?;
                removed = true;
            }
        }
        if removed {
            self.sync()?;
        }
        Ok(())
    }

    /// Segments (+ checksums) no durable manifest lists any more
    ///
    /// - no dir sync: unlink lost to a crash = unlisted segment, removed at the next open
    pub(crate) fn remove(&self, segments: &[SegmentMeta]) -> io::Result<()> {
        for segment in segments {
            let path = self.segment(segment.id);
            self.fs.remove(&sums_path(&path))?;
            self.fs.remove(&path)?;
        }
        Ok(())
    }

    /// Segments written since the last call → durably linked
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.fs.sync_dir(&self.path)
    }
}

/// Every file `list` seals, by path under the index directory (`<table>/<segment>`)
pub(crate) fn files<'a>(
    table: &'a MapTable,
    list: &'a [SegmentMeta],
) -> impl Iterator<Item = (String, Sealed)> + 'a {
    list.iter().map(|segment| (format!("{}/{}", table.name, file_name(segment.id)), segment.sealed))
}

/// `<id:010>.seg`, zero-padded so a listing sorts by id
pub(crate) fn file_name(id: u32) -> String {
    format!("{id:010}.seg")
}

/// Id of a segment file or its checksums (`None` for anything else in the directory)
fn segment_id(name: &str) -> Option<u32> {
    sums_of(name).unwrap_or(name).strip_suffix(".seg")?.parse().ok()
}
