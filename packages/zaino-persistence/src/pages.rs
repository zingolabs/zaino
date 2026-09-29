//! Page checksums: the one disk-integrity check every index file shares
//!
//! ```text
//! <file>       append-only data
//! <file>.crc   CRC-32 LE per complete 4 KiB page of <file>
//! MANIFEST     per file: committed length + CRC-32 of the partial tail page (`Sealed`)
//! ```
//!
//! - a complete page never changes, so its CRC lives beside it; the tail page still grows, so
//!   its CRC rides the manifest (a crash never leaves a committed page with a stale CRC)
//! - integrity only: what the bytes mean was settled before they were written; open checks
//!   lengths and the tail page, a read checks each page the first time it touches it

use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use zaino_primitives::types::Height;

use crate::{
    fs::{Access, FileHandle, Fs, Mapping},
    manifest::{BodyReader, ManifestError},
};

pub const PAGE: usize = 4096;

const SUM: usize = 4;

/// Dirty bytes per file before writeback starts (RocksDB `bytes_per_sync` recommendation)
const WRITE_BEHIND: u64 = 1 << 20;

/// Operator remedy in a checksum-mismatch panic
const CORRUPTION: &str = "on-disk corruption: stop zainod, run `zainod verify`, resync";

/// A file's committed state, as its owner's manifest records it
///
/// - `tail` = CRC-32 of the bytes after the last complete page (CRC of nothing when none)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sealed {
    pub len: u64,
    pub tail: u32,
}

impl Sealed {
    pub const EMPTY: Self = Self { len: 0, tail: 0 };

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.tail.to_le_bytes());
    }

    pub fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        Ok(Self { len: body.u64()?, tail: body.u32()? })
    }

    fn full_pages(&self) -> u64 {
        self.len / PAGE as u64
    }
}

/// `<file>.crc`
pub fn sums_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".crc");
    PathBuf::from(name)
}

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error("page io: {0}")]
    Io(#[from] io::Error),

    #[error("{path} is {have} bytes, the committed state needs {need}")]
    Lost { path: PathBuf, have: u64, need: u64 },

    #[error("{path}: tail page fails its checksum")]
    Tail { path: PathBuf },
}

/// The write side of one append-only file and its checksums
///
/// - `unsealed_sums` = CRCs of pages completed since the last seal, from page `sealed_pages`
#[derive(Debug)]
pub struct PagedFile {
    path: PathBuf,
    data: Arc<dyn FileHandle>,
    sums: Arc<dyn FileHandle>,
    len: u64,
    tail_page: Vec<u8>,
    unsealed_sums: Vec<u8>,
    sealed_pages: u64,
    writeback_from: u64,
}

