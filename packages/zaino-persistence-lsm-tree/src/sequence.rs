//! Sequence namespaces as positional files: record `i` found by arithmetic, never searched
//!
//! ```text
//! <name>.dat   records back to back, position order        Fixed(n): record i at i × n
//! <name>.idx   Variable only: end offset u64 LE per record  record i = end(i - 1)..end(i)
//! ```
//!
//! - both files = `crate::pages` append logs (checksummed, sealed per commit)

use std::{ops::Range, path::Path};

use zaino_persistence::Width;

use crate::{
    error::StoreError,
    fs::Fs,
    manifest::{BodyReader, ManifestError},
    pages::{FileKind, PagedFile, Pages, Sealed},
};

const END: usize = 8;
const END_U64: u64 = END as u64;

/// Sequence's committed files, as the manifest records them (`ends` = `EMPTY` when Fixed)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Seals {
    pub(crate) data: Sealed,
    pub(crate) ends: Sealed,
}

impl Seals {
    pub(crate) const EMPTY: Self = Self {
        data: Sealed::EMPTY,
        ends: Sealed::EMPTY,
    };

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
            Width::Fixed(n) => data.len.is_multiple_of(u64::from(n.get())),
            Width::Variable => ends.len.is_multiple_of(END_U64),
        };
        match whole {
            true => Ok(Self { data, ends }),
            false => Err(ManifestError::Body(
                "a sequence's length is not whole records",
            )),
        }
    }
}

/// `(file name, its seal)` per file of sequence `name` (the scrub's units)
pub(crate) fn files(name: &str, width: Width, seals: &Seals) -> Vec<(String, Sealed)> {
    let mut files = vec![(data_name(name), seals.data)];
    if width == Width::Variable {
        files.push((ends_name(name), seals.ends));
    }
    files
}

fn data_name(name: &str) -> String {
    format!("{name}.dat")
}

fn ends_name(name: &str) -> String {
    format!("{name}.idx")
}

/// Bytes per fixed record
fn fixed(n: std::num::NonZeroU32) -> Result<usize, StoreError> {
    usize::try_from(n.get()).map_err(|_| StoreError::Overflow("record width"))
}

/// Write side of one sequence: appended, then sealed by the store's commit
#[derive(Debug)]
pub(crate) struct SequenceFile {
    width: Width,
    data: PagedFile,
    ends: Option<PagedFile>,
    sealed: Seals,
    grown: bool,
    cache_writes: bool,
}

impl SequenceFile {
    /// Sequence `name`'s files under `dir`, at `sealed` (bytes past it dropped, shorter file
    /// refused)
    pub(crate) fn open(
        fs: &dyn Fs,
        dir: &Path,
        name: &str,
        width: Width,
        cache_writes: bool,
        sealed: Seals,
    ) -> Result<Self, StoreError> {
        let file = |name: String, seal| PagedFile::open(fs, &dir.join(name), seal, FileKind::Log);
        let data = file(data_name(name), sealed.data)?;
        let ends = match width {
            Width::Fixed(_) => None,
            Width::Variable => Some(file(ends_name(name), sealed.ends)?),
        };
        Ok(Self {
            width,
            data,
            ends,
            sealed,
            grown: false,
            cache_writes,
        })
    }

    /// File names a fresh directory must not hold data in
    pub(crate) fn names(name: &str, width: Width) -> Vec<String> {
        let names = match width {
            Width::Fixed(_) => vec![data_name(name)],
            Width::Variable => vec![data_name(name), ends_name(name)],
        };
        names
            .iter()
            .flat_map(|name| [name.clone(), format!("{name}.crc")])
            .collect()
    }

