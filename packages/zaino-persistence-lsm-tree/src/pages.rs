//! Page checksums: the one disk-integrity check every index file shares
//!
//! ```text
//! <file>       append-only data
//! <file>.crc   CRC-32 LE per complete 4 KiB page of <file>
//! MANIFEST     per file: committed length, the tail page's CRC, a digest of <file>.crc (`Sealed`)
//! ```
//!
//! - complete page never changes → its CRC in `<file>.crc`; growing tail page → CRC in manifest
//!   (no committed page ever left with a stale CRC)
//! - CRC covers page index + bytes (page moved with its CRC still fails)
//! - manifest digest of `<file>.crc`, checked at open: manifest pins every CRC, each CRC its page
//!   (stale page + stale CRC after a lost write still fails)
//! - integrity only (meaning settled before the write)
//! - open checks lengths, tail page, digest; a read checks each page on first touch

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
use zaino_persistence::{Problem, Unit};

use crate::{
    fs::{Access, FileHandle, Fs, Mapping},
    manifest::{BodyReader, ManifestError},
};

pub(crate) const PAGE: usize = 4096;
const PAGE_U64: u64 = PAGE as u64;

const SUM: usize = 4;
const SUM_U64: u64 = SUM as u64;

/// Dirty bytes per file before writeback starts (RocksDB `bytes_per_sync` recommendation)
const WRITE_BEHIND: u64 = 1 << 20;

/// [`Reserve`] growth step bounds (step = file's size, clamped)
const MIN_RESERVE: u64 = 64 << 10;
const MAX_RESERVE: u64 = 64 << 20;

/// Zeros written per call while growing a reserve
static ZEROS: [u8; 1 << 20] = [0; 1 << 20];

/// File's committed state, as its owner's manifest records it
///
/// - `tail` = [`page_sum`] of the bytes after the last complete page
/// - `sums` = CRC-32 of `<file>.crc` through the last complete page (CRC of nothing when none)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sealed {
    pub(crate) len: u64,
    pub(crate) tail: u32,
    pub(crate) sums: u32,
}

impl Sealed {
    /// Nothing committed
    pub(crate) const EMPTY: Self = Self {
        len: 0,
        tail: 0,
        sums: 0,
    };

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.tail.to_le_bytes());
        out.extend_from_slice(&self.sums.to_le_bytes());
    }

    pub(crate) fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        Ok(Self {
            len: body.u64()?,
            tail: body.u32()?,
            sums: body.u32()?,
        })
    }

    fn full_pages(&self) -> u64 {
        self.len / PAGE_U64
    }

    /// Bytes of `<file>.crc` this seal covers
    fn sums_len(&self) -> u64 {
        self.full_pages().saturating_mul(SUM as u64)
    }
}

/// Page `index`'s checksum: CRC-32 of the index (u64 LE), then the page's bytes
fn page_sum(index: u64, bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&index.to_le_bytes());
    hasher.update(bytes);
    hasher.finalize()
}

/// Tail page's checksum; 0 = no tail (so [`Sealed::EMPTY`] can be a constant)
fn tail_sum(index: u64, bytes: &[u8]) -> u32 {
    match bytes.is_empty() {
        true => 0,
        false => page_sum(index, bytes),
    }
}

/// `digest` (CRC-32 of the checksums so far) extended by `more` checksum bytes
fn extend_digest(digest: u32, more: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new_with_initial(digest);
    hasher.update(more);
    hasher.finalize()
}

/// `<file>.crc`
pub(crate) fn sums_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".crc");
    PathBuf::from(name)
}

