//! File layer every index writes through: [`RealFs`] in production, `SimFs` under crash tests
//!
//! - positional I/O only (a partial write can never move where the next one lands)
//! - new file / rename / removal durable only after [`Fs::sync_dir`] on its parent
//! - maps read-only; callers bound reads by committed lengths (`docs/design/durability.md`)

use std::{fmt, io, ops::Range, path::Path, sync::Arc};

use bytes::Bytes;

mod real;
#[cfg(any(test, feature = "testing"))]
mod sim;

pub(crate) use real::background_priority;
pub use real::RealFs;
#[cfg(any(test, feature = "testing"))]
pub use sim::{CrashState, SimFs};

/// Directory operations (Linux semantics; `docs/design/durability.md` §3)
pub trait Fs: Send + Sync + fmt::Debug + 'static {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;

    /// Read + write, created empty if absent, never truncated
    fn open(&self, path: &Path) -> io::Result<Arc<dyn FileHandle>>;

    /// `None` when absent
    fn open_existing(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>>;

    /// Read only, `None` when absent (offline verify: beside a live writer, never writes)
    fn open_read_only(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>>;

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    fn remove(&self, path: &Path) -> io::Result<()>;

    /// Entry names directly under `dir`, sorted
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;

    fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// Exclusive for the guard's life; held elsewhere = `ErrorKind::WouldBlock`
    fn lock(&self, path: &Path) -> io::Result<LockGuard>;
}

/// Open file
pub trait FileHandle: Send + Sync + fmt::Debug {
    fn len(&self) -> io::Result<u64>;

    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    fn read_exact_at(&self, buf: &mut [u8], at: u64) -> io::Result<()>;

    fn write_all_at(&self, buf: &[u8], at: u64) -> io::Result<()>;

    fn set_len(&self, len: u64) -> io::Result<()>;

    /// Content and length durable on return
    fn sync_data(&self) -> io::Result<()>;

    /// Writeback of `range` started: no wait, no durability (bounds dirty pages ahead of
    /// `sync_data`; RocksDB `bytes_per_sync`)
    fn write_behind(&self, range: Range<u64>) -> io::Result<()>;

    /// Read-only view of the whole file; `None` when empty (mmap refuses zero length)
    fn map(&self) -> io::Result<Option<Mapping>>;
}

impl dyn FileHandle {
    pub(crate) fn read_all(&self) -> io::Result<Vec<u8>> {
        let len = usize::try_from(self.len()?).map_err(io::Error::other)?;
        let mut bytes = vec![0; len];
        self.read_exact_at(&mut bytes, 0)?;
        Ok(bytes)
    }
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
#[derive(Clone)]
pub struct Mapping {
    bytes: Bytes,
    advice: Option<Arc<memmap2::Mmap>>,
}

impl Mapping {
    pub(crate) fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    /// `MADV_WILLNEED` over `range` (advisory: a failure costs page faults, not correctness)
    pub(crate) fn will_need(&self, range: Range<usize>) {
        if let Some(map) = &self.advice {
            let _ = map.advise_range(memmap2::Advice::WillNeed, range.start, range.len());
        }
    }

    /// Readahead for this mapping only (advice is per mapping, never per file; advisory)
    pub(crate) fn advise(&self, access: Access) {
        let advice = match access {
            Access::Normal => return,
            Access::Random => memmap2::Advice::Random,
            Access::Sequential => memmap2::Advice::Sequential,
        };
        if let Some(map) = &self.advice {
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
