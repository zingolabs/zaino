//! File layer every index writes through: [`RealFs`] in production, `SimFs` under crash tests
//!
//! - positional I/O only (a partial write can never move where the next one lands)
//! - new file / rename / removal durable only after [`Fs::sync_dir`] on its parent
//! - maps read-only (full disk = `io::Error` from a write, never SIGBUS on a mapped store)
//! - mapping spans the whole file → callers bound reads by committed lengths (`crate::pages`)

use std::{
    fmt, io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use bytes::Bytes;

#[cfg(target_os = "linux")]
mod probe;
mod real;
#[cfg(any(test, feature = "testing"))]
mod sim;

use real::resident_bytes;
pub use real::RealFs;
#[cfg(any(test, feature = "testing"))]
pub use sim::{CrashState, Recording, SimFs};

/// Directory operations (Linux semantics)
pub trait Fs: Send + Sync + fmt::Debug + 'static {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;

    /// Read + write, created empty if absent, never truncated
    fn open(&self, path: &Path) -> io::Result<Opened>;

    /// Read only, `None` when absent (sealed segments, offline verify beside a live writer)
    fn open_read(&self, path: &Path) -> io::Result<Option<Arc<dyn ReadHandle>>>;

    /// `path`'s bytes (a stat: nothing opened); `None` when absent, `IsADirectory` for one
    fn size(&self, path: &Path) -> io::Result<Option<u64>>;

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Entry names directly under `dir`, sorted
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;

    fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// Exclusive for the guard's life; held elsewhere = `ErrorKind::WouldBlock`
    fn lock(&self, path: &Path) -> io::Result<LockGuard>;

    /// How a commit may make `files` durable (decided once, at open: the files a barrier covers)
    fn durability(&self, files: &[PathBuf]) -> io::Result<Durability>;
}

/// [`Fs::open`]'s file + whether this call created it
pub struct Opened {
    pub file: Arc<dyn FileHandle>,
    pub created: bool,
}

/// How a commit makes the files it grew durable before its manifest
///
/// - `PerFile` = `sync_data` each (any POSIX filesystem)
/// - `Barrier` = [`FileHandle::write_back`] each, then one `sync_data` = one device cache flush
///   covering them all (in-place-update filesystems only: `probe.rs`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    PerFile,
    Barrier,
}

/// Open file, read side
pub trait ReadHandle: Send + Sync + fmt::Debug {
    fn size(&self) -> io::Result<u64>;

    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()>;

    /// Read-only view of the whole file; `None` when empty (mmap refuses zero length)
    fn map(&self) -> io::Result<Option<Mapping>>;

    /// Its clean pages out of page cache (`POSIX_FADV_DONTNEED`; advisory: later reads fault them
    /// back in)
    fn drop_cache(&self) -> io::Result<()>;
}

/// Open file, read + write
pub trait FileHandle: ReadHandle {
    fn write_all_at(&self, buf: &[u8], at: u64) -> io::Result<()>;

    fn set_len(&self, len: u64) -> io::Result<()>;

    /// Content and length durable on return
    fn sync_data(&self) -> io::Result<()>;

    /// Every dirty page written to the device and waited on, no cache flush (durable only after a
    /// later flush: [`Durability::Barrier`])
    fn write_back(&self) -> io::Result<()>;

    /// Writeback of `range` started: no wait, no durability (bounds dirty pages ahead of
    /// `sync_data`; RocksDB `bytes_per_sync`)
    fn write_behind(&self, range: Range<u64>) -> io::Result<()>;
}

/// Every byte of `file`
pub(crate) fn read_all(file: &dyn ReadHandle) -> io::Result<Vec<u8>> {
    let len = usize::try_from(file.size()?).map_err(io::Error::other)?;
    let mut bytes = vec![0; len];
    file.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}

/// Held lock; released on drop
pub struct LockGuard {
    _held: Box<dyn Send + Sync>,
}

impl fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LockGuard")
    }
}

/// Read-only file contents, sliceable into refcounted [`Bytes`] (zero-copy on [`RealFs`])
///
/// - `mmap` = the live mapping (`None` = a copy: `SimFs`; advice + residency then no-ops)
#[derive(Clone)]
pub struct Mapping {
    bytes: Bytes,
    mmap: Option<Arc<memmap2::Mmap>>,
}

impl Mapping {
    pub(crate) fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// `MADV_WILLNEED` over `range` (advisory: a failure costs page faults, not correctness)
    pub(crate) fn will_need(&self, range: Range<usize>) {
        if let Some(map) = &self.mmap {
            let _ = map.advise_range(memmap2::Advice::WillNeed, range.start, range.len());
        }
    }

    /// Bytes of the first `len` in page cache (`None` = not knowable: no real mapping, or the
    /// kernel refused)
    pub(crate) fn resident_bytes(&self, len: usize) -> Option<u64> {
        resident_bytes(self.mmap.as_deref()?, len).ok()
    }

    /// Readahead for this mapping only (advice is per mapping, never per file; advisory)
    pub(crate) fn advise(&self, access: Access) {
        let advice = match access {
            Access::Normal => return,
            Access::Random => memmap2::Advice::Random,
            Access::Sequential => memmap2::Advice::Sequential,
        };
        if let Some(map) = &self.mmap {
            let _ = map.advise(advice);
        }
    }
}

/// How a mapping is read, for the kernel's readahead
///
/// - `Random` = point lookups (one 4 KiB fault each, not a 128 KiB window)
/// - `Sequential` = one pass start to end (larger readahead, pages reclaimed sooner)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Normal,
    Random,
    Sequential,
}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mapping").field("len", &self.bytes.len()).finish_non_exhaustive()
    }
}