/// Checksum `at` in a checksum list; `None` = past its end
fn sum_at(sums: &[u8], page: u64) -> Option<u32> {
    let at = usize::try_from(page).ok()?.checked_mul(SUM)?;
    let bytes = sums.get(at..at.checked_add(SUM)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PageError {
    #[error("page io: {0}")]
    Io(#[from] io::Error),

    #[error("{path} is {have} bytes, the committed state needs {need}")]
    Lost { path: PathBuf, have: u64, need: u64 },

    #[error("{path}: tail page fails its checksum")]
    Tail { path: PathBuf },

    #[error("{path}: page checksums differ from the digest the manifest committed")]
    Sums { path: PathBuf },

    /// Bytes changed on disk after the seal
    #[error("{path} page {page}: checksum mismatch (on-disk corruption: stop, verify, resync)")]
    Corrupt { path: PathBuf, page: usize },

    #[error("{path}: read {start}..{end} outside its {len} sealed bytes")]
    OutOfRange {
        path: PathBuf,
        start: usize,
        end: usize,
        len: usize,
    },

    /// Debug builds only: an unchecked read of a page no checked read covered
    #[error("{path} page {page}: unchecked read before a checked one")]
    Unchecked { path: PathBuf, page: usize },

    #[error("{path}: {what} overflows")]
    Overflow { path: PathBuf, what: &'static str },
}

fn overflow(path: &Path, what: &'static str) -> PageError {
    PageError::Overflow {
        path: path.to_owned(),
        what,
    }
}

/// How a paged file grows + who makes its name durable
///
/// - `Segment` = written once, sealed for good (LSM): grows by exactly the appends; caller syncs
///   the linking directory
/// - `Log` = appended commit after commit: linked durably at creation, grown into a [`Reserve`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Segment,
    Log,
}

/// Zeroed, flushed room past the end of a [`FileKind::Log`] (appends below EOF = no metadata
/// change: seal's `fdatasync` never waits on the journal / other files' writeback)
///
/// - zeros, not `fallocate` (unwritten extents = metadata change on first write); usage.md
///   "Write-ahead reserve"
#[derive(Debug)]
struct Reserve {
    end: u64,
}

impl Reserve {
    /// Room up to `end`, grown (zero-filled, then flushed) when short
    fn cover(&mut self, file: &dyn FileHandle, end: u64) -> io::Result<()> {
        if end <= self.end {
            return Ok(());
        }
        let too_large = || io::Error::other("reserve past u64");
        let step = self.end.clamp(MIN_RESERVE, MAX_RESERVE);
        let grown = end
            .max(self.end.checked_add(step).ok_or_else(too_large)?)
            .checked_next_multiple_of(PAGE as u64)
            .ok_or_else(too_large)?;
        let mut at = self.end;
        while let Some(left) = grown.checked_sub(at).filter(|left| *left > 0) {
            let take = usize::try_from(left).map_or(ZEROS.len(), |left| left.min(ZEROS.len()));
            let zeros = ZEROS.get(..take).unwrap_or(&ZEROS);
            file.write_all_at(zeros, at)?;
            at = at.checked_add(zeros.len() as u64).ok_or_else(too_large)?;
        }
        file.sync_data()?;
        self.end = grown;
        Ok(())
    }
}

/// Write side of one append-only file + its checksums
///
/// - `unsealed_sums` = CRCs of pages completed since the last seal, from page `sealed_pages`
/// - `sums_digest` = CRC-32 of every sealed page checksum (what the next seal extends)
/// - `reserves` = data's + checksums' [`Reserve`]s (`None` = [`FileKind::Segment`])
#[derive(Debug)]
pub(crate) struct PagedFile {
    path: PathBuf,
    data: Arc<dyn FileHandle>,
    sums: Arc<dyn FileHandle>,
    len: u64,
    tail_page: Vec<u8>,
    unsealed_sums: Vec<u8>,
    sealed_pages: u64,
    sums_digest: u32,
    writeback_from: u64,
    reserves: Option<[Reserve; 2]>,
}

impl PagedFile {
    /// `path` at `sealed` (fresh = [`Sealed::EMPTY`]): bytes past it dropped, shorter file
    /// refused, tail page + checksums' digest read back and checked (`.crc` = 1/1024 of the data)
    ///
    /// - [`FileKind::Log`] with either file created here → directory synced (survives a crash
    ///   before any manifest names it; commits never sync directories)
    pub(crate) fn open(
        fs: &dyn Fs,
        path: &Path,
        sealed: Sealed,
        kind: FileKind,
    ) -> Result<Self, PageError> {
        let created =
            fs.open_existing(path)?.is_none() || fs.open_existing(&sums_path(path))?.is_none();
        let data = fs.open(path)?;
        let sums = fs.open(&sums_path(path))?;
        if kind == FileKind::Log && created {
            fs.sync_dir(path.parent().unwrap_or(Path::new("")))?;
        }
        let sums_len = sealed.sums_len();
        truncate(data.as_ref(), path, sealed.len)?;
        truncate(sums.as_ref(), &sums_path(path), sums_len)?;

        let tail_at = sealed.full_pages().saturating_mul(PAGE as u64);
        let tail_len = sealed.len.saturating_sub(tail_at);
        let tail_len = usize::try_from(tail_len).map_err(|_| overflow(path, "tail page"))?;
        let mut tail_page = vec![0; tail_len];
        data.read_exact_at(&mut tail_page, tail_at)?;
        if tail_sum(sealed.full_pages(), &tail_page) != sealed.tail {
            return Err(PageError::Tail {
                path: path.to_owned(),
            });
        }
        tail_page.reserve(PAGE.saturating_sub(tail_page.len()));
        let sums_len_bytes =
            usize::try_from(sums_len).map_err(|_| overflow(path, "checksum list"))?;
        let mut committed_sums = vec![0; sums_len_bytes];
        sums.read_exact_at(&mut committed_sums, 0)?;
        if crc32fast::hash(&committed_sums) != sealed.sums {
            return Err(PageError::Sums {
                path: path.to_owned(),
            });
        }

        Ok(Self {
            path: path.to_owned(),
            data,
            sums,
            len: sealed.len,
            tail_page,
            unsealed_sums: Vec::new(),
            sealed_pages: sealed.full_pages(),
            sums_digest: sealed.sums,
            writeback_from: sealed.len,
            reserves: (kind == FileKind::Log)
                .then_some([Reserve { end: sealed.len }, Reserve { end: sums_len }]),
        })
    }

    /// Bytes written (sealed or not)
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// At the end; neither durable nor sealed until [`seal`](Self::seal)
    ///
    /// - writeback started per `WRITE_BEHIND` appended (`seal`'s fsync finds little dirty)
    pub(crate) fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let too_large = || io::Error::other(format!("{} past u64 bytes", self.path.display()));
        let end = self
            .len
            .checked_add(bytes.len() as u64)
            .ok_or_else(too_large)?;
        if let Some([data, _]) = &mut self.reserves {
            data.cover(self.data.as_ref(), end)?;
        }
        self.data.write_all_at(bytes, self.len)?;
        self.len = end;
        if self.len.saturating_sub(self.writeback_from) >= WRITE_BEHIND {
            self.data.write_behind(self.writeback_from..self.len)?;
            self.writeback_from = self.len;
        }
        while !bytes.is_empty() {
            let take = PAGE.saturating_sub(self.tail_page.len()).min(bytes.len());
            let (head, rest) = bytes.split_at_checked(take).unwrap_or((bytes, &[]));
            self.tail_page.extend_from_slice(head);
            bytes = rest;
            if self.tail_page.len() == PAGE {
                let completed = (self.unsealed_sums.len() / SUM) as u64;
                let index = self
                    .sealed_pages
                    .checked_add(completed)
                    .ok_or_else(too_large)?;
                let sum = page_sum(index, &self.tail_page);
                self.unsealed_sums.extend_from_slice(&sum.to_le_bytes());
                self.tail_page.clear();
            }
        }
        Ok(())
    }

    /// Synced data out of page cache (advisory)
    pub(crate) fn drop_cache(&self) -> io::Result<()> {
        self.data.drop_cache()
    }

    /// Data fsync → completed pages' CRCs written + fsynced → seal for the owner's next manifest
    /// (committed only once that manifest is durable)
    pub(crate) fn seal(&mut self) -> io::Result<Sealed> {
        self.data.sync_data()?;
        if !self.unsealed_sums.is_empty() {
            let too_large = || io::Error::other(format!("{} past u64 pages", self.path.display()));
            let at = self
                .sealed_pages
                .checked_mul(SUM as u64)
                .ok_or_else(too_large)?;
            let end = at
                .checked_add(self.unsealed_sums.len() as u64)
                .ok_or_else(too_large)?;
            if let Some([_, sums]) = &mut self.reserves {
                sums.cover(self.sums.as_ref(), end)?;
            }
            self.sums.write_all_at(&self.unsealed_sums, at)?;
            self.sums.sync_data()?;
            let completed = (self.unsealed_sums.len() / SUM) as u64;
            self.sealed_pages = self
                .sealed_pages
                .checked_add(completed)
                .ok_or_else(too_large)?;
            self.sums_digest = extend_digest(self.sums_digest, &self.unsealed_sums);
            self.unsealed_sums.clear();
        }
        let tail = tail_sum(self.sealed_pages, &self.tail_page);
        Ok(Sealed {
            len: self.len,
            tail,
            sums: self.sums_digest,
        })
    }

    /// Read view of `sealed` (a seal this file produced), keeping `previous`'s checked pages
    pub(crate) fn pages(
        &self,
        sealed: Sealed,
        previous: Option<&Pages>,
    ) -> Result<Pages, PageError> {
        let (data, sums) = (self.data.as_ref(), self.sums.as_ref());
        Pages::map(&self.path, data, sums, sealed, previous, Access::Normal)
    }
}

fn truncate(file: &dyn FileHandle, path: &Path, need: u64) -> Result<(), PageError> {
    let have = file.len()?;
    if have < need {
        return Err(PageError::Lost {
            path: path.to_owned(),
            have,
            need,
        });
    }
    if have > need {
        file.set_len(need)?;
    }
    Ok(())
}

/// `(sealed bytes, bytes of them in page cache)` over `pages` (`None` = any one not knowable)
pub(crate) fn footprint<'a>(pages: impl IntoIterator<Item = &'a Pages>) -> (u64, Option<u64>) {
    pages.into_iter().fold(
        (0, Some(0)),
        |(bytes, cached): (u64, Option<u64>), pages| {
            let cached = cached
                .zip(pages.resident_bytes())
                .map(|(sum, resident)| sum.saturating_add(resident));
            (bytes.saturating_add(pages.len() as u64), cached)
        },
    )
}

/// Sealed file, mapped: every page checked on a read's first touch
///
/// - failed check = bytes changed on disk after the seal → `PageError::Corrupt` (never serves
///   unverified bytes; `docs/durability.md`)
#[derive(Clone)]
pub(crate) struct Pages {
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
    /// Immutable sealed file (segment): read-only, lengths + checksums' digest checked, nothing
    /// truncated
    ///
    /// - `access` = this mapping's read pattern (other mappings of the file keep their own)
    pub(crate) fn open(
        fs: &dyn Fs,
        path: &Path,
        sealed: Sealed,
        access: Access,
    ) -> Result<Self, PageError> {
        let missing = |path: &Path| PageError::Lost {
            path: path.to_owned(),
            have: 0,
            need: sealed.len,
        };
        // read only: sealed segments are read-only files
        let data = fs.open_read_only(path)?.ok_or_else(|| missing(path))?;
        let sums_at = sums_path(path);
        let sums = fs
            .open_read_only(&sums_at)?
            .ok_or_else(|| missing(&sums_at))?;
        for (file, at, need) in [
            (&data, path, sealed.len),
            (&sums, sums_at.as_path(), sealed.sums_len()),
        ] {
            let have = file.len()?;
            if have < need {
                return Err(PageError::Lost {
                    path: at.to_owned(),
                    have,
                    need,
                });
            }
        }
        let pages = Self::map(path, data.as_ref(), sums.as_ref(), sealed, None, access)?;
        if crc32fast::hash(&pages.inner.sums) != sealed.sums {
            return Err(PageError::Sums {
                path: path.to_owned(),
            });
        }
        Ok(pages)
    }

    fn map(
        path: &Path,
        data: &dyn FileHandle,
        sums: &dyn FileHandle,
        sealed: Sealed,
        previous: Option<&Pages>,
        access: Access,
    ) -> Result<Self, PageError> {
        let len = usize::try_from(sealed.len).map_err(|_| overflow(path, "sealed length"))?;
        let sums_len =
            usize::try_from(sealed.sums_len()).map_err(|_| overflow(path, "checksum list"))?;
        let mapping = data.map()?;
        let sums_mapping = sums.map()?;
        for mapped in mapping.iter().chain(&sums_mapping) {
            mapped.advise(access);
        }
        let prefix = |mapped: Option<&Mapping>, at: &Path, need: usize| {
            let bytes = mapped.map(Mapping::bytes).cloned().unwrap_or_default();
            match bytes.len() >= need {
                true => Ok(bytes.slice(..need)),
                false => Err(PageError::Lost {
                    path: at.to_owned(),
                    have: bytes.len() as u64,
                    need: need as u64,
                }),
            }
        };
        let bytes = prefix(mapping.as_ref(), path, len)?;
        let sums = prefix(sums_mapping.as_ref(), &sums_path(path), sums_len)?;

        let pages = len.div_ceil(PAGE);
        let checked: Box<[AtomicU64]> =
            (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        if let Some(previous) = previous {
            // complete pages only (tail page may have grown since)
            let carried = previous.inner.data.len() / PAGE;
            for page in 0..carried.min(pages) {
                if previous.is_checked(page) {
                    mark(&checked, page);
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

    pub(crate) fn len(&self) -> usize {
        self.inner.data.len()
    }

    /// `range`, checked, as a zero-copy slice of the mapping
    pub(crate) fn bytes(&self, range: Range<usize>) -> Result<Bytes, PageError> {
        self.check(range.clone())?;
        Ok(self.inner.data.slice(range))
    }

    /// `range`, checked
    pub(crate) fn read(&self, range: Range<usize>) -> Result<&[u8], PageError> {
        self.check(range.clone())?;
        self.within(range)
    }

    /// `range` whose pages an earlier [`read`](Self::read) checked (caller's own record of it):
    /// no per-page bitmap walk on a hot path; debug builds confirm it
    pub(crate) fn read_unchecked(&self, range: Range<usize>) -> Result<&[u8], PageError> {
        let bytes = self.within(range.clone())?;
        #[cfg(debug_assertions)]
        if let Some(page) = pages_of(&range).find(|page| !self.is_checked(*page)) {
            return Err(PageError::Unchecked {
                path: self.inner.path.clone(),
                page,
            });
        }
        Ok(bytes)
    }

    /// `range` of the sealed bytes, or `OutOfRange`
    fn within(&self, range: Range<usize>) -> Result<&[u8], PageError> {
        let data = &self.inner.data;
        data.get(range.clone())
            .ok_or_else(|| PageError::OutOfRange {
                path: self.inner.path.clone(),
                start: range.start,
                end: range.end,
                len: data.len(),
            })
    }

    /// Sealed bytes in page cache (`None` = not knowable; an empty file holds none)
    pub(crate) fn resident_bytes(&self) -> Option<u64> {
        match &self.inner.mapping {
            Some(mapping) => mapping.resident_bytes(self.inner.data.len()),
            None => Some(0),
        }
    }

    /// `MADV_WILLNEED` over `range`, clipped to the sealed bytes (advisory)
    pub(crate) fn will_need(&self, range: Range<usize>) {
        let end = range.end.min(self.inner.data.len());
        if let Some(mapping) = self.inner.mapping.as_ref().filter(|_| range.start < end) {
            mapping.will_need(range.start..end);
        }
    }

    fn is_checked(&self, page: usize) -> bool {
        self.inner
            .checked
            .get(page / 64)
            .is_some_and(|word| word.load(Ordering::Relaxed) & (1 << (page % 64)) != 0)
    }

    fn check(&self, range: Range<usize>) -> Result<(), PageError> {
        let inner = &self.inner;
        if range.start > range.end || range.end > inner.data.len() {
            return Err(PageError::OutOfRange {
                path: inner.path.clone(),
                start: range.start,
                end: range.end,
                len: inner.data.len(),
            });
        }
        for page in pages_of(&range) {
            if self.is_checked(page) {
                continue;
            }
            let at = page.saturating_mul(PAGE);
            let end = at.saturating_add(PAGE).min(inner.data.len());
            let corrupt = || PageError::Corrupt {
                path: inner.path.clone(),
                page,
            };
            let bytes = inner.data.get(at..end).ok_or_else(corrupt)?;
            let expected = match bytes.len() == PAGE {
                true => sum_at(&inner.sums, page as u64).ok_or_else(corrupt)?,
                false => inner.tail,
            };
            if page_sum(page as u64, bytes) != expected {
                return Err(corrupt());
            }
            mark(&inner.checked, page);
        }
        Ok(())
    }
}

/// Pages `range` touches (none when empty)
fn pages_of(range: &Range<usize>) -> Range<usize> {
    match range.is_empty() {
        true => 0..0,
        false => range.start / PAGE..range.end.div_ceil(PAGE),
    }
}

/// Page `page` recorded as checked (a page past the bitmap = never recorded, rechecked)
fn mark(checked: &[AtomicU64], page: usize) {
    if let Some(word) = checked.get(page / 64) {
        word.fetch_or(1 << (page % 64), Ordering::Relaxed);
    }
}

/// Stored unit, checked
///
/// - `orphaned_bytes` = past what the commit claims (interrupted commit's tail: harmless)
/// - `lost` = gone or shorter than committed; `bad_sums` = checksum list != commit's
/// - `bad_pages` = pages whose bytes != their checksum
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    pub(crate) name: String,
    pub(crate) committed_bytes: u64,
    pub(crate) orphaned_bytes: u64,
    pub(crate) lost: Option<Lost>,
    pub(crate) bad_sums: bool,
    pub(crate) bad_pages: Vec<u64>,
}

/// How a unit was lost
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lost {
    Missing,
    Truncated,
}

impl Checked {
    pub(crate) fn is_clean(&self) -> bool {
        self.lost.is_none() && !self.bad_sums && self.bad_pages.is_empty()
    }

    /// As the port reports it
    pub(crate) fn into_unit(self) -> Unit {
        let lost = self.lost.map(|lost| match lost {
            Lost::Missing => Problem::Missing,
            Lost::Truncated => Problem::Truncated,
        });
        let sums = self.bad_sums.then_some(Problem::ChecksumListMismatch);
        let pages = self
            .bad_pages
            .into_iter()
            .map(|page| Problem::ChecksumMismatch { page });
        let orphaned = (self.orphaned_bytes > 0).then_some(Problem::Orphaned {
            bytes: self.orphaned_bytes,
        });
        let problems = lost.into_iter().chain(sums).chain(pages).chain(orphaned);
        Unit {
            name: self.name,
            problems: problems.collect(),
        }
    }
}

/// `dir/relative` up to `sealed`, every page checked against `<file>.crc` + the tail CRC
///
/// - read only, no lock: safe beside a live writer
pub(crate) fn scrub(
    fs: &dyn Fs,
    dir: &Path,
    relative: &str,
    sealed: Sealed,
) -> io::Result<Checked> {
    let path = dir.join(relative);
    let mut report = Checked {
        name: relative.to_owned(),
        committed_bytes: sealed.len,
        orphaned_bytes: 0,
        lost: None,
        bad_sums: false,
        bad_pages: Vec::new(),
    };
    let (Some(data), Some(sums)) = (
        fs.open_read_only(&path)?,
        fs.open_read_only(&sums_path(&path))?,
    ) else {
        report.lost = Some(Lost::Missing);
        return Ok(report);
    };
    let (len, sums) = (data.len()?, sums.read_all()?);
    if len < sealed.len || (sums.len() as u64) < sealed.sums_len() {
        report.lost = Some(Lost::Truncated);
        return Ok(report);
    }
    report.orphaned_bytes = len.saturating_sub(sealed.len);
    let committed_len = usize::try_from(sealed.sums_len()).map_err(io::Error::other)?;
    let committed_sums = sums.get(..committed_len).unwrap_or(&sums);
    report.bad_sums = crc32fast::hash(committed_sums) != sealed.sums;

    let mut page = vec![0u8; PAGE];
    for index in 0..sealed.len.div_ceil(PAGE as u64) {
        let at = index.saturating_mul(PAGE as u64);
        let size = sealed.len.saturating_sub(at).min(PAGE as u64) as usize;
        let read = page.get_mut(..size).unwrap_or_default();
        data.read_exact_at(read, at)?;
        let expected = match size == PAGE {
            true => sum_at(&sums, index),
            false => Some(sealed.tail),
        };
        if expected != Some(page_sum(index, read)) {
            report.bad_pages.push(index);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
