//! Sequence tables as positional files: record `i` found by arithmetic, never searched
//!
//! ```text
//! <name>.dat   records back to back, position order        Fixed(n): record i at i × n
//! <name>.idx   Variable only: end offset u64 LE per record  record i = end(i - 1)..end(i)
//! ```
//!
//! - both files = `crate::pages` append logs (checksummed, sealed per commit)

use std::{io, num::NonZeroUsize, ops::Range, path::Path};

use bytes::Bytes;

use crate::{
    fs::{Durability, Fs},
    manifest::{self, BodyReader},
    pages::{sums_name, FileKind, PageError, PagedFile, Pages, Sealed},
    schema::{SequenceTable, Width},
};

const END: usize = 8;

/// Where record `i` sits in `<name>.dat`: at `i × n`, or between two ends `T` holds (`.idx`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Records<T> {
    Fixed(NonZeroUsize),
    Variable(T),
}

impl<T> Records<T> {
    fn of(width: Width, ends: impl FnOnce() -> T) -> Self {
        match width {
            Width::Fixed(n) => Self::Fixed(n),
            Width::Variable => Self::Variable(ends()),
        }
    }

    fn ends(&self) -> Option<&T> {
        match self {
            Self::Fixed(_) => None,
            Self::Variable(ends) => Some(ends),
        }
    }
}

/// Sequence's durable files, as the manifest records them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seals {
    pub(crate) data: Sealed,
    pub(crate) records: Records<Sealed>,
}

impl Seals {
    /// Nothing durable
    pub(crate) fn empty(width: Width) -> Self {
        Self { data: Sealed::EMPTY, records: Records::of(width, || Sealed::EMPTY) }
    }

    /// `data` seal, then (Variable) the `ends` seal
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        self.data.encode(out);
        if let Records::Variable(ends) = &self.records {
            ends.encode(out);
        }
    }

    pub(crate) fn decode(width: Width, body: &mut BodyReader<'_>) -> Self {
        let data = Sealed::decode(body);
        let records = match width {
            Width::Fixed(n) => Records::Fixed(n),
            Width::Variable => Records::Variable(Sealed::decode(body)),
        };
        let whole = match records {
            Records::Fixed(n) => {
                data.len.is_multiple_of(u64::try_from(n.get()).expect("record width fits u64"))
            }
            Records::Variable(ends) => {
                ends.len.is_multiple_of(u64::try_from(END).expect("END = 8"))
            }
        };
        if !whole {
            manifest::malformed("seals a sequence at part of a record")
        }
        Self { data, records }
    }
}

fn data_name(table: &SequenceTable) -> String {
    format!("{}.dat", table.name)
}

fn ends_name(table: &SequenceTable) -> String {
    format!("{}.idx", table.name)
}

/// `(file name, its seal)` per file of `table` (the scrub's units)
pub(crate) fn files(table: &SequenceTable, seals: &Seals) -> Vec<(String, Sealed)> {
    let ends = seals.records.ends().map(|ends| (ends_name(table), *ends));
    std::iter::once((data_name(table), seals.data)).chain(ends).collect()
}

/// Every file `table` writes, checksums included (what a fresh directory must not hold data in)
pub(crate) fn names(table: &SequenceTable) -> Vec<String> {
    let files = files(table, &Seals::empty(table.record));
    files.into_iter().flat_map(|(name, _)| [sums_name(&name), name]).collect()
}

/// One paged file + its last seal
#[derive(Debug)]
struct Sealing {
    file: PagedFile,
    sealed: Sealed,
}

impl Sealing {
    fn open(fs: &dyn Fs, path: &Path, sealed: Sealed) -> Result<Self, PageError> {
        Ok(Self { file: PagedFile::open(fs, path, sealed, FileKind::Log)?, sealed })
    }

    fn seal(&mut self, durability: Durability, drop_cache: bool) -> io::Result<()> {
        self.sealed = self.file.seal(durability)?;
        // written back = clean: dropping costs nothing now, one 4 KiB re-read of the tail page later
        if drop_cache {
            self.file.drop_cache()?;
        }
        Ok(())
    }
}

/// Write side of one sequence: appended, then sealed by the store's commit
#[derive(Debug)]
pub(crate) struct SequenceFile {
    data: Sealing,
    records: Records<Sealing>,
    grown: bool,
    cache_writes: bool,
}

impl SequenceFile {
    /// `table`'s files under `dir`, at `sealed` (bytes past it dropped, shorter file refused)
    pub(crate) fn open(
        fs: &dyn Fs,
        dir: &Path,
        table: &SequenceTable,
        sealed: Seals,
    ) -> Result<Self, PageError> {
        let data = Sealing::open(fs, &dir.join(data_name(table)), sealed.data)?;
        let records = match sealed.records {
            Records::Fixed(n) => Records::Fixed(n),
            Records::Variable(ends) => {
                Records::Variable(Sealing::open(fs, &dir.join(ends_name(table)), ends)?)
            }
        };
        Ok(Self { data, records, grown: false, cache_writes: table.cache_writes })
    }

