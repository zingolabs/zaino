//! Page checksums: the one disk-integrity check every index file shares
//!
//! ```text
//! <file>       append-only data
//! <file>.crc   CRC-32 LE per complete 4 KiB page of <file>
//! MANIFEST     per file: durable length, the tail page's CRC, a digest of <file>.crc (`Sealed`)
//! ```
//!
//! - complete page never changes → its CRC in `<file>.crc`
//! - growing tail page → CRC in manifest (no durable page ever left with a stale CRC)
//! - CRC covers page index + bytes (page moved with its CRC still fails)
//! - manifest digest of `<file>.crc`, checked at open: manifest pins every CRC, each CRC its page
//!   (stale page + stale CRC after a lost write still fails)
//! - integrity only (meaning settled before the write)
//! - open checks lengths, tail page, digest
//! - read checks each page on first touch

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

use crate::{
    fs::{Access, Durability, FileHandle, Fs, Mapping, ReadHandle},
    manifest::{BodyReader, ManifestError},
    port::Checked,
};

pub(crate) const PAGE: usize = 4096;

const SUM: usize = 4;

/// Dirty bytes per file before writeback starts (RocksDB `bytes_per_sync` recommendation)
const WRITE_BEHIND: u64 = 1 << 20;

/// [`Reserve`] growth step bounds (step = file's size, clamped)
const MIN_RESERVE: u64 = 64 << 10;
const MAX_RESERVE: u64 = 64 << 20;

/// Zeros written per call while growing a reserve
static ZEROS: [u8; 1 << 20] = [0; 1 << 20];

/// Operator remedy in a checksum-mismatch panic
const CORRUPTION: &str = "on-disk corruption: stop zainod, run `zainod verify`, resync";

/// File's durable state, as its owner's manifest records it
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
    /// Nothing durable
    pub(crate) const EMPTY: Self = Self { len: 0, tail: 0, sums: 0 };

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&self.tail.to_le_bytes());
        out.extend_from_slice(&self.sums.to_le_bytes());
    }

    pub(crate) fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        Ok(Self { len: body.u64()?, tail: body.u32()?, sums: body.u32()? })
    }

    fn full_pages(&self) -> u64 {
        self.len / PAGE as u64
    }

    /// Bytes of `<file>.crc` this seal covers
    fn sums_len(&self) -> u64 {
        self.full_pages() * SUM as u64
    }

    /// `sums` = the checksums this seal records
    fn digest_matches(&self, sums: &[u8]) -> bool {
        crc32fast::hash(sums) == self.sums
    }
}

/// Page `index` (`bytes`: a full page, or the tail) against its CRC in `sums`, or `tail`
fn page_intact(index: u64, bytes: &[u8], sums: &[u8], tail: u32) -> bool {
    let expected = match bytes.len() == PAGE {
        true => {
            let at = usize::try_from(index).expect("page index fits usize") * SUM;
            u32::from_le_bytes(std::array::from_fn(|byte| sums[at + byte]))
        }
        false => tail,
    };
    page_sum(index, bytes) == expected
}

/// `file`'s length, refused when short of `need` (durable bytes lost)
fn durable_len(file: &dyn ReadHandle, path: &Path, need: u64) -> Result<u64, PageError> {
    let have = file.size()?;
    match have < need {
        true => Err(PageError::Lost { path: path.to_owned(), have, need }),
        false => Ok(have),
    }
}

/// Page `index`'s checksum: CRC-32 of the index (u64 LE), then the page's bytes
fn page_sum(index: u64, bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&index.to_le_bytes());
    hasher.update(bytes);
    hasher.finalize()
}

/// Tail page's checksum (0 = no tail, so [`Sealed::EMPTY`] can be a constant)
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

/// Checksum file of `<file>`: `<file>.crc`
const SUMS_SUFFIX: &str = ".crc";

pub(crate) fn sums_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(SUMS_SUFFIX);
    PathBuf::from(name)
}

/// Checksum file name of data file `name`
pub(crate) fn sums_name(name: &str) -> String {
    format!("{name}{SUMS_SUFFIX}")
}