impl PagedFile {
    /// Opens `path` at `sealed` (fresh = [`Sealed::EMPTY`]): bytes past it dropped, a shorter
    /// file refused, the tail page read back and checked (≤ 4 KiB)
    pub fn open(fs: &dyn Fs, path: &Path, sealed: Sealed) -> Result<Self, PageError> {
        let data = fs.open(path)?;
        let sums = fs.open(&sums_path(path))?;
        truncate(data.as_ref(), path, sealed.len)?;
        truncate(sums.as_ref(), &sums_path(path), sealed.full_pages() * SUM as u64)?;

        let tail_at = sealed.full_pages() * PAGE as u64;
        let mut tail_page = vec![0; usize::try_from(sealed.len - tail_at).expect("tail < PAGE")];
        data.read_exact_at(&mut tail_page, tail_at)?;
        if crc32fast::hash(&tail_page) != sealed.tail {
            return Err(PageError::Tail { path: path.to_owned() });
        }
        tail_page.reserve(PAGE - tail_page.len());

        Ok(Self {
            path: path.to_owned(),
            data,
            sums,
            len: sealed.len,
            tail_page,
            unsealed_sums: Vec::new(),
            sealed_pages: sealed.full_pages(),
            writeback_from: sealed.len,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written (sealed or not)
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends at the end; neither durable nor sealed until [`seal`](Self::seal)
    ///
    /// - writeback started per `WRITE_BEHIND` appended (`seal`'s fsync finds little dirty)
    pub fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        self.data.write_all_at(bytes, self.len)?;
        self.len += bytes.len() as u64;
        if self.len - self.writeback_from >= WRITE_BEHIND {
            self.data.write_behind(self.writeback_from..self.len)?;
            self.writeback_from = self.len;
        }
        while !bytes.is_empty() {
            let take = (PAGE - self.tail_page.len()).min(bytes.len());
            self.tail_page.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.tail_page.len() == PAGE {
                let sum = crc32fast::hash(&self.tail_page);
                self.unsealed_sums.extend_from_slice(&sum.to_le_bytes());
                self.tail_page.clear();
            }
        }
        Ok(())
    }

    /// Data fsync → completed pages' CRCs written + fsynced; the seal the owner's next manifest
    /// must carry (committed only once that manifest is durable)
    pub fn seal(&mut self) -> io::Result<Sealed> {
        self.data.sync_data()?;
        if !self.unsealed_sums.is_empty() {
            self.sums.write_all_at(&self.unsealed_sums, self.sealed_pages * SUM as u64)?;
            self.sums.sync_data()?;
            self.sealed_pages += (self.unsealed_sums.len() / SUM) as u64;
            self.unsealed_sums.clear();
        }
        Ok(Sealed { len: self.len, tail: crc32fast::hash(&self.tail_page) })
    }

    /// Read view of `sealed` (a seal this file produced), keeping `previous`'s checked pages
    pub fn pages(&self, sealed: Sealed, previous: Option<&Pages>) -> io::Result<Pages> {
        let (data, sums) = (self.data.as_ref(), self.sums.as_ref());
        Pages::map(&self.path, data, sums, sealed, previous, Access::Normal)
    }
}

fn truncate(file: &dyn FileHandle, path: &Path, need: u64) -> Result<(), PageError> {
    let have = file.len()?;
    if have < need {
        return Err(PageError::Lost { path: path.to_owned(), have, need });
    }
    if have > need {
        file.set_len(need)?;
    }
    Ok(())
}

/// A sealed file, mapped: every page checked the first time a read touches it
///
/// - a failed check = bytes changed on disk after they were sealed: dies (never serves bytes
///   it cannot vouch for; `docs/design/durability.md`)
#[derive(Clone)]
pub struct Pages {
    inner: Arc<Mapped>,
}

struct Mapped {
    path: PathBuf,
    data: Bytes,
    mapping: Option<Mapping>,
    sums: Bytes,
    tail: u32,
    checked: Box<[AtomicU64]>,
}

impl std::fmt::Debug for Pages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pages")
            .field("path", &self.inner.path)
            .field("len", &self.inner.data.len())
            .finish()
    }
}

impl Pages {
    /// An immutable sealed file (a segment): opened read-only, lengths checked, nothing truncated
    ///
    /// - `access` = how this mapping is read (another mapping of the same file keeps its own)
    pub fn open(
        fs: &dyn Fs,
        path: &Path,
        sealed: Sealed,
        access: Access,
    ) -> Result<Self, PageError> {
        let missing =
            |path: &Path| PageError::Lost { path: path.to_owned(), have: 0, need: sealed.len };
        let data = fs.open_existing(path)?.ok_or_else(|| missing(path))?;
        let sums_at = sums_path(path);
        let sums = fs.open_existing(&sums_at)?.ok_or_else(|| missing(&sums_at))?;
        for (file, at, need) in [
            (&data, path, sealed.len),
            (&sums, sums_at.as_path(), sealed.full_pages() * SUM as u64),
        ] {
            let have = file.len()?;
            if have < need {
                return Err(PageError::Lost { path: at.to_owned(), have, need });
            }
        }
        Ok(Self::map(path, data.as_ref(), sums.as_ref(), sealed, None, access)?)
    }

