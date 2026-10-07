//! Sequence tables as positional files: record `i` found by arithmetic, never searched
//!
//! ```text
//! <name>.dat   records back to back, position order        Fixed(n): record i at i × n
//! <name>.idx   Variable only: end offset u64 LE per record  record i = end(i - 1)..end(i)
//! ```
//!
//! - both files = `crate::pages` append logs (checksummed, sealed per commit)

use std::{io, ops::Range, path::Path};

use bytes::Bytes;

use crate::{
    fs::Fs,
    manifest::{BodyReader, ManifestError},
    pages::{FileKind, PageError, PagedFile, Pages, Sealed},
    port::{SequenceTable, Width},
};

const END: usize = 8;

/// Sequence's committed files, as the manifest records them (`ends` = `EMPTY` when Fixed)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seals {
    pub(crate) data: Sealed,
    pub(crate) ends: Sealed,
}

impl Seals {
    pub(crate) const EMPTY: Self = Self { data: Sealed::EMPTY, ends: Sealed::EMPTY };

    /// `data` seal, then (Variable) the `ends` seal
    pub(crate) fn encode(&self, width: Width, out: &mut Vec<u8>) {
        self.data.encode(out);
        if width == Width::Variable {
            self.ends.encode(out);
        }
    }

    pub(crate) fn decode(width: Width, body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        let data = Sealed::decode(body)?;
        let ends = match width {
            Width::Fixed(_) => Sealed::EMPTY,
            Width::Variable => Sealed::decode(body)?,
        };
        let whole = match width {
            Width::Fixed(n) => data.len % u64::from(n.get()) == 0,
            Width::Variable => ends.len % END as u64 == 0,
        };
        match whole {
            true => Ok(Self { data, ends }),
            false => Err(ManifestError::Body("a sequence's length is not whole records")),
        }
    }
}

/// `(file name, its seal)` per file of `table` (the scrub's units)
pub(crate) fn files(table: &SequenceTable, seals: &Seals) -> Vec<(String, Sealed)> {
    let mut files = vec![(data_name(table), seals.data)];
    if table.record == Width::Variable {
        files.push((ends_name(table), seals.ends));
    }
    files
}

fn data_name(table: &SequenceTable) -> String {
    format!("{}.dat", table.name)
}

fn ends_name(table: &SequenceTable) -> String {
    format!("{}.idx", table.name)
}

/// Write side of one sequence: appended, then sealed by the store's commit
#[derive(Debug)]
pub(crate) struct SequenceFile {
    width: Width,
    data: PagedFile,
    ends: Option<PagedFile>,
    sealed: Seals,
    grown: bool,
}

impl SequenceFile {
    /// `table`'s files under `dir`, at `sealed` (bytes past it dropped, shorter file refused)
    pub(crate) fn open(
        fs: &dyn Fs,
        dir: &Path,
        table: &SequenceTable,
        sealed: Seals,
    ) -> Result<Self, PageError> {
        let file = |name: String, seal| PagedFile::open(fs, &dir.join(name), seal, FileKind::Log);
        let data = file(data_name(table), sealed.data)?;
        let ends = match table.record {
            Width::Fixed(_) => None,
            Width::Variable => Some(file(ends_name(table), sealed.ends)?),
        };
        Ok(Self { width: table.record, data, ends, sealed, grown: false })
    }

    /// File names a fresh directory must not hold data in
    pub(crate) fn names(table: &SequenceTable) -> Vec<String> {
        let names = match table.record {
            Width::Fixed(_) => vec![data_name(table)],
            Width::Variable => vec![data_name(table), ends_name(table)],
        };
        names.iter().flat_map(|name| [name.clone(), format!("{name}.crc")]).collect()
    }

    /// `record` at the end; neither durable nor readable until [`seal`](Self::seal) + commit
    pub(crate) fn append(&mut self, record: &[u8]) -> io::Result<()> {
        if let Width::Fixed(n) = self.width {
            let n = usize::try_from(n.get()).expect("u32 fits usize");
            assert_eq!(record.len(), n, "a fixed-width sequence record of {} bytes", record.len());
        }
        self.data.append(record)?;
        if let Some(ends) = &mut self.ends {
            ends.append(&self.data.len().to_le_bytes())?;
        }
        self.grown = true;
        Ok(())
    }

    /// Files fsynced if grown (fsync cost ∝ files: tree-state holds ~100) → seals for the next
    /// manifest
    pub(crate) fn seal(&mut self) -> io::Result<Seals> {
        if self.grown {
            self.sealed.data = self.data.seal()?;
            if let Some(ends) = &mut self.ends {
                self.sealed.ends = ends.seal()?;
            }
            self.grown = false;
        }
        Ok(self.sealed)
    }

    /// Read view at the last seal; `previous` reused whole when at the same seals, else its
    /// checked pages carried over
    pub(crate) fn pages(&self, previous: Option<&SequencePages>) -> io::Result<SequencePages> {
        if let Some(previous) = previous.filter(|old| old.sealed == self.sealed) {
            return Ok(previous.clone());
        }
        let data = self.data.pages(self.sealed.data, previous.map(|old| &old.data))?;
        let ends = match &self.ends {
            Some(ends) => {
                let old = previous.and_then(|old| old.ends.as_ref());
                Some(ends.pages(self.sealed.ends, old)?)
            }
            None => None,
        };
        Ok(SequencePages { width: self.width, sealed: self.sealed, data, ends })
    }
}

/// Read side of one sequence at a commit
#[derive(Debug, Clone)]
pub(crate) struct SequencePages {
    width: Width,
    sealed: Seals,
    data: Pages,
    ends: Option<Pages>,
}

impl SequencePages {
    pub(crate) fn len(&self) -> u64 {
        match (self.width, &self.ends) {
            (Width::Fixed(n), _) => self.data.len() as u64 / u64::from(n.get()),
            (Width::Variable, Some(ends)) => (ends.len() / END) as u64,
            (Width::Variable, None) => unreachable!("a variable sequence keeps its ends"),
        }
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
        match (self.width, &self.ends) {
            (Width::Fixed(n), _) => {
                let n = usize::try_from(n.get()).expect("u32 fits usize");
                at(range.start) * n..at(range.end) * n
            }
            (Width::Variable, Some(ends)) => {
                let end = |position: usize| {
                    let bytes = ends.read(position * END..(position + 1) * END);
                    let end = u64::from_le_bytes(bytes.try_into().expect("END bytes"));
                    usize::try_from(end).expect("committed offsets fit usize")
                };
                let start = at(range.start).checked_sub(1).map_or(0, end);
                start..end(at(range.end) - 1)
            }
            (Width::Variable, None) => unreachable!("a variable sequence keeps its ends"),
        }
    }
}