    /// `record` at the end; neither durable nor readable until [`seal`](Self::seal) + commit
    pub(crate) fn append(&mut self, record: &[u8]) -> Result<(), StoreError> {
        if let Width::Fixed(n) = self.width {
            if record.len() != fixed(n)? {
                return Err(StoreError::Internal("a record off its sequence's width"));
            }
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
    pub(crate) fn seal(&mut self) -> Result<Seals, StoreError> {
        if self.grown {
            self.sealed.data = self.data.seal()?;
            if let Some(ends) = &mut self.ends {
                self.sealed.ends = ends.seal()?;
            }
            // synced = clean: dropping costs nothing now, one 4 KiB re-read of the tail page later
            if !self.cache_writes {
                self.data.drop_cache()?;
                if let Some(ends) = &self.ends {
                    ends.drop_cache()?;
                }
            }
            self.grown = false;
        }
        Ok(self.sealed)
    }

    /// Read view at the last seal; `previous` reused whole when at the same seals, else its
    /// checked pages carried over
    pub(crate) fn pages(
        &self,
        previous: Option<&SequencePages>,
    ) -> Result<SequencePages, StoreError> {
        if let Some(previous) = previous.filter(|old| old.sealed == self.sealed) {
            return Ok(previous.clone());
        }
        let data = self
            .data
            .pages(self.sealed.data, previous.map(|old| &old.data))?;
        let layout = match (self.width, &self.ends) {
            (Width::Fixed(n), _) => Layout::Fixed(fixed(n)?),
            (Width::Variable, Some(ends)) => {
                let old = previous.and_then(|old| match &old.layout {
                    Layout::Variable(ends) => Some(ends),
                    Layout::Fixed(_) => None,
                });
                Layout::Variable(ends.pages(self.sealed.ends, old)?)
            }
            (Width::Variable, None) => {
                return Err(StoreError::Internal("a variable sequence without its ends"))
            }
        };
        Ok(SequencePages {
            sealed: self.sealed,
            data,
            layout,
        })
    }
}

/// Read side of one sequence at a commit
#[derive(Debug, Clone)]
pub(crate) struct SequencePages {
    sealed: Seals,
    data: Pages,
    layout: Layout,
}

/// How a record's bytes are found: fixed stride, or its end offset in `<name>.idx`
#[derive(Debug, Clone)]
enum Layout {
    Fixed(usize),
    Variable(Pages),
}

impl SequencePages {
    /// Its files (data + ends when variable)
    pub(crate) fn pages(&self) -> impl Iterator<Item = &Pages> {
        let ends = match &self.layout {
            Layout::Variable(ends) => Some(ends),
            Layout::Fixed(_) => None,
        };
        std::iter::once(&self.data).chain(ends)
    }

    pub(crate) fn len(&self) -> u64 {
        match &self.layout {
            Layout::Fixed(n) => (self.data.len() as u64)
                .checked_div(*n as u64)
                .unwrap_or_default(),
            Layout::Variable(ends) => (ends.len() / END) as u64,
        }
    }

    /// Record `at`, checked, borrowed from the mapping (`None` = past the end)
    pub(crate) fn record(&self, at: u64) -> Result<Option<&[u8]>, StoreError> {
        if at >= self.len() {
            return Ok(None);
        }
        let span = self.span(at..at.saturating_add(1))?;
        Ok(Some(self.data.read(span)?))
    }

    /// Readahead over the first `budget` bytes of records `range` in walk order (`reverse` = from
    /// its end; advisory: one request, not one fault per page)
    pub(crate) fn will_need(
        &self,
        range: Range<u64>,
        budget: usize,
        reverse: bool,
    ) -> Result<(), StoreError> {
        let range = range.start.min(self.len())..range.end.min(self.len());
        if range.is_empty() {
            return Ok(());
        }
        let span = self.span(range)?;
        let span = match reverse {
            false => span.start..span.end.min(span.start.saturating_add(budget)),
            true => span.start.max(span.end.saturating_sub(budget))..span.end,
        };
        self.data.will_need(span);
        Ok(())
    }

    /// Bytes of records `range` (within `len`) in `<name>.dat`
    fn span(&self, range: Range<u64>) -> Result<Range<usize>, StoreError> {
        let at = |position: u64| {
            usize::try_from(position).map_err(|_| StoreError::Overflow("sequence position"))
        };
        match &self.layout {
            Layout::Fixed(n) => {
                let offset = |position: u64| {
                    at(position)?
                        .checked_mul(*n)
                        .ok_or(StoreError::Overflow("record offset"))
                };
                Ok(offset(range.start)?..offset(range.end)?)
            }
            Layout::Variable(ends) => {
                let end = |position: usize| -> Result<usize, StoreError> {
                    let from = position
                        .checked_mul(END)
                        .ok_or(StoreError::Overflow("end offset"))?;
                    let bytes = ends.read(from..from.saturating_add(END))?;
                    let end = <[u8; END]>::try_from(bytes)
                        .map_err(|_| StoreError::Internal("an end offset not 8 bytes"))?;
                    usize::try_from(u64::from_le_bytes(end))
                        .map_err(|_| StoreError::Overflow("record end"))
                };
                let start = match at(range.start)?.checked_sub(1) {
                    Some(before) => end(before)?,
                    None => 0,
                };
                let last = at(range.end)?
                    .checked_sub(1)
                    .ok_or(StoreError::Internal("an empty record span"))?;
                Ok(start..end(last)?)
            }
        }
    }
}

#[cfg(test)]
mod tests;