/// Data file whose checksums `name` holds (`None` = not a checksum file)
pub(crate) fn sums_of(name: &str) -> Option<&str> {
    name.strip_suffix(SUMS_SUFFIX)
}

#[derive(Debug, thiserror::Error)]
pub enum PageError {
    #[error("page io: {0}")]
    Io(#[from] io::Error),

    #[error("{path} is {have} bytes, the durable state needs {need}")]
    Lost { path: PathBuf, have: u64, need: u64 },

    #[error("{path}: tail page fails its checksum")]
    Tail { path: PathBuf },

    #[error("{path}: page checksums differ from the digest the manifest committed")]
    Sums { path: PathBuf },
}

/// How a paged file grows + who makes its name durable
///
/// - `Segment` = written once, sealed for good (LSM): grows by exactly the appends, caller syncs
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
/// - zeros, not `fallocate` (unwritten extents = metadata change on first write)
/// - cost: file's bytes written twice
/// - dropped at open (truncated to the seal), rebuilt on append
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
        let step = self.end.clamp(MIN_RESERVE, MAX_RESERVE);
        let grown = end.max(self.end + step).next_multiple_of(PAGE as u64);
        let mut at = self.end;
        while at < grown {
            let take = (grown - at).min(ZEROS.len() as u64);
            file.write_all_at(&ZEROS[..take as usize], at)?;
            at += take;
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
    ///   before any manifest names it)
    /// - commits never sync directories
    pub(crate) fn open(
        fs: &dyn Fs,
        path: &Path,
        sealed: Sealed,
        kind: FileKind,
    ) -> Result<Self, PageError> {
        let (data, sums) = (fs.open(path)?, fs.open(&sums_path(path))?);
        let created = data.created || sums.created;
        let (data, sums) = (data.file, sums.file);
        if kind == FileKind::Log && created {
            fs.sync_dir(path.parent().unwrap_or(Path::new("")))?;
        }
        let sums_len = sealed.sums_len();
        truncate(data.as_ref(), path, sealed.len)?;
        truncate(sums.as_ref(), &sums_path(path), sums_len)?;

        let tail_at = sealed.full_pages() * PAGE as u64;
        let mut tail_page = vec![0; usize::try_from(sealed.len - tail_at).expect("tail < PAGE")];
        data.read_exact_at(&mut tail_page, tail_at)?;
        if tail_sum(sealed.full_pages(), &tail_page) != sealed.tail {
            return Err(PageError::Tail { path: path.to_owned() });
        }
        tail_page.reserve(PAGE - tail_page.len());
        let mut durable_sums = vec![0; usize::try_from(sums_len).expect("sums fit usize")];
        sums.read_exact_at(&mut durable_sums, 0)?;
        if !sealed.digest_matches(&durable_sums) {
            return Err(PageError::Sums { path: path.to_owned() });
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

    /// At the end (neither durable nor sealed until [`seal`](Self::seal))
    ///
    /// - writeback started per `WRITE_BEHIND` appended (`seal`'s fsync finds little dirty)
    pub(crate) fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if let Some([data, _]) = &mut self.reserves {
            data.cover(self.data.as_ref(), self.len + bytes.len() as u64)?;
        }
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
                let index = self.sealed_pages + (self.unsealed_sums.len() / SUM) as u64;
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

    /// `sync_data` on the data file (a device flush on a `Barrier` filesystem)
    pub(crate) fn flush(&self) -> io::Result<()> {
        self.data.sync_data()
    }

    /// Data durable → completed pages' CRCs written + durable → seal for the owner's next
    /// manifest (durable only once that manifest is durable)
    ///
    /// - [`Durability::Barrier`] = written back only: durable at the owner's one flush, before its
    ///   manifest
    pub(crate) fn seal(&mut self, durability: Durability) -> io::Result<Sealed> {
        let durable = |file: &dyn FileHandle| match durability {
            Durability::PerFile => file.sync_data(),
            Durability::Barrier => file.write_back(),
        };
        durable(self.data.as_ref())?;
        if !self.unsealed_sums.is_empty() {
            let at = self.sealed_pages * SUM as u64;
            if let Some([_, sums]) = &mut self.reserves {
                sums.cover(self.sums.as_ref(), at + self.unsealed_sums.len() as u64)?;
            }
            self.sums.write_all_at(&self.unsealed_sums, at)?;
            durable(self.sums.as_ref())?;
            self.sealed_pages += (self.unsealed_sums.len() / SUM) as u64;
            self.sums_digest = extend_digest(self.sums_digest, &self.unsealed_sums);
            self.unsealed_sums.clear();
        }
        let tail = tail_sum(self.sealed_pages, &self.tail_page);
        Ok(Sealed { len: self.len, tail, sums: self.sums_digest })
    }

    /// Read view of `sealed` (a seal this file produced), keeping `previous`'s checked pages
    pub(crate) fn pages(&self, sealed: Sealed, previous: Option<&Pages>) -> io::Result<Pages> {
        let (data, sums) = (self.data.as_ref(), self.sums.as_ref());
        Pages::map(&self.path, data, sums, sealed, previous, Access::Normal)
    }
}

fn truncate(file: &dyn FileHandle, path: &Path, need: u64) -> Result<(), PageError> {
    if durable_len(file, path, need)? > need {
        file.set_len(need)?;
    }
    Ok(())
}

/// `(sealed bytes, bytes of them in page cache)` over `pages` (`None` = any one not knowable)
pub(crate) fn footprint<'a>(pages: impl IntoIterator<Item = &'a Pages>) -> (u64, Option<u64>) {
    pages.into_iter().fold((0, Some(0)), |(bytes, cached), pages| {
        let cached = cached.zip(pages.resident_bytes()).map(|(sum, resident)| sum + resident);
        (bytes + pages.len() as u64, cached)
    })
}

/// Sealed file, mapped: every page checked on a read's first touch
///
/// - failed check = bytes changed on disk after the seal → dies (never serves unverified bytes)
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
        let missing = |path: &Path, need| PageError::Lost { path: path.to_owned(), have: 0, need };
        let data = fs.open_read(path)?.ok_or_else(|| missing(path, sealed.len))?;
        let sums_at = sums_path(path);
        let sums = fs.open_read(&sums_at)?.ok_or_else(|| missing(&sums_at, sealed.sums_len()))?;
        durable_len(data.as_ref(), path, sealed.len)?;
        durable_len(sums.as_ref(), &sums_at, sealed.sums_len())?;
        let pages = Self::map(path, data.as_ref(), sums.as_ref(), sealed, None, access)?;
        if !sealed.digest_matches(&pages.inner.sums) {
            return Err(PageError::Sums { path: path.to_owned() });
        }
        Ok(pages)
    }

