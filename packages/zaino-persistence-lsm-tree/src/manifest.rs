//! `MANIFEST`: index's one commit point (`docs/durability.md` §2)
//!
//! ```text
//! file = slot 0 ‖ slot 1                     2 × SLOT bytes, zero-filled when created
//! slot = magic b"ZAINOMS2" ‖ format u16 ‖ name_len u8 ‖ name ‖ seq u64 ‖ body_len u32
//!        ‖ body ‖ crc32 ‖ (unused to the slot's end)
//! ```
//!
//! - little-endian throughout; CRC-32 (IEEE) over the slot's bytes before it
//! - commit `seq` overwrites slot `seq % 2` in place (other slot = the commit before it)
//! - committed state = valid slot with the highest `seq` ([`latest`])
//! - in place, not a renamed file (overwrite below EOF = no metadata change: `fdatasync` never
//!   waits on the filesystem journal or every other file's writeback)

use zaino_primitives::types::{BlockHash, BlockRef, Height};

const MAGIC: [u8; 8] = *b"ZAINOMS2";
const CRC: usize = 4;

/// Where the name starts: after magic ‖ format u16 ‖ name_len u8
const NAME_AT: usize = MAGIC.len() + 2 + 1;

/// Slot capacity: room for tens of thousands of live segments across every map
pub(crate) const SLOT: usize = 1 << 20;

/// Both slots
pub(crate) const FILE_LEN: u64 = 2 * SLOT as u64;

/// Longest index name a slot header holds
const MAX_NAME: usize = u8::MAX as usize;

/// What a directory must have been written as (any mismatch refused = identity check)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    name: Box<str>,
    format: u16,
}

impl Identity {
    /// `name` of at most 255 bytes, written at layout version `format`
    pub(crate) fn new(name: &str, format: u16) -> Result<Self, ManifestError> {
        match name.len() <= MAX_NAME {
            true => Ok(Self {
                name: name.into(),
                format,
            }),
            false => Err(ManifestError::NameLength { len: name.len() }),
        }
    }

    /// Header bytes before `seq`: magic ‖ format ‖ name_len ‖ name
    fn prefix_len(&self) -> usize {
        NAME_AT.saturating_add(self.name.len())
    }
}

/// - `Unmanifested` never follows a crash (a directory's first commit precedes any data)
#[derive(Debug, thiserror::Error)]
pub(crate) enum ManifestError {
    #[error("manifest io: {0}")]
    Io(#[from] std::io::Error),

    #[error("MANIFEST slot does not start with the zaino manifest magic")]
    Magic,

    #[error("MANIFEST checksum mismatch")]
    Checksum,

    #[error("MANIFEST is {len} bytes, not this build's two-slot layout: resync the index")]
    Layout { len: u64 },

    #[error("MANIFEST slots both claim commit {seq}")]
    Sequence { seq: u64 },

    #[error("MANIFEST body claims {claimed} bytes, {actual} present")]
    BodyLength { claimed: u32, actual: usize },

    #[error("directory holds index {found:?}, expected {expected:?}")]
    Name { expected: String, found: String },

    #[error("index name of {len} bytes, at most {MAX_NAME}")]
    NameLength { len: usize },

    #[error("directory holds format {found}, this build reads format {expected}")]
    Format { expected: u16, found: u16 },

    #[error("manifest body of {body} bytes overflows its {SLOT}-byte slot")]
    Full { body: usize },

    #[error("malformed manifest body: {0}")]
    Body(&'static str),

    #[error("{path} holds data but the directory has no MANIFEST")]
    Unmanifested { path: String },
}

/// Commit `seq`'s slot bytes (written at [`slot_offset`]`(seq)`; rest of the slot untouched,
/// outside the CRC)
pub(crate) fn encode(identity: &Identity, seq: u64, body: &[u8]) -> Result<Vec<u8>, ManifestError> {
    let full = || ManifestError::Full { body: body.len() };
    let len = identity
        .prefix_len()
        .checked_add(8 + 4 + CRC)
        .and_then(|header| header.checked_add(body.len()))
        .filter(|len| *len <= SLOT)
        .ok_or_else(full)?;
    let body_len = u32::try_from(body.len()).map_err(|_| full())?;
    let name_len = u8::try_from(identity.name.len()).map_err(|_| ManifestError::NameLength {
        len: identity.name.len(),
    })?;
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&identity.format.to_le_bytes());
    out.push(name_len);
    out.extend_from_slice(identity.name.as_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32fast::hash(&out).to_le_bytes());
    Ok(out)
}

/// Commit `seq`'s offset: alternating slots (never over the commit before it)
pub(crate) fn slot_offset(seq: u64) -> u64 {
    match seq.is_multiple_of(2) {
        true => 0,
        false => SLOT as u64,
    }
}

/// - `Unwritten` = all zeros (no commit used it yet)
/// - `Torn` = fails magic, length or CRC (write a crash interrupted; why, for the error)
#[derive(Debug)]
enum Slot<'a> {
    Unwritten,
    Torn(ManifestError),
    Committed { seq: u64, body: &'a [u8] },
}

/// Bytes `n` on from `slot[*at..]`, advancing `at`; `None` = past the slot's end
fn take<'a>(slot: &'a [u8], at: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = at.checked_add(n)?;
    let bytes = slot.get(*at..end)?;
    *at = end;
    Some(bytes)
}

