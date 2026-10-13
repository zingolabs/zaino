//! [`Fs`] over `std::fs` + `memmap2` (Linux)

use std::{
    fs::{File, OpenOptions},
    io,
    ops::Range,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;
use memmap2::Mmap;

use super::{Durability, FileHandle, Fs, LockGuard, Mapping, Opened, ReadHandle};

#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl RealFs {
    pub fn shared() -> Arc<dyn Fs> {
        Arc::new(Self)
    }
}

/// Read + write, created if absent (`true` = by this call), never truncated
fn open_or_create(path: &Path) -> io::Result<(File, bool)> {
    let options = || OpenOptions::new().read(true).write(true).clone();
    match options().create_new(true).open(path) {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Ok((options().open(path)?, false))
        }
        Err(error) => Err(error),
    }
}

impl Fs for RealFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn open(&self, path: &Path) -> io::Result<Opened> {
        let (file, created) = open_or_create(path)?;
        Ok(Opened { file: Arc::new(RealFile(file)), created })
    }

    fn open_read(&self, path: &Path) -> io::Result<Option<Arc<dyn ReadHandle>>> {
        match File::open(path) {
            Ok(file) => Ok(Some(Arc::new(RealFile(file)))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn size(&self, path: &Path) -> io::Result<Option<u64>> {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => Err(io::ErrorKind::IsADirectory.into()),
            Ok(meta) => Ok(Some(meta.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let name = entry?.file_name();
            names.push(name.into_string().map_err(|name| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("non-UTF-8 entry {name:?} in {}", dir.display()),
                )
            })?);
        }
        names.sort_unstable();
        Ok(names)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    #[cfg(target_os = "linux")]
    fn durability(&self, files: &[PathBuf]) -> io::Result<Durability> {
        Ok(super::probe::durability(files))
    }

    /// No probe off Linux (no `sync_file_range`)
    #[cfg(not(target_os = "linux"))]
    fn durability(&self, _: &[PathBuf]) -> io::Result<Durability> {
        Ok(Durability::PerFile)
    }

    fn lock(&self, path: &Path) -> io::Result<LockGuard> {
        let (file, _) = open_or_create(path)?;
        match file.try_lock() {
            Ok(()) => Ok(LockGuard { _held: Box::new(file) }),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is held by another process", path.display()),
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

#[derive(Debug)]
struct RealFile(File);

impl ReadHandle for RealFile {
    fn size(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        self.0.read_exact_at(buf, at)
    }

    fn drop_cache(&self) -> io::Result<()> {
        Ok(rustix::fs::fadvise(&self.0, 0, None, rustix::fs::Advice::DontNeed)?)
    }

    fn map(&self) -> io::Result<Option<Mapping>> {
        if self.size()? == 0 {
            return Ok(None);
        }
        let map = Arc::new(map_read_only(&self.0)?);
        Ok(Some(Mapping { bytes: Bytes::from_owner(Shared(Arc::clone(&map))), mmap: Some(map) }))
    }
}

impl FileHandle for RealFile {
    fn write_all_at(&self, buf: &[u8], at: u64) -> io::Result<()> {
        self.0.write_all_at(buf, at)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn write_back(&self) -> io::Result<()> {
        write_back(&self.0)
    }

    fn write_behind(&self, range: Range<u64>) -> io::Result<()> {
        start_writeback(&self.0, range)
    }
}

/// Bytes of `map[..len]` in page cache (`mincore`; sound: `map` = a live mapping, `len` <= its
/// length, the vector one byte per page)
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(super) fn resident_bytes(map: &Mmap, len: usize) -> io::Result<u64> {
    assert!(len <= map.len(), "residency of {len} bytes past a {}-byte mapping", map.len());
    if len == 0 {
        return Ok(0);
    }
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .map_err(|_| io::Error::other("no page size"))?;
    let mut pages = vec![0u8; len.div_ceil(page)];
    match unsafe { libc::mincore(map.as_ptr().cast_mut().cast(), len, pages.as_mut_ptr()) } {
        0 => {
            let resident = pages.iter().filter(|&&page| page & 1 == 1).count();
            Ok(u64::try_from(resident.saturating_mul(page).min(len)).unwrap_or(u64::MAX))
        }
        _ => Err(io::Error::last_os_error()),
    }
}

/// No `mincore` reading off Linux
#[cfg(not(target_os = "linux"))]
pub(super) fn resident_bytes(_: &Mmap, _: usize) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

/// `Arc<Mmap>` as the `AsRef<[u8]>` owner `Bytes::from_owner` wants
struct Shared(Arc<Mmap>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// - sound: mapped bytes below a committed length never rewritten or truncated after publication
///   (truncation only in `PagedFile::open`, before any map; `LOCK` keeps other processes out)
/// - broken rule = SIGBUS (uncatchable) or corrupted zero-copy `Bytes` already handed out
#[allow(unsafe_code)]
fn map_read_only(file: &File) -> io::Result<Mmap> {
    unsafe { Mmap::map(file) }
}

/// `sync_file_range(SYNC_FILE_RANGE_WRITE)`: dirty pages in `range` queued, no wait (sound: fd
/// borrowed from a live `File`, no memory passed)
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn start_writeback(file: &File, range: Range<u64>) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let offset = i64::try_from(range.start).map_err(io::Error::other)?;
    let len = i64::try_from(range.end - range.start).map_err(io::Error::other)?;
    let flags = libc::SYNC_FILE_RANGE_WRITE;
    match unsafe { libc::sync_file_range(file.as_raw_fd(), offset, len, flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// No write-behind hint off Linux (`sync_data` alone flushes)
#[cfg(not(target_os = "linux"))]
fn start_writeback(_: &File, _: Range<u64>) -> io::Result<()> {
    Ok(())
}

/// `sync_file_range(WAIT_BEFORE | WRITE | WAIT_AFTER)` over the whole file: every dirty page
/// written + its bio completed, no cache flush, no metadata (sound: fd borrowed from a live
/// `File`, no memory passed)
///
/// - an `Err` = the commit's: the writeback error is consumed here, never reported again
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn write_back(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
        | libc::SYNC_FILE_RANGE_WRITE
        | libc::SYNC_FILE_RANGE_WAIT_AFTER;
    match unsafe { libc::sync_file_range(file.as_raw_fd(), 0, 0, flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Off Linux: `sync_data` (never reached: `durability` = `PerFile` there)
#[cfg(not(target_os = "linux"))]
fn write_back(file: &File) -> io::Result<()> {
    file.sync_data()
}