    /// `record` at the end (neither durable nor readable until [`seal`](Self::seal) + commit)
    pub(crate) fn append(&mut self, record: &[u8]) -> io::Result<()> {
        if let Records::Fixed(n) = self.records {
            assert_eq!(
                record.len(),
                n.get(),
                "a fixed-width sequence record of {} bytes",
                record.len()
            );
        }
        self.data.file.append(record)?;
        if let Records::Variable(ends) = &mut self.records {
            ends.file.append(&self.data.file.len().to_le_bytes())?;
        }
        self.grown = true;
        Ok(())
    }

    /// Files made durable per `durability` if grown → seals for the next manifest + whether it grew
    ///
    /// - `PerFile`: an fsync per file (∝ files, tree-state holds ~100)
    /// - `Barrier`: written back, durable at the store's one [`flush`](Self::flush)
    pub(crate) fn seal(&mut self, durability: Durability) -> io::Result<(Seals, bool)> {
        let grown = std::mem::take(&mut self.grown);
        if grown {
            self.data.seal(durability, !self.cache_writes)?;
            if let Records::Variable(ends) = &mut self.records {
                ends.seal(durability, !self.cache_writes)?;
            }
        }
        Ok((self.seals(), grown))
    }

    /// The device flush that makes every file a `Barrier` seal wrote back durable (`sync_data` on
    /// this table's data: a file the commit wrote, on the index's device)
    pub(crate) fn flush(&self) -> io::Result<()> {
        self.data.file.flush()
    }

    /// Read view at the last seal
    ///
    /// - `previous` reused whole when at the same seals, else its checked pages carried over
    pub(crate) fn pages(&self, previous: Option<&SequencePages>) -> io::Result<SequencePages> {
        let sealed = self.seals();
        if let Some(previous) = previous.filter(|old| old.sealed == sealed) {
            return Ok(previous.clone());
        }
        let data = self.data.file.pages(sealed.data, previous.map(|old| &old.data))?;
        let records = match &self.records {
            Records::Fixed(n) => Records::Fixed(*n),
            Records::Variable(ends) => {
                let old = previous.and_then(|old| old.records.ends());
                Records::Variable(ends.file.pages(ends.sealed, old)?)
            }
        };
        Ok(SequencePages { sealed, data, records })
    }

    fn seals(&self) -> Seals {
        let records = match &self.records {
            Records::Fixed(n) => Records::Fixed(*n),
            Records::Variable(ends) => Records::Variable(ends.sealed),
        };
        Seals { data: self.data.sealed, records }
    }
}

/// Read side of one sequence at a commit
#[derive(Debug, Clone)]
pub(crate) struct SequencePages {
    sealed: Seals,
    data: Pages,
    records: Records<Pages>,
}

impl SequencePages {
    /// Its files (data + ends when variable)
    pub(crate) fn pages(&self) -> impl Iterator<Item = &Pages> {
        std::iter::once(&self.data).chain(self.records.ends())
    }

    pub(crate) fn len(&self) -> u64 {
        let records = match &self.records {
            Records::Fixed(n) => self.data.len() / n.get(),
            Records::Variable(ends) => ends.len() / END,
        };
        u64::try_from(records).expect("usize fits u64 (64-bit target)")
    }

    pub(crate) fn record(&self, at: u64) -> Option<Bytes> {
        (at < self.len()).then(|| self.data.bytes(self.span(at..at + 1)))
    }

    /// Records in `range` (within `len`), sliced from one span (one readahead for all of it)
    pub(crate) fn records(&self, range: Range<u64>) -> Vec<Bytes> {
        assert!(range.end <= self.len(), "records {range:?} past the sequence's {}", self.len());
        if range.is_empty() {
            return Vec::new();
        }
        let span = self.span(range.clone());
        self.data.will_need(span.clone());
        let bytes = self.data.bytes(span.clone());
        range
            .map(|at| {
                let record = self.span(at..at + 1);
                bytes.slice(record.start - span.start..record.end - span.start)
            })
            .collect()
    }

    /// Bytes of records `range` in `<name>.dat`
    fn span(&self, range: Range<u64>) -> Range<usize> {
        let at = |position: u64| usize::try_from(position).expect("positions fit usize");
        match &self.records {
            Records::Fixed(n) => at(range.start) * n.get()..at(range.end) * n.get(),
            Records::Variable(ends) => {
                let end = |position: usize| {
                    let bytes = ends.read(position * END..(position + 1) * END);
                    let end = u64::from_le_bytes(bytes.try_into().expect("END bytes"));
                    usize::try_from(end).expect("durable offsets fit usize")
                };
                let start = at(range.start).checked_sub(1).map_or(0, end);
                start..end(at(range.end) - 1)
            }
        }
    }
}