    fn map(
        path: &Path,
        data: &dyn FileHandle,
        sums: &dyn FileHandle,
        sealed: Sealed,
        previous: Option<&Pages>,
        access: Access,
    ) -> io::Result<Self> {
        let len = usize::try_from(sealed.len).expect("sealed length fits usize");
        let mapping = data.map()?;
        let sums_mapping = sums.map()?;
        for mapped in mapping.iter().chain(&sums_mapping) {
            mapped.advise(access);
        }
        let bytes =
            mapping.as_ref().map(|mapping| mapping.bytes().slice(..len)).unwrap_or_default();
        let sums = sums_mapping
            .map(|mapping| mapping.bytes().slice(..len / PAGE * SUM))
            .unwrap_or_default();

        let pages = len.div_ceil(PAGE);
        let checked: Box<[AtomicU64]> =
            (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        if let Some(previous) = previous {
            // complete pages only: a tail page may have grown since
            let carried = previous.inner.data.len() / PAGE;
            for page in 0..carried.min(pages) {
                if previous.is_checked(page) {
                    checked[page / 64].fetch_or(1 << (page % 64), Ordering::Relaxed);
                }
            }
        }

        Ok(Self {
            inner: Arc::new(Mapped {
                path: path.to_owned(),
                data: bytes,
                mapping,
                sums,
                tail: sealed.tail,
                checked,
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.inner.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.data.is_empty()
    }

    /// `range`, checked, as a zero-copy slice of the mapping
    pub fn bytes(&self, range: Range<usize>) -> Bytes {
        self.check(range.clone());
        self.inner.data.slice(range)
    }

    /// `range`, checked
    pub fn read(&self, range: Range<usize>) -> &[u8] {
        self.check(range.clone());
        &self.inner.data[range]
    }

    /// `range` whose pages an earlier [`read`](Self::read) checked (caller's own record of it;
    /// debug-asserted): no per-page bitmap walk on a hot path
    pub(crate) fn read_unchecked(&self, range: Range<usize>) -> &[u8] {
        debug_assert!(
            (range.start / PAGE..range.end.div_ceil(PAGE)).all(|page| self.is_checked(page)),
            "{} {range:?}: read_unchecked before a checked read",
            self.inner.path.display()
        );
        &self.inner.data[range]
    }

    /// `MADV_WILLNEED` over `range` (advisory)
    pub fn will_need(&self, range: Range<usize>) {
        if let Some(mapping) = &self.inner.mapping {
            mapping.will_need(range);
        }
    }

    fn is_checked(&self, page: usize) -> bool {
        self.inner.checked[page / 64].load(Ordering::Relaxed) & (1 << (page % 64)) != 0
    }

    fn check(&self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let inner = &self.inner;
        for page in range.start / PAGE..range.end.div_ceil(PAGE) {
            if self.is_checked(page) {
                continue;
            }
            let at = page * PAGE;
            let bytes = &inner.data[at..(at + PAGE).min(inner.data.len())];
            let expected = match bytes.len() == PAGE {
                true => u32::from_le_bytes(
                    inner.sums[page * SUM..page * SUM + SUM].try_into().expect("SUM bytes"),
                ),
                false => inner.tail,
            };
            if crc32fast::hash(bytes) != expected {
                let path = inner.path.display();
                panic!("{path} page {page}: checksum mismatch ({CORRUPTION})");
            }
            inner.checked[page / 64].fetch_or(1 << (page % 64), Ordering::Relaxed);
        }
    }
}

/// What an index directory's manifest commits: its tip (last height, inclusive; `None` = empty),
/// and every file it seals (paths relative to the directory)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedFiles {
    pub tip: Option<Height>,
    pub files: Vec<(String, Sealed)>,
}

/// One file's offline check (plain reads: safe beside a live writer)
///
/// - `orphaned_bytes` = past the committed length (uncommitted, dropped at the next open)
/// - `lost` = missing or shorter than committed
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Scrub {
    pub path: String,
    pub committed_bytes: u64,
    pub orphaned_bytes: u64,
    pub lost: bool,
    pub bad_pages: Vec<u64>,
}

impl Scrub {
    pub fn is_clean(&self) -> bool {
        !self.lost && self.bad_pages.is_empty()
    }
}

/// Reads `dir/relative` up to `sealed` and checks every page against `<file>.crc` + the tail CRC
pub fn scrub(dir: &Path, relative: &str, sealed: Sealed) -> io::Result<Scrub> {
    use std::io::Read as _;

    let path = dir.join(relative);
    let mut report = Scrub {
        path: relative.to_owned(),
        committed_bytes: sealed.len,
        orphaned_bytes: 0,
        lost: false,
        bad_pages: Vec::new(),
    };
    let (data, sums) = match (std::fs::File::open(&path), std::fs::read(sums_path(&path))) {
        (Ok(data), Ok(sums)) => (data, sums),
        (Err(error), _) | (_, Err(error)) if error.kind() == io::ErrorKind::NotFound => {
            report.lost = true;
            return Ok(report);
        }
        (Err(error), _) | (_, Err(error)) => return Err(error),
    };
    let len = data.metadata()?.len();
    if len < sealed.len || (sums.len() as u64) < sealed.full_pages() * SUM as u64 {
        report.lost = true;
        return Ok(report);
    }
    report.orphaned_bytes = len - sealed.len;

    let mut reader = io::BufReader::with_capacity(1 << 20, data).take(sealed.len);
    let mut page = vec![0u8; PAGE];
    for index in 0..sealed.len.div_ceil(PAGE as u64) {
        let size = (sealed.len - index * PAGE as u64).min(PAGE as u64) as usize;
        reader.read_exact(&mut page[..size])?;
        let expected = match size == PAGE {
            true => {
                let at = usize::try_from(index).expect("page index fits usize") * SUM;
                u32::from_le_bytes(sums[at..at + SUM].try_into().expect("SUM bytes"))
            }
            false => sealed.tail,
        };
        if crc32fast::hash(&page[..size]) != expected {
            report.bad_pages.push(index);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SimFs;

    /// Appends across page boundaries seal to the same CRCs a from-scratch hash gives; reopen
    /// continues the tail; a flipped committed byte = a panic on first touch and a scrub fault
    #[test]
    fn seals_reopen_and_every_committed_byte_is_checked() {
        let fs = SimFs::new();
        let path = Path::new("/p/data");
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let bytes: Vec<u8> = (0..3 * PAGE + 100).map(|n| (n * 7 % 251) as u8).collect();

        let mut file = PagedFile::open(fs.as_ref(), path, Sealed::EMPTY).expect("open");
        file.append(&bytes[..PAGE - 3]).expect("append");
        let first = file.seal().expect("seal");
        let first_tail = crc32fast::hash(&bytes[..PAGE - 3]);
        assert_eq!(first, Sealed { len: (PAGE - 3) as u64, tail: first_tail });
        drop(file);

        let mut file = PagedFile::open(fs.as_ref(), path, first).expect("reopen");
        file.append(&bytes[PAGE - 3..]).expect("append");
        let sealed = file.seal().expect("seal");
        let tail = crc32fast::hash(&bytes[3 * PAGE..]);
        assert_eq!(sealed, Sealed { len: bytes.len() as u64, tail });
        let sums: Vec<u8> = bytes
            .chunks(PAGE)
            .take(3)
            .flat_map(|page| crc32fast::hash(page).to_le_bytes())
            .collect();
        assert_eq!(fs.contents(&sums_path(path)).expect("sums"), sums);

        let pages = file.pages(sealed, None).expect("pages");
        assert_eq!(pages.read(PAGE - 10..PAGE + 10), &bytes[PAGE - 10..PAGE + 10]);
        assert_eq!(pages.bytes(0..bytes.len()), bytes);

        // a longer file: the uncommitted bytes past the seal are dropped at open
        fs.corrupt(path, |data| data.extend_from_slice(&[9; 17]));
        drop(file);
        PagedFile::open(fs.as_ref(), path, sealed).expect("reopen past orphans");
        assert_eq!(fs.contents(path).expect("data"), bytes);

        fs.corrupt(path, |data| data[PAGE + 5] ^= 1);
        let pages = Pages::open(fs.as_ref(), path, sealed, Access::Normal).expect("lengths intact");
        assert_eq!(pages.read(0..PAGE), &bytes[..PAGE], "untouched page");
        let touched = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pages.read(PAGE..PAGE + 1).to_vec()
        }));
        let message = touched
            .expect_err("a corrupt page never serves")
            .downcast::<String>()
            .expect("formatted");
        assert!(message.contains("page 1: checksum mismatch"), "{message}");

        fs.corrupt(path, |data| data[3 * PAGE + 1] ^= 1);
        assert!(matches!(PagedFile::open(fs.as_ref(), path, sealed), Err(PageError::Tail { .. })));
        fs.corrupt(path, |data| data.truncate(10));
        let short = PagedFile::open(fs.as_ref(), path, sealed);
        assert!(matches!(short, Err(PageError::Lost { have: 10, .. })));
    }

    /// Offline scrub over a real file written past `WRITE_BEHIND` (writeback hint issued): clean,
    /// a bad page named, orphans counted, a short file lost
    #[test]
    fn scrub_names_bad_pages_orphans_and_losses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fs = crate::fs::RealFs::shared();
        let path = dir.path().join("data");
        let bytes: Vec<u8> = (0..WRITE_BEHIND as usize + 2 * PAGE + 9).map(|n| n as u8).collect();
        let mut file = PagedFile::open(fs.as_ref(), &path, Sealed::EMPTY).expect("open");
        file.append(&bytes).expect("append");
        let sealed = file.seal().expect("seal");

        let clean = scrub(dir.path(), "data", sealed).expect("scrub");
        assert!(clean.is_clean(), "{clean:?}");
        assert_eq!(clean.committed_bytes, bytes.len() as u64);

        let mut damaged = bytes.clone();
        damaged[PAGE + 1] ^= 1;
        damaged.extend_from_slice(&[0; 5]);
        std::fs::write(&path, &damaged).expect("write");
        let report = scrub(dir.path(), "data", sealed).expect("scrub");
        assert_eq!((report.bad_pages.clone(), report.orphaned_bytes), (vec![1], 5));

        std::fs::write(&path, &bytes[..PAGE]).expect("truncate");
        assert!(scrub(dir.path(), "data", sealed).expect("scrub").lost);
    }
}