fn array<const N: usize>(slot: &[u8], at: &mut usize) -> Option<[u8; N]> {
    take(slot, at, N)?.try_into().ok()
}

/// `Err` = whole, checksummed slot of another index or format (refused outright, never mistaken
/// for a torn write)
fn decode_slot<'a>(identity: &Identity, slot: &'a [u8]) -> Result<Slot<'a>, ManifestError> {
    if slot.iter().all(|byte| *byte == 0) {
        return Ok(Slot::Unwritten);
    }
    let at = &mut 0;
    if array::<8>(slot, at) != Some(MAGIC) {
        return Ok(Slot::Torn(ManifestError::Magic));
    }
    let header = (|| {
        let format = u16::from_le_bytes(array(slot, at)?);
        let [name_len] = array::<1>(slot, at)?;
        let name = take(slot, at, usize::from(name_len))?;
        let seq = u64::from_le_bytes(array(slot, at)?);
        let claimed = u32::from_le_bytes(array(slot, at)?);
        Some((format, name, seq, claimed))
    })();
    let Some((format, name, seq, claimed)) = header else {
        return Ok(Slot::Torn(ManifestError::Body("truncated header")));
    };
    let header_len = *at;
    let rest = slot.get(header_len..).unwrap_or_default();
    let body_len = usize::try_from(claimed).unwrap_or(usize::MAX);
    let crc_end = body_len.checked_add(CRC);
    let (Some(body), Some(crc)) = (
        rest.get(..body_len),
        crc_end.and_then(|end| rest.get(body_len..end)),
    ) else {
        let actual = rest.len().saturating_sub(CRC);
        return Ok(Slot::Torn(ManifestError::BodyLength { claimed, actual }));
    };
    let covered = slot
        .get(..header_len.saturating_add(body_len))
        .unwrap_or_default();
    if crc32fast::hash(covered).to_le_bytes() != crc {
        return Ok(Slot::Torn(ManifestError::Checksum));
    }
    if name != identity.name.as_bytes() {
        return Err(ManifestError::Name {
            expected: identity.name.to_string(),
            found: String::from_utf8_lossy(name).into_owned(),
        });
    }
    if format != identity.format {
        return Err(ManifestError::Format {
            expected: identity.format,
            found: format,
        });
    }
    Ok(Slot::Committed { seq, body })
}

