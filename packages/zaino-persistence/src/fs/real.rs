//! [`Fs`] over `std::fs` + `memmap2` (Linux)

use std::{
    fs::{File, OpenOptions},
    io,
    ops::Range,
    os::unix::fs::FileExt,
    path::Path,
    sync::Arc,
};

use bytes::Bytes;
use memmap2::Mmap;

use super::{FileHandle, Fs, LockGuard, Mapping};

#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl RealFs {
    pub fn shared() -> Arc<dyn Fs> {
        Arc::new(Self)
    }
}

fn existing(opened: io::Result<File>) -> io::Result<Option<Arc<dyn FileHandle>>> {
    match opened {
        Ok(file) => Ok(Some(Arc::new(RealFile(file)))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Fs for RealFs {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)
    }

    fn open(&self, path: &Path) -> io::Result<Arc<dyn FileHandle>> {
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        Ok(Arc::new(RealFile(file)))
    }

    fn open_existing(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        existing(OpenOptions::new().read(true).write(true).open(path))
    }

    fn open_read_only(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        existing(OpenOptions::new().read(true).open(path))
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

    fn lock(&self, path: &Path) -> io::Result<LockGuard> {
        let file =
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
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

impl FileHandle for RealFile {
    fn len(&self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()> {
        self.0.read_exact_at(buf, at)
    }

    fn write_all_at(&self, buf: &[u8], at: u64) -> io::Result<()> {
        self.0.write_all_at(buf, at)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn write_behind(&self, range: Range<u64>) -> io::Result<()> {
        start_writeback(&self.0, range)
    }

    fn map(&self) -> io::Result<Option<Mapping>> {
        if self.len()? == 0 {
            return Ok(None);
        }
        let map = Arc::new(map_read_only(&self.0)?);
        Ok(Some(Mapping { bytes: Bytes::from_owner(Shared(Arc::clone(&map))), advice: Some(map) }))
    }
}

/// `Arc<Mmap>` as the `AsRef<[u8]>` owner `Bytes::from_owner` wants
struct Shared(Arc<Mmap>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// - sound: mapped bytes below a committed length are never rewritten or truncated after
///   publication (`docs/design/persistence-architecture.md` §5.2)
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

/// Calling thread → background priority: CPU nice 10, I/O best-effort level 7 (merges yield disk
/// + cores to serving reads)
///
/// - hint: failures ignored; I/O class honoured by BFQ / mq-deadline only (`none`, common on NVMe,
///   ignores it)
/// - sound: plain syscalls on the calling thread's own id, no memory passed
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn background_priority() {
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    const IOPRIO_CLASS_BE: libc::c_int = 2;
    const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
    const LOWEST_BE_LEVEL: libc::c_int = 7;
    const NICE: libc::c_int = 10;

    unsafe {
        let thread = libc::gettid();
        let io_priority = (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | LOWEST_BE_LEVEL;
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, thread, io_priority);
        libc::setpriority(libc::PRIO_PROCESS as _, thread as _, NICE);
    }
}

/// No thread priorities off Linux
#[cfg(not(target_os = "linux"))]
pub(crate) fn background_priority() {}
