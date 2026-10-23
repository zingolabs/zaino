//! `TAILS`: every log file's committed partial last page, saved at each commit so open can
//! restore one that a crash tore while the next commit's appends rewrote it in place (InnoDB
//! doublewrite, PostgreSQL full-page writes, for the one page an append rewrites)
//!
//! ```text
//! file  = slot 0 ‖ slot 1                      each `slot_len` bytes, zero-filled when created
//! slot  = magic b"ZAINOTL1" ‖ seq u64 ‖ count u32 ‖ entry* ‖ crc32 ‖ (zeros to the slot's end)
//! entry = path_len u16 ‖ path ‖ offset u64 ‖ len u16 ‖ bytes
//! ```
//!
//! - commit `seq`'s tails are written over slot `seq % 2` and synced before its manifest, so the
//!   slot of whichever manifest open finds is whole (the next commit writes the other slot)
//! - `slot_len` is fixed by the index's log files (at most a page each): written in place, no
//!   metadata change (as the manifest)
//! - only a device that tears inside a sector can damage a committed tail (sector-atomic writes
//!   keep each sector wholly old or new, and both hold the committed bytes)
//! - open restores only files the index declares, within their last committed page: a damaged
//!   `TAILS` can never write anywhere else

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    error::StoreError,
    fs::{FileHandle, Fs},
    manifest::BodyReader,
    pages::PAGE,
};

pub(crate) const TAILS: &str = "TAILS";
const MAGIC: [u8; 8] = *b"ZAINOTL1";
const HEADER: usize = 8 + 8 + 4;
const CRC: usize = 4;
/// An entry's bytes besides its path: path_len ‖ offset ‖ len ‖ at most a page
const ENTRY: usize = 2 + 8 + 2 + PAGE;

/// A log file's committed bytes past its last full page (`offset` = that page's start)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) offset: u64,
    pub(crate) bytes: Vec<u8>,
}

/// The index's `TAILS` file, sized for its log files
pub(crate) struct Tails {
    file: Arc<dyn FileHandle>,
    names: BTreeSet<String>,
    slot_len: u64,
}

impl std::fmt::Debug for Tails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tails")
            .field("files", &self.names.len())
            .field("slot_len", &self.slot_len)
            .finish()
    }
}

fn malformed(reason: &str) -> StoreError {
    StoreError::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("TAILS: {reason}"),
    ))
}

impl Tails {
    /// `dir`'s `TAILS` for log files `names` (paths under `dir`): created zeroed and synced, its
    /// name synced, when absent or sized for other files
    pub(crate) fn open(
        fs: &dyn Fs,
        dir: &Path,
        names: impl IntoIterator<Item = String>,
    ) -> Result<Self, StoreError> {
        let names: BTreeSet<String> = names.into_iter().collect();
        let entries = names.iter().try_fold(0usize, |sum, name| {
            sum.checked_add(name.len().checked_add(ENTRY)?)
        });
        let slot_len = entries
            .and_then(|entries| entries.checked_add(HEADER + CRC))
            .and_then(|len| len.checked_next_multiple_of(PAGE))
            .ok_or(StoreError::Overflow("TAILS slot"))? as u64;
        let path = dir.join(TAILS);
        let file = fs.open(&path)?;
        let file_len = slot_len
            .checked_mul(2)
            .ok_or(StoreError::Overflow("TAILS"))?;
        if file.len()? != file_len {
            file.set_len(0)?;
            let zeros = vec![0; usize::try_from(file_len).map_err(io::Error::other)?];
            file.write_all_at(&zeros, 0)?;
            file.sync_data()?;
            fs.sync_dir(dir)?;
        }
        Ok(Self {
            file,
            names,
            slot_len,
        })
    }