/// Committed `(seq, body)` of a whole manifest file; `None` = no commit ever completed
///
/// - both committed → higher `seq` (other = the commit before it)
/// - committed + torn → the committed one (torn = later commit interrupted before its ack)
/// - none committed, <= 1 torn → nothing committed yet (first commit interrupted)
/// - both torn → error (created zeroed + synced first; a crash tears only the slot written)
/// - other length → error (older layout; file only ever created whole)
pub(crate) fn latest<'a>(
    identity: &Identity,
    file: &'a [u8],
) -> Result<Option<(u64, &'a [u8])>, ManifestError> {
    let layout = || ManifestError::Layout {
        len: file.len() as u64,
    };
    if file.len() as u64 != FILE_LEN {
        return Err(layout());
    }
    let (first, second) = file.split_at_checked(SLOT).ok_or_else(layout)?;
    let slots = [
        decode_slot(identity, first)?,
        decode_slot(identity, second)?,
    ];

    let mut commits = slots.iter().filter_map(|slot| match slot {
        Slot::Committed { seq, body } => Some((*seq, *body)),
        Slot::Unwritten | Slot::Torn(_) => None,
    });
    match (commits.next(), commits.next()) {
        (Some((a, _)), Some((b, _))) if a == b => Err(ManifestError::Sequence { seq: a }),
        (Some(a), Some(b)) => Ok(Some(if a.0 > b.0 { a } else { b })),
        (Some(only), None) => Ok(Some(only)),
        (None, _) => match slots {
            [Slot::Torn(why), Slot::Torn(_)] => Err(why),
            _ => Ok(None),
        },
    }
}

/// `dir`'s committed body, read offline (read only, no lock)
///
/// - `None` = never committed; manifest not decoding as `identity` = `InvalidData`
pub(crate) fn read(
    fs: &dyn crate::fs::Fs,
    dir: &std::path::Path,
    identity: &Identity,
) -> std::io::Result<Option<Vec<u8>>> {
    let Some(file) = fs.open_read_only(&dir.join(crate::dir::MANIFEST))? else {
        return Ok(None);
    };
    latest(identity, &file.read_all()?)
        .map(|committed| committed.map(|(_, body)| body.to_vec()))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Committed tip (`None` = nothing committed): every body's first 40 bytes
///
/// - block count from genesis ‖ tip hash (zeros when empty)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Committed {
    pub(crate) tip: Option<BlockRef>,
}

impl Committed {
    pub(crate) const EMPTY: Self = Self { tip: None };

    /// Last committed height, inclusive (`None` = nothing committed)
    pub(crate) fn height(&self) -> Option<Height> {
        self.tip.map(|tip| tip.height)
    }

    /// Blocks committed from genesis (what a file sized per height is checked against)
    pub(crate) fn count(&self) -> u64 {
        self.height()
            .map_or(0, |height| u64::from(height).saturating_add(1))
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.count().to_le_bytes());
        out.extend_from_slice(&self.tip.map_or([0; 32], |tip| <[u8; 32]>::from(tip.hash)));
    }

    pub(crate) fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        let count = body.u64()?;
        let hash = body.array::<32>()?;
        // presence = count > 0 (a zero hash is a hash, not a sentinel)
        let Some(last) = count.checked_sub(1) else {
            return match hash == [0; 32] {
                true => Ok(Self::EMPTY),
                false => Err(ManifestError::Body("tip hash with nothing committed")),
            };
        };
        let height = u32::try_from(last)
            .ok()
            .and_then(|last| Height::try_from(last).ok())
            .ok_or(ManifestError::Body(
                "committed count past the height ceiling",
            ))?;
        Ok(Self {
            tip: Some(BlockRef {
                hash: BlockHash::from(hash),
                height,
            }),
        })
    }
}

/// Cursor over a body: every read bounds-checked, [`finish`](Self::finish) refuses a tail
pub(crate) struct BodyReader<'a> {
    rest: &'a [u8],
}

impl<'a> BodyReader<'a> {
    pub(crate) fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], ManifestError> {
        let (head, rest) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(ManifestError::Body("truncated"))?;
        self.rest = rest;
        Ok(*head)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, ManifestError> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ManifestError> {
        self.array().map(u64::from_le_bytes)
    }

    pub(crate) fn finish(self) -> Result<(), ManifestError> {
        match self.rest.is_empty() {
            true => Ok(()),
            false => Err(ManifestError::Body("trailing bytes")),
        }
    }
}

#[cfg(test)]
mod tests;