    fn map(
        path: &Path,
        data: &dyn ReadHandle,
        sums: &dyn ReadHandle,
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
        // mapping may run past the seal (reserve zeros, next commit's appends): sliced to `len`
        let bytes =
            mapping.as_ref().map(|mapping| mapping.bytes().slice(..len)).unwrap_or_default();
        let sums = sums_mapping
            .map(|mapping| mapping.bytes().slice(..len / PAGE * SUM))
            .unwrap_or_default();

        let pages = len.div_ceil(PAGE);
        let checked: Box<[AtomicU64]> =
            (0..pages.div_ceil(64)).map(|_| AtomicU64::new(0)).collect();
        if let Some(previous) = previous {
            // complete pages only (tail page may have grown since)
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

    pub(crate) fn len(&self) -> usize {
        self.inner.data.len()
    }

    /// `range`, checked, as a zero-copy slice of the mapping
    pub(crate) fn bytes(&self, range: Range<usize>) -> Bytes {
        self.check(range.clone());
        self.inner.data.slice(range)
    }

    /// `range`, checked
    pub(crate) fn read(&self, range: Range<usize>) -> &[u8] {
        self.check(range.clone());
        &self.inner.data[range]
    }

    /// Sealed bytes in page cache (`None` = not knowable, empty file holds none)
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
            if !page_intact(page as u64, bytes, &inner.sums, inner.tail) {
                let path = inner.path.display();
                panic!("{path} page {page}: checksum mismatch ({CORRUPTION})");
            }
            inner.checked[page / 64].fetch_or(1 << (page % 64), Ordering::Relaxed);
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
) -> Result<Checked, PageError> {
    let path = dir.join(relative);
    let mut report = Checked {
        name: relative.to_owned(),
        committed_bytes: sealed.len,
        orphaned_bytes: 0,
        lost: false,
        bad_sums: false,
        bad_pages: Vec::new(),
    };
    let (Some(data), Some(sums)) = (fs.open_read(&path)?, fs.open_read(&sums_path(&path))?) else {
        report.lost = true;
        return Ok(report);
    };
    let lengths = durable_len(data.as_ref(), &path, sealed.len)
        .and_then(|len| Ok((len, durable_len(sums.as_ref(), &path, sealed.sums_len())?)));
    let len = match lengths {
        Ok((len, _)) => len,
        Err(PageError::Lost { .. }) => {
            report.lost = true;
            return Ok(report);
        }
        Err(error) => return Err(error),
    };
    report.orphaned_bytes = len - sealed.len;
    let mut durable_sums = vec![0; usize::try_from(sealed.sums_len()).expect("sums fit usize")];
    sums.read_exact_at(&mut durable_sums, 0)?;
    report.bad_sums = !sealed.digest_matches(&durable_sums);

    let mut page = vec![0u8; PAGE];
    for index in 0..sealed.len.div_ceil(PAGE as u64) {
        let size = (sealed.len - index * PAGE as u64).min(PAGE as u64) as usize;
        data.read_exact_at(&mut page[..size], index * PAGE as u64)?;
        if !page_intact(index, &page[..size], &durable_sums, sealed.tail) {
            report.bad_pages.push(index);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::SimFs;

    /// 3 pages + 100 bytes, no page repeating another
    fn three_pages_and_a_tail() -> Vec<u8> {
        (0..3 * PAGE + 100).map(|n| (n * 7 % 251) as u8).collect()
    }

    /// Checksum index = page index: a page moved with its checksum fails at its new place
    #[test]
    #[should_panic(expected = "page 0: checksum mismatch")]
    fn a_page_swapped_with_its_checksum_still_panics() {
        let fs = SimFs::new();
        let path = Path::new("/p/data");
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let mut file =
            PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
        file.append(&three_pages_and_a_tail()).expect("append");
        let mut sealed = file.seal(Durability::PerFile).expect("seal");
        let swap = |data: &mut Vec<u8>, width: usize| {
            let (first, second) = data.split_at_mut(width);
            first.swap_with_slice(&mut second[..width]);
        };
        fs.corrupt(path, |data| swap(data, PAGE));
        fs.corrupt(&sums_path(path), |sums| swap(sums, SUM));
        sealed.sums = crc32fast::hash(&fs.contents(&sums_path(path)).expect("sums"));
        let pages = Pages::open(fs.as_ref(), path, sealed, Access::Normal).expect("digest");

        pages.read(0..PAGE);
    }

    /// Seal at `PAGE - 3` bytes (tail only), reopen, append the rest (3 pages + 100 bytes)
    ///
    /// - one digest whether the checksums were sealed at once or not
    /// - `.crc` = each complete page's checksum, the tail's in the seal
    #[test]
    fn appends_across_pages_seal_to_each_page_s_checksum_and_reopen_continues_the_tail() {
        let fs = SimFs::new();
        let path = Path::new("/p/data");
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let bytes = three_pages_and_a_tail();

        let mut file =
            PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
        file.append(&bytes[..PAGE - 3]).expect("append");
        let first = file.seal(Durability::PerFile).expect("seal");
        drop(file);
        let mut file =
            PagedFile::open(fs.as_ref(), path, first, FileKind::Segment).expect("reopen");
        file.append(&bytes[PAGE - 3..]).expect("append");
        let sealed = file.seal(Durability::PerFile).expect("seal");

        let tail_only =
            Sealed { len: PAGE as u64 - 3, tail: page_sum(0, &bytes[..PAGE - 3]), sums: 0 };
        assert_eq!(first, tail_only, "first seal: no complete page, the tail's checksum");
        let sums = [
            page_sum(0, &bytes[..PAGE]).to_le_bytes(),
            page_sum(1, &bytes[PAGE..2 * PAGE]).to_le_bytes(),
            page_sum(2, &bytes[2 * PAGE..3 * PAGE]).to_le_bytes(),
        ]
        .concat();
        assert_eq!(fs.contents(&sums_path(path)).expect("sums"), sums, ".crc = pages 0, 1, 2");
        let tail = page_sum(3, &bytes[3 * PAGE..]);
        let whole = Sealed { len: bytes.len() as u64, tail, sums: crc32fast::hash(&sums) };
        assert_eq!(sealed, whole, "second seal: page 3's tail + the digest of pages 0-2");
        let pages = file.pages(sealed, None).expect("pages");
        let across = PAGE - 10..PAGE + 10;
        assert_eq!(pages.read(across.clone()), &bytes[across], "a read across pages 0 and 1");
        assert_eq!(pages.bytes(0..bytes.len()), bytes, "every byte");
    }

    #[test]
    fn open_drops_bytes_written_past_the_seal() {
        let fs = SimFs::new();
        let path = Path::new("/p/data");
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let bytes = three_pages_and_a_tail();
        let mut file =
            PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
        file.append(&bytes).expect("append");
        let sealed = file.seal(Durability::PerFile).expect("seal");
        file.append(&[9; 17]).expect("append past the seal");
        drop(file);

        PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment).expect("reopen at the seal");

        assert_eq!(fs.contents(path).expect("data"), bytes, "17 unsealed bytes dropped");
    }

    /// Byte 5 of page 1 flipped: open checks lengths + the checksums' digest, never page bytes
    #[test]
    fn pages_open_past_a_flipped_byte_and_serve_every_other_page() {
        let fs = SimFs::new();
        let path = Path::new("/p/data");
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let bytes = three_pages_and_a_tail();
        let mut file =
            PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
        file.append(&bytes).expect("append");
        let sealed = file.seal(Durability::PerFile).expect("seal");
        fs.corrupt(path, |data| data[PAGE + 5] ^= 1);

        let pages = Pages::open(fs.as_ref(), path, sealed, Access::Normal).expect("lengths intact");

        assert_eq!(pages.read(0..PAGE), &bytes[..PAGE], "page 0");
        assert_eq!(pages.read(2 * PAGE..bytes.len()), &bytes[2 * PAGE..], "pages 2 + 3");
    }

    /// Sealed file of 3 pages + 100 bytes, damaged after its seal: what each open refuses
    #[test]
    fn open_refuses_a_forged_checksum_reordered_pages_a_torn_tail_and_a_short_file() {
        struct Damaged {
            case: &'static str,
            damage: fn(&SimFs, &Path),
            paged_file: &'static str,
            pages: Option<&'static str>,
        }
        let sums = "/p/data: page checksums differ from the digest the manifest committed";
        let cases = [
            Damaged {
                case: "page 0 changed, its checksum rewritten to match",
                damage: |fs, path| {
                    fs.corrupt(path, |data| data[5] ^= 1);
                    let page = fs.contents(path).expect("data")[..PAGE].to_vec();
                    let forged = page_sum(0, &page).to_le_bytes();
                    fs.corrupt(&sums_path(path), |sums| sums[..SUM].copy_from_slice(&forged));
                },
                paged_file: sums,
                pages: Some(sums),
            },
            Damaged {
                case: "pages 0 + 1 swapped with their checksums",
                damage: |fs, path| {
                    let swap = |data: &mut Vec<u8>, width: usize| {
                        let (first, second) = data.split_at_mut(width);
                        first.swap_with_slice(&mut second[..width]);
                    };
                    fs.corrupt(path, |data| swap(data, PAGE));
                    fs.corrupt(&sums_path(path), |sums| swap(sums, SUM));
                },
                paged_file: sums,
                pages: Some(sums),
            },
            Damaged {
                case: "tail byte flipped",
                damage: |fs, path| fs.corrupt(path, |data| data[3 * PAGE + 1] ^= 1),
                paged_file: "/p/data: tail page fails its checksum",
                pages: None,
            },
            Damaged {
                case: "cut to 10 bytes",
                damage: |fs, path| fs.corrupt(path, |data| data.truncate(10)),
                paged_file: "/p/data is 10 bytes, the durable state needs 12388",
                pages: Some("/p/data is 10 bytes, the durable state needs 12388"),
            },
        ];
        for Damaged { case, damage, paged_file, pages } in cases {
            let fs = SimFs::new();
            let path = Path::new("/p/data");
            fs.create_dir_all(Path::new("/p")).expect("dir");
            let mut file =
                PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
            file.append(&three_pages_and_a_tail()).expect("append");
            let sealed = file.seal(Durability::PerFile).expect("seal");
            drop(file);
            damage(&fs, path);

            let mapped = Pages::open(fs.as_ref(), path, sealed, Access::Normal);
            let reopened = PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment);

            let refused = mapped.err().map(|error| error.to_string());
            assert_eq!(refused.as_deref(), pages, "{case}: Pages::open");
            let refused = reopened.map(|_| ()).expect_err(case).to_string();
            assert_eq!(refused, paged_file, "{case}: PagedFile::open");
        }
    }

    /// Crash right after open: a log's files survive (linked at creation), a segment's wait for
    /// its owner's directory sync
    #[test]
    fn a_log_is_linked_durably_at_creation_a_segment_only_by_its_owner() {
        let fs = SimFs::new();
        for dir in ["/segment", "/log"] {
            fs.create_dir_all(Path::new(dir)).expect("dir");
        }
        fs.sync_dir(Path::new("/")).expect("link every directory");
        let (segment, log) = (Path::new("/segment/data"), Path::new("/log/data"));

        PagedFile::open(fs.as_ref(), segment, Sealed::EMPTY, FileKind::Segment).expect("segment");
        PagedFile::open(fs.as_ref(), log, Sealed::EMPTY, FileKind::Log).expect("log");

        let crashed = fs.power_loss();
        let survived = |path: &Path| crashed.contents(path).is_some();
        let files = [log, &sums_path(log), segment, &sums_path(segment)].map(survived);
        assert_eq!(files, [true, true, false, false], "survived: log, its .crc, segment, its .crc");
    }

    /// Data durably linked, checksums never created (crash between the two)
    #[test]
    fn a_log_reopened_without_its_checksums_links_a_new_checksums_file() {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/half")).expect("dir");
        fs.sync_dir(Path::new("/")).expect("link the directory");
        let half = Path::new("/half/data");
        fs.open(half).expect("data only");
        fs.sync_dir(Path::new("/half")).expect("link the data");

        PagedFile::open(fs.as_ref(), half, Sealed::EMPTY, FileKind::Log).expect("open");

        let crashed = fs.power_loss();
        assert!(crashed.contents(&sums_path(half)).is_some(), ".crc lost in a crash after open");
    }

    /// Log + segment fed the same 3 pages + 100 bytes in two appends (reserve = 64 KiB at first,
    /// then a step of the file's size)
    #[test]
    fn a_log_appends_into_a_zeroed_reserve_and_seals_like_a_segment() {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let (segment_path, log_path) = (Path::new("/p/segment"), Path::new("/p/log"));
        let open = |path, kind| PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, kind);
        let mut segment = open(segment_path, FileKind::Segment).expect("open segment");
        let mut log = open(log_path, FileKind::Log).expect("open log");
        let len = |path: &Path| fs.contents(path).expect("file").len() as u64;
        let bytes = three_pages_and_a_tail();

        for piece in [&bytes[..10], &bytes[10..]] {
            segment.append(piece).expect("append segment");
            log.append(piece).expect("append log");
            assert_eq!(len(log_path), 64 << 10, "log after {} bytes: the first step", piece.len());
        }
        let sealed = segment.seal(Durability::PerFile).expect("seal segment");
        let log_sealed = log.seal(Durability::PerFile).expect("seal log");

        assert_eq!(log_sealed, sealed, "log seal = segment seal (reserve never sealed)");
        assert_eq!(len(&sums_path(log_path)), 64 << 10, "log .crc: the first step too");
        let stored = fs.contents(log_path).expect("log");
        assert_eq!(&stored[..bytes.len()], &bytes[..], "log bytes");
        let written = stored[bytes.len()..].iter().position(|byte| *byte != 0);
        assert_eq!(written, None, "reserve past byte {} = zeros", bytes.len());
        let read =
            |file: &PagedFile| file.pages(sealed, None).expect("pages").bytes(0..bytes.len());
        assert_eq!(read(&log), read(&segment), "log reads = segment reads");
        log.append(&[1; 64 << 10]).expect("past the reserve");
        assert_eq!(len(log_path), 128 << 10, "next step = the file's 64 KiB again");
    }

    /// Log of 3 pages + 100 bytes (12 388), sealed inside its 64 KiB reserve
    #[test]
    fn reopening_a_log_drops_its_reserve_and_the_next_append_rebuilds_it() {
        let fs = SimFs::new();
        fs.create_dir_all(Path::new("/p")).expect("dir");
        let path = Path::new("/p/log");
        let len = || fs.contents(path).expect("log").len();
        let mut log =
            PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Log).expect("open");
        log.append(&three_pages_and_a_tail()).expect("append");
        let sealed = log.seal(Durability::PerFile).expect("seal");
        drop(log);

        let mut reopened = PagedFile::open(fs.as_ref(), path, sealed, FileKind::Log).expect("open");
        assert_eq!(len(), 12_388, "reopened: truncated to the seal");
        reopened.append(&[2]).expect("append");
        assert_eq!(len(), 81_920, "one append on: 12 388 + a 64 KiB step, to a whole page");
    }

