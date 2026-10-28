//! Byte stream a segment's navigation writes while its records stream out, copied in after the
//! records: memory up to its threshold (`LsmConfig::spill_bytes`), then a scratch file beside the
//! segment
//!
//! - large merge's fences + fingerprints = hundreds of MB (held in memory: merge memory ∝ segment)
//! - small batch never reaches the threshold, never touches a scratch file
//! - scratch files never listed, always removed when a set opens (crash can leave one); one whose
//!   removal failed is handed back to the owner's leftover cleanup

use std::{
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    fs::{FileHandle, Fs},
    pages::PagedFile,
};

/// `file` = scratch file + bytes already written to it, once spilled; `at` = bytes held in
/// memory before the stream moves to its scratch file (and the copy chunk size)
pub(super) struct Spill {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    at: usize,
    buffer: Vec<u8>,
    file: Option<(Arc<dyn FileHandle>, u64)>,
}

/// A scratch file still on disk: its removal failed
#[derive(Debug)]
pub(super) struct Scratch {
    pub(super) path: PathBuf,
    pub(super) bytes: u64,
}

fn overflow() -> io::Error {
    io::Error::other("scratch stream past u64 bytes")
}

impl Spill {
    /// `path` = where it spills, once it grows past `at` bytes
    pub(crate) fn new(fs: Arc<dyn Fs>, path: PathBuf, at: NonZeroUsize) -> Self {
        Self {
            fs,
            path,
            at: at.get(),
            buffer: Vec::new(),
            file: None,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() >= self.at {
            self.write_out()?;
        }
        Ok(())
    }

    /// Bytes pushed so far
    pub(crate) fn len(&self) -> u64 {
        let written = self.file.as_ref().map_or(0, |(_, written)| *written);
        written.saturating_add(self.buffer.len() as u64)
    }

    /// Every pushed byte appended to `out`, in order, then the scratch file removed → the
    /// scratch file if that removal failed; `None` = `pace` stopped it (the scratch file left,
    /// unlisted, for open's cleanup)
    ///
    /// - `pace` is told the I/O of each chunk as it goes: appended, plus written to and read
    ///   back from the scratch file once spilled; `false` = stop
    pub(crate) fn append_to(
        mut self,
        out: &mut PagedFile,
        pace: &mut dyn FnMut(u64) -> bool,
    ) -> io::Result<Option<Vec<Scratch>>> {
        let Some((file, _)) = &self.file else {
            out.append(&self.buffer)?;
            return Ok(pace(self.buffer.len() as u64).then(Vec::new));
        };
        let file = Arc::clone(file);
        self.write_out()?;
        let total = self.len();
        let mut chunk = vec![0; self.at];
        let mut at: u64 = 0;
        while let Some(left) = total.checked_sub(at).filter(|left| *left > 0) {
            let take = usize::try_from(left).map_or(self.at, |left| left.min(self.at));
            let piece = chunk.get_mut(..take).ok_or_else(overflow)?;
            file.read_exact_at(piece, at)?;
            out.append(piece)?;
            at = at.checked_add(take as u64).ok_or_else(overflow)?;
            if !pace((take as u64).saturating_mul(3)) {
                return Ok(None);
            }
        }
        drop(file);
        self.file = None;
        // the owner's cleanup retries it and reports why it failed
        Ok(Some(match self.fs.remove(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => vec![Scratch {
                path: self.path,
                bytes: total,
            }],
            _ => Vec::new(),
        }))
    }

    fn write_out(&mut self) -> io::Result<()> {
        let (file, written) = match &mut self.file {
            Some(opened) => opened,
            None => {
                let file = self.fs.open(&self.path)?;
                // scratch name reusable after a crash (open removes it; belt and braces)
                file.set_len(0)?;
                self.file.insert((file, 0))
            }
        };
        file.write_all_at(&self.buffer, *written)?;
        *written = written
            .checked_add(self.buffer.len() as u64)
            .ok_or_else(overflow)?;
        self.buffer.clear();
        Ok(())
    }
}

/// Scratch file of segment `id`'s `part`
pub(super) fn scratch_path(dir: &Path, id: u32, part: &str) -> PathBuf {
    dir.join(format!("{}{part}.scratch", super::file_prefix(id)))
}

/// Scratch file (removed whenever a set opens)
pub(super) fn is_scratch(name: &str) -> bool {
    name.ends_with(".scratch")
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;
    use crate::{
        fs::SimFs,
        pages::{FileKind, Sealed},
    };

    const AT: NonZeroUsize = NonZeroUsize::new(4096).expect("non-zero");

    /// - past the threshold → scratch file; appended bytes = exactly what was pushed, scratch gone
    /// - a scratch file whose removal fails is handed back with its bytes, still on disk
    /// - below it: nothing touches the disk
    #[test]
    fn a_spill_appends_every_byte_in_order_and_removes_its_scratch() {
        let fs = SimFs::new();
        let dir = Path::new("/set");
        fs.create_dir_all(dir).expect("dir");
        let pushed: Vec<u8> = (0..3 * AT.get() + 77).map(|n| (n % 251) as u8).collect();
        let spilled = |id| {
            let path = scratch_path(dir, id, "fences");
            let mut spill = Spill::new(fs.clone(), path.clone(), AT);
            for piece in pushed.chunks(1000) {
                spill.push(piece).expect("push");
            }
            assert_eq!(spill.len(), pushed.len() as u64);
            assert!(fs.contents(&path).is_some(), "spilled past the threshold");
            let target = dir.join(format!("{id:010}.seg"));
            let mut out = PagedFile::open(fs.as_ref(), &target, Sealed::EMPTY, FileKind::Segment)
                .expect("segment");
            let kept = spill.append_to(&mut out, &mut |_| true).expect("append");
            assert_eq!(fs.contents(&target).expect("segment"), pushed);
            (path, kept.expect("not stopped"))
        };

        let (path, kept) = spilled(7);
        assert!(
            kept.is_empty() && fs.contents(&path).is_none(),
            "scratch removed"
        );

        fs.fail_removes(true);
        let (path, kept) = spilled(9);
        fs.fail_removes(false);
        let [Scratch { path: left, bytes }] = kept.as_slice() else {
            panic!("the scratch file handed back: {kept:?}");
        };
        assert_eq!((left, *bytes), (&path, pushed.len() as u64));
        assert!(fs.contents(&path).is_some(), "still on disk");

        let small = Spill::new(fs.clone(), scratch_path(dir, 8, "fences"), AT);
        assert!(
            fs.contents(&scratch_path(dir, 8, "fences")).is_none(),
            "never spilled"
        );
        drop(small);
        assert!(is_scratch("0000000008.filter.scratch") && !is_scratch("0000000008.seg"));
    }
}