    /// Commit `seq`'s `tails` (path under the directory, tail) over slot `seq % 2`, synced
    pub(crate) fn write(&self, seq: u64, tails: &[(String, Tail)]) -> Result<(), StoreError> {
        let mut slot = Vec::with_capacity(usize::try_from(self.slot_len).unwrap_or(0));
        slot.extend_from_slice(&MAGIC);
        slot.extend_from_slice(&seq.to_le_bytes());
        let count = u32::try_from(tails.len()).map_err(|_| StoreError::Overflow("TAILS count"))?;
        slot.extend_from_slice(&count.to_le_bytes());
        for (name, tail) in tails {
            if !self.names.contains(name) {
                return Err(StoreError::Internal("a tail of an undeclared log file"));
            }
            let name_len =
                u16::try_from(name.len()).map_err(|_| StoreError::Overflow("TAILS path"))?;
            let len = u16::try_from(tail.bytes.len())
                .ok()
                .filter(|len| usize::from(*len) < PAGE)
                .ok_or(StoreError::Internal("a tail of a page or more"))?;
            slot.extend_from_slice(&name_len.to_le_bytes());
            slot.extend_from_slice(name.as_bytes());
            slot.extend_from_slice(&tail.offset.to_le_bytes());
            slot.extend_from_slice(&len.to_le_bytes());
            slot.extend_from_slice(&tail.bytes);
        }
        let crc = crc32fast::hash(&slot);
        slot.extend_from_slice(&crc.to_le_bytes());
        if slot.len() as u64 > self.slot_len {
            return Err(StoreError::Internal("tails past their slot"));
        }
        self.file.write_all_at(&slot, self.slot_at(seq))?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Every log file under `dir` whose bytes differ from the tails commit `seq` saved: those
    /// bytes rewritten and the file synced (nothing saved for `seq`, or a torn slot = nothing to
    /// restore: the files' own checks then decide); how many were restored
    pub(crate) fn restore(&self, fs: &dyn Fs, dir: &Path, seq: u64) -> Result<usize, StoreError> {
        let Some(tails) = self.read(seq)? else {
            return Ok(0);
        };
        let mut restored = 0usize;
        for (name, tail) in tails {
            let path: PathBuf = dir.join(&name);
            // a file missing, or cut short of its tail page, lost more than a tail: its own
            // checks report it
            let Some(file) = fs.open_existing(&path)? else {
                continue;
            };
            if file.len()? < tail.offset {
                continue;
            }
            let mut held = vec![0; tail.bytes.len()];
            let intact = file.read_exact_at(&mut held, tail.offset).is_ok() && held == tail.bytes;
            if !intact {
                file.write_all_at(&tail.bytes, tail.offset)?;
                file.sync_data()?;
                restored = restored.saturating_add(1);
            }
        }
        Ok(restored)
    }

    /// Slot `seq % 2`'s tails, if it holds commit `seq`'s whole (`None` = torn, empty, or another
    /// commit's)
    fn read(&self, seq: u64) -> Result<Option<Vec<(String, Tail)>>, StoreError> {
        let mut slot = vec![0; usize::try_from(self.slot_len).map_err(io::Error::other)?];
        self.file.read_exact_at(&mut slot, self.slot_at(seq))?;
        let mut reader = BodyReader::new(&slot);
        let (Ok(magic), Ok(held_seq), Ok(count)) =
            (reader.array::<8>(), reader.u64(), reader.u32())
        else {
            return Err(malformed("slot shorter than its header"));
        };
        if magic != MAGIC || held_seq != seq {
            return Ok(None);
        }
        let mut tails = Vec::new();
        for _ in 0..count {
            let Some(entry) = self.entry(&mut reader) else {
                return Ok(None);
            };
            tails.push(entry);
        }
        let read = slot.len().saturating_sub(reader.remaining());
        let body = slot.get(..read).ok_or_else(|| malformed("body"))?;
        match reader.u32() {
            Ok(crc) if crc32fast::hash(body) == crc => Ok(Some(tails)),
            _ => Ok(None),
        }
    }

    /// One entry, if it names a declared log file within its last page
    fn entry(&self, reader: &mut BodyReader<'_>) -> Option<(String, Tail)> {
        let name_len = usize::from(reader.u16().ok()?);
        let name = String::from_utf8(reader.take(name_len).ok()?.to_vec()).ok()?;
        let offset = reader.u64().ok()?;
        let len = usize::from(reader.u16().ok()?);
        let bytes = reader.take(len).ok()?.to_vec();
        let page = PAGE as u64;
        let declared = self.names.contains(&name) && offset.is_multiple_of(page) && len < PAGE;
        declared.then_some((name, Tail { offset, bytes }))
    }

    fn slot_at(&self, seq: u64) -> u64 {
        match seq.is_multiple_of(2) {
            true => 0,
            false => self.slot_len,
        }
    }
}

#[cfg(test)]
mod tests;
