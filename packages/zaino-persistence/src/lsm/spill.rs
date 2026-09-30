//! A byte stream a segment's navigation writes while its records stream out, copied into the
//! segment after the records: held in memory up to [`SPILL_AT`], then on a scratch file beside
//! the segment
//!
//! A large merge's fences and filter fingerprints run to hundreds of megabytes, so holding them
//! until the records end would make merge memory grow with the segment. A small batch never
//! reaches [`SPILL_AT`] and never touches a scratch file. Scratch files are never listed and are
//! always removed when a set opens (a crash can leave one behind).

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    fs::{FileHandle, Fs},
    pages::PagedFile,
};

/// Bytes held in memory before the stream moves to its scratch file (and the copy chunk size)
///
/// - tests spill from 4 KiB, so every test that writes a segment, crash states included, runs
///   through the scratch files too
const SPILL_AT: usize = match cfg!(any(test, feature = "testing")) {
    true => 4096,
    false => 1 << 20,
};

pub(crate) struct Spill {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    buffer: Vec<u8>,
    /// Scratch file and the bytes already written to it, once spilled
    file: Option<(Arc<dyn FileHandle>, u64)>,
}

impl Spill {
    /// `path` = where it spills, if it grows past [`SPILL_AT`]
    pub(crate) fn new(fs: Arc<dyn Fs>, path: PathBuf) -> Self {
        Self { fs, path, buffer: Vec::new(), file: None }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= SPILL_AT {
            self.write_out()?;
        }
        Ok(())
    }

    /// Bytes pushed so far
    pub(crate) fn len(&self) -> u64 {
        self.file.as_ref().map_or(0, |(_, written)| *written) + self.buffer.len() as u64
    }

    /// Every pushed byte appended to `out`, in order; the scratch file removed
    pub(crate) fn append_to(mut self, out: &mut PagedFile) -> io::Result<()> {
        let Some((file, _)) = &self.file else {
            return out.append(&self.buffer);
        };
        let file = Arc::clone(file);
        self.write_out()?;
        let total = self.len();
        let mut chunk = vec![0; SPILL_AT];
        let mut at = 0;
        while at < total {
            let take = usize::try_from((total - at).min(SPILL_AT as u64)).expect("chunk");
            file.read_exact_at(&mut chunk[..take], at)?;
            out.append(&chunk[..take])?;
            at += take as u64;
        }
        drop(file);
        self.file = None;
        self.fs.remove(&self.path)
    }

    fn write_out(&mut self) -> io::Result<()> {
        if self.file.is_none() {
            let file = self.fs.open(&self.path)?;
            // a scratch name can be reused after a crash (open removes it, this makes sure)
            file.set_len(0)?;
            self.file = Some((file, 0));
        }
        let (file, written) = self.file.as_mut().expect("opened above");
        file.write_all_at(&self.buffer, *written)?;
        *written += self.buffer.len() as u64;
        self.buffer.clear();
        Ok(())
    }
}

/// Scratch file of segment `id`'s `part`
pub(crate) fn scratch_path(dir: &Path, id: u32, part: &str) -> PathBuf {
    dir.join(format!("{id:010}.{part}.scratch"))
}

/// A scratch file (removed whenever a set opens)
pub(crate) fn is_scratch(name: &str) -> bool {
    name.ends_with(".scratch")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fs::SimFs,
        pages::{FileKind, Sealed},
    };

    /// Past the threshold the stream moves to its scratch file; appended, the bytes are exactly
    /// what was pushed, and the scratch file is gone. Below it, nothing touches the disk.
    #[test]
    fn a_spill_appends_every_byte_in_order_and_removes_its_scratch() {
        let fs = SimFs::new();
        let dir = Path::new("/set");
        fs.create_dir_all(dir).expect("dir");
        let pushed: Vec<u8> = (0..3 * SPILL_AT + 77).map(|n| (n % 251) as u8).collect();

        let path = scratch_path(dir, 7, "fences");
        let mut spill = Spill::new(fs.clone(), path.clone());
        for piece in pushed.chunks(1000) {
            spill.push(piece).expect("push");
        }
        assert_eq!(spill.len(), pushed.len() as u64);
        assert!(fs.contents(&path).is_some(), "spilled past the threshold");

        let target = dir.join("0000000007.seg");
        let mut out = PagedFile::open(fs.as_ref(), &target, Sealed::EMPTY, FileKind::Segment)
            .expect("segment");
        spill.append_to(&mut out).expect("append");
        assert_eq!(fs.contents(&target).expect("segment"), pushed);
        assert!(fs.contents(&path).is_none(), "scratch removed");

        let small = Spill::new(fs.clone(), scratch_path(dir, 8, "fences"));
        assert!(fs.contents(&scratch_path(dir, 8, "fences")).is_none(), "never spilled");
        drop(small);
        assert!(is_scratch("0000000008.filter.scratch") && !is_scratch("0000000008.seg"));
    }
}