    /// Real file past `WRITE_BEHIND` (writeback hint issued) of 1 MiB + 2 pages + 9 bytes,
    /// damaged after its seal
    #[test]
    fn scrub_names_bad_pages_orphaned_bytes_bad_checksums_and_lost_files() {
        const LEN: usize = (1 << 20) + 2 * PAGE + 9;
        let clean = Checked {
            name: "data".to_owned(),
            committed_bytes: LEN as u64,
            orphaned_bytes: 0,
            lost: false,
            bad_sums: false,
            bad_pages: vec![],
        };
        struct Damaged {
            case: &'static str,
            damage: fn(&Path),
            checked: Checked,
        }
        let cases = [
            Damaged { case: "untouched", damage: |_| {}, checked: clean.clone() },
            Damaged {
                case: "page 1 byte flipped, 5 bytes appended",
                damage: |path| {
                    let mut bytes = std::fs::read(path).expect("read");
                    bytes[PAGE + 1] ^= 1;
                    bytes.extend_from_slice(&[0; 5]);
                    std::fs::write(path, &bytes).expect("write");
                },
                checked: Checked { bad_pages: vec![1], orphaned_bytes: 5, ..clean.clone() },
            },
            Damaged {
                case: "page 0's checksum flipped",
                damage: |path| {
                    let mut sums = std::fs::read(sums_path(path)).expect("read");
                    sums[0] ^= 1;
                    std::fs::write(sums_path(path), &sums).expect("write");
                },
                checked: Checked { bad_sums: true, bad_pages: vec![0], ..clean.clone() },
            },
            Damaged {
                case: "cut to one page",
                damage: |path| {
                    let bytes = std::fs::read(path).expect("read");
                    std::fs::write(path, &bytes[..PAGE]).expect("write");
                },
                checked: Checked { lost: true, ..clean.clone() },
            },
        ];
        for Damaged { case, damage, checked } in cases {
            let dir = tempfile::tempdir().expect("tempdir");
            let fs = crate::fs::RealFs::shared();
            let path = dir.path().join("data");
            let mut file = PagedFile::open(fs.as_ref(), &path, Sealed::EMPTY, FileKind::Segment)
                .expect("open");
            file.append(&(0..LEN).map(|n| n as u8).collect::<Vec<_>>()).expect("append");
            let sealed = file.seal(Durability::PerFile).expect("seal");
            damage(&path);

            let scrubbed = scrub(fs.as_ref(), dir.path(), "data", sealed).expect("scrub");

            assert_eq!(scrubbed, checked, "{case}");
        }
    }
}
