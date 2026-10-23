//! [`Fs`] over `std::fs` + `memmap2` (Linux semantics; macOS and other Unix best effort)

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

use super::{
    sys::{map_read_only, start_writeback},
    too_long, FileHandle, Fs, LockGuard, Mapping,
};

/// The real filesystem
#[derive(Debug, Default, Clone, Copy)]
pub struct RealFs;

impl RealFs {
    /// Shared, as the engine holds it
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Arc::new(RealFile(file)))
    }

    fn open_existing(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        existing(OpenOptions::new().read(true).write(true).open(path))
    }

    fn open_read_only(&self, path: &Path) -> io::Result<Option<Arc<dyn FileHandle>>> {
        existing(OpenOptions::new().read(true).open(path))
    }

    fn read_uncached(&self, path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let file = match uncached(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let len = file.metadata()?.len();
        if len > limit {
            return Err(too_long(path, len, limit));
        }
        let len = usize::try_from(len).map_err(io::Error::other)?;
        match read_aligned(&file, len) {
            // a direct open accepted, the direct read refused (some FUSE / network mounts)
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                read_aligned(&cached_where_exact(path)?, len).map(Some)
            }
            read => read.map(Some),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn set_read_only(&self, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))
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
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(LockGuard {
                _held: Box::new(file),
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{} is held by another process", path.display()),
            )),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

/// Direct I/O's buffer, offset and length alignment
const DIRECT_ALIGN: usize = 4096;

/// `path` read only, past the page cache: `O_DIRECT`; a filesystem refusing it (`EINVAL`) read
/// through the cache only where that is exact ([`cached_where_exact`])
#[cfg(target_os = "linux")]
fn uncached(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let direct = i32::try_from(rustix::fs::OFlags::DIRECT.bits()).map_err(io::Error::other)?;
    match OpenOptions::new()
        .read(true)
        .custom_flags(direct)
        .open(path)
    {
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => cached_where_exact(path),
        opened => opened,
    }
}

/// `path` opened through the page cache on tmpfs, whose cache is its storage; anywhere else a
/// cached read could pass bytes the device never got, so refused
#[cfg(target_os = "linux")]
fn cached_where_exact(path: &Path) -> io::Result<File> {
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    match rustix::fs::statfs(path)?.f_type as u64 == TMPFS_MAGIC {
        true => OpenOptions::new().read(true).open(path),
        false => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "{}: the filesystem refuses direct I/O, so the commit point can't be read past \
                 the page cache (only tmpfs, whose cache is its storage, is read through it)",
                path.display()
            ),
        )),
    }
}

/// No cache bypass to fall back from: a plain open
#[cfg(not(target_os = "linux"))]
fn cached_where_exact(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

/// `path` read only, `F_NOCACHE`: best effort (pages already cached may still be served)
#[cfg(target_vendor = "apple")]
fn uncached(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().read(true).open(path)?;
    rustix::fs::fcntl_nocache(&file, true)?;
    Ok(file)
}

/// No cache bypass on this platform: a plain read
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn uncached(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

/// `len` bytes of `file` through an aligned buffer, in aligned reads (direct I/O's rules)
fn read_aligned(file: &File, len: usize) -> io::Result<Vec<u8>> {
    let too_large = || io::Error::other("an uncached read past usize");
    let padded = len
        .checked_next_multiple_of(DIRECT_ALIGN)
        .ok_or_else(too_large)?;
    let mut buffer = vec![0u8; padded.checked_add(DIRECT_ALIGN).ok_or_else(too_large)?];
    let start = buffer.as_ptr().align_offset(DIRECT_ALIGN);
    let end = start.checked_add(padded).ok_or_else(too_large)?;
    let aligned = buffer
        .get_mut(start..end)
        .ok_or_else(|| io::Error::other("no aligned window in the read buffer"))?;
    let mut done = 0usize;
    while done < len {
        let rest = aligned.get_mut(done..).unwrap_or_default();
        match file.read_at(rest, done as u64) {
            Ok(0) => break,
            Ok(read) => done = done.saturating_add(read),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    match aligned.get(..len) {
        Some(bytes) if done >= len => Ok(bytes.to_vec()),
        _ => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file shorter than its length",
        )),
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

    fn drop_cache(&self) -> io::Result<()> {
        Ok(rustix::fs::fadvise(
            &self.0,
            0,
            None,
            rustix::fs::Advice::DontNeed,
        )?)
    }

    fn map(&self, len: usize) -> io::Result<Option<Mapping>> {
        if len == 0 {
            return Ok(None);
        }
        if self.len()? < len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "a file shorter than its mapping",
            ));
        }
        let map = Arc::new(map_read_only(&self.0, len)?);
        Ok(Some(Mapping {
            bytes: Bytes::from_owner(Shared(Arc::clone(&map))),
            advice: Some(map),
        }))
    }
}

/// `Arc<Mmap>` as the `AsRef<[u8]>` owner `Bytes::from_owner` wants
struct Shared(Arc<Mmap>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every length (empty, unaligned, aligned, multi-page) read back whole past the cache;
    /// absent = `None`
    #[test]
    fn an_uncached_read_returns_exactly_the_files_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = RealFs;
        for len in [0, 1, 4095, 4096, 4097, 3 * 4096 + 17] {
            let path = dir.path().join(format!("file-{len}"));
            let bytes: Vec<u8> = (0..len).map(|n| (n % 251) as u8).collect();
            std::fs::write(&path, &bytes).expect("write");
            assert_eq!(
                fs.read_uncached(&path, len as u64).expect("read"),
                Some(bytes),
                "{len} bytes"
            );
            if len > 0 {
                let refused = fs.read_uncached(&path, len as u64 - 1).map(drop);
                assert!(
                    refused.is_err_and(|error| error.kind() == io::ErrorKind::InvalidData),
                    "{len} bytes past a {}-byte limit",
                    len - 1
                );
            }
        }
        let absent = fs
            .read_uncached(&dir.path().join("absent"), 0)
            .expect("read");
        assert_eq!(absent, None);
    }

    /// Where direct I/O is refused: read through the cache on tmpfs only (its cache is its
    /// storage), refused anywhere else (a cached read could pass bytes the device never got)
    #[cfg(target_os = "linux")]
    #[test]
    fn a_cached_read_stands_in_for_a_direct_one_on_tmpfs_only() {
        const TMPFS_MAGIC: u64 = 0x0102_1994;
        let check = |root: tempfile::TempDir| {
            let path = root.path().join("file");
            std::fs::write(&path, [7; 10]).expect("write");
            let on_tmpfs =
                rustix::fs::statfs(root.path()).expect("statfs").f_type as u64 == TMPFS_MAGIC;
            let opened = cached_where_exact(&path);
            match on_tmpfs {
                true => assert!(opened.is_ok(), "tmpfs: {opened:?}"),
                false => assert!(
                    opened.is_err_and(|error| error.kind() == io::ErrorKind::Unsupported),
                    "not tmpfs: refused"
                ),
            }
            on_tmpfs
        };
        // the temp directory's filesystem, whichever it is; tmpfs too where there is one
        check(tempfile::tempdir().expect("tempdir"));
        if std::path::Path::new("/dev/shm").is_dir() {
            let shm = tempfile::tempdir_in("/dev/shm").expect("tempdir on tmpfs");
            assert!(check(shm), "/dev/shm is tmpfs");
        }
    }
}
