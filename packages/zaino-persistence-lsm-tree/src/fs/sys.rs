//! The crate's only `unsafe`: one wrapper per syscall or mapping, each with its soundness note
//!
//! - `map_read_only` (mmap), `start_writeback` (`sync_file_range`), `resident_bytes`
//!   (`mincore`), `background_priority` (`ioprio_set` + `setpriority`)
//! - off Linux: safe no-op / `Unsupported` stand-ins

use std::{fs::File, io, ops::Range};

use memmap2::Mmap;

/// Bytes of `map[..len]` in page cache (`mincore`; sound: `map` = a live mapping, `len` <= its
/// length, the vector one byte per page)
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn resident_bytes(map: &Mmap, len: usize) -> io::Result<u64> {
    if len > map.len() {
        let message = format!("residency of {len} bytes past a {}-byte mapping", map.len());
        return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    }
    if len == 0 {
        return Ok(0);
    }
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|page| *page > 0)
        .ok_or_else(|| io::Error::other("no page size"))?;
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
pub(crate) fn resident_bytes(_: &Mmap, _: usize) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
}

/// - sound: mapped bytes below a committed length are never rewritten or truncated after
///   publication (`docs/persistence-engine.md`)
#[allow(unsafe_code)]
pub(super) fn map_read_only(file: &File) -> io::Result<Mmap> {
    unsafe { Mmap::map(file) }
}

/// `sync_file_range(SYNC_FILE_RANGE_WRITE)`: dirty pages in `range` queued, no wait (sound: fd
/// borrowed from a live `File`, no memory passed)
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(super) fn start_writeback(file: &File, range: Range<u64>) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;

    let offset = i64::try_from(range.start).map_err(io::Error::other)?;
    let len = range
        .end
        .checked_sub(range.start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "writeback range inverted"))?;
    let len = i64::try_from(len).map_err(io::Error::other)?;
    let flags = libc::SYNC_FILE_RANGE_WRITE;
    match unsafe { libc::sync_file_range(file.as_raw_fd(), offset, len, flags) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// No write-behind hint off Linux (`sync_data` alone flushes)
#[cfg(not(target_os = "linux"))]
pub(super) fn start_writeback(_: &File, _: Range<u64>) -> io::Result<()> {
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
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            thread,
            io_priority,
        );
        libc::setpriority(libc::PRIO_PROCESS as _, thread as _, NICE);
    }
}

/// No thread priorities off Linux
#[cfg(not(target_os = "linux"))]
pub(crate) fn background_priority() {}
