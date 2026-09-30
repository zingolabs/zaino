//! `MANIFEST`: an index's one commit point (`docs/design/durability.md` §2)
//!
//! ```text
//! file = slot 0 ‖ slot 1                     2 × SLOT bytes, zero-filled when created
//! slot = magic b"ZAINOMS\0" ‖ format u16 ‖ kind u8 ‖ network u8 ‖ seq u64 ‖ body_len u32
//!        ‖ body ‖ crc32 ‖ (unused to the slot's end)
//! ```
//!
//! - little-endian throughout; CRC-32 (IEEE) over the slot's bytes before it
//! - commit `seq` overwrites slot `seq % 2` in place, so the other slot always holds the commit
//!   before it; the committed state = the valid slot with the highest `seq` ([`latest`])
//! - in place, not a renamed file: an overwrite below EOF changes no metadata, so the commit's
//!   `fdatasync` never waits on the filesystem journal (and on every other file's writeback)

use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zcash_protocol::consensus::NetworkType;

const MAGIC: [u8; 8] = *b"ZAINOMS\0";
const HEADER: usize = 24;
const CRC: usize = 4;

/// One slot's capacity (the largest body: a segment store's lists, a few KiB at most)
pub const SLOT: usize = 64 << 10;

/// The whole manifest file: both slots
pub const FILE_LEN: u64 = 2 * SLOT as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexKind {
    CompactBlock = 1,
    TreeState = 2,
    TransparentAddress = 3,
    BlockHash = 4,
    ValueBalance = 5,
}

/// What a directory must have been written as: refusing any mismatch is the chain-identity check
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub kind: IndexKind,
    pub format: u16,
    pub network: NetworkType,
}

fn network_tag(network: NetworkType) -> u8 {
    match network {
        NetworkType::Main => 0,
        NetworkType::Test => 1,
        NetworkType::Regtest => 2,
    }
}

/// - `Unmanifested` never follows a crash (a directory's first commit precedes any data)
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
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

    #[error("directory holds index kind {found}, expected {expected:?}")]
    Kind { expected: IndexKind, found: u8 },

    #[error("directory holds format {found}, this build reads format {expected}")]
    Format { expected: u16, found: u16 },

    #[error("directory was built for network tag {found}, configured network is {expected:?}")]
    Network { expected: NetworkType, found: u8 },

    #[error("malformed manifest body: {0}")]
    Body(&'static str),

    #[error("{path} holds data but the directory has no MANIFEST")]
    Unmanifested { path: String },
}

/// Commit `seq`'s slot bytes (written at [`slot_offset`]`(seq)`; the rest of the slot is left as
/// it was, outside the CRC)
pub fn encode(identity: Identity, seq: u64, body: &[u8]) -> Vec<u8> {
    assert!(
        HEADER + body.len() + CRC <= SLOT,
        "manifest body of {} bytes overflows its {SLOT}-byte slot",
        body.len()
    );
    let mut out = Vec::with_capacity(HEADER + body.len() + CRC);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&identity.format.to_le_bytes());
    out.push(identity.kind as u8);
    out.push(network_tag(identity.network));
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&u32::try_from(body.len()).expect("body < SLOT").to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32fast::hash(&out).to_le_bytes());
    out
}

/// Where commit `seq` is written: alternating slots, so it never overwrites the commit before it
pub fn slot_offset(seq: u64) -> u64 {
    (seq % 2) * SLOT as u64
}

/// What one slot holds
#[derive(Debug)]
enum Slot<'a> {
    /// All zeros: no commit has used it yet
    Unwritten,
    /// Fails its magic, length or CRC: a write a crash interrupted (why, for the error)
    Torn(ManifestError),
    Committed {
        seq: u64,
        body: &'a [u8],
    },
}

/// Reads one slot; `Err` = a whole, checksummed slot written as another index, format or
/// network (refused outright, never mistaken for a torn write)
fn decode_slot(identity: Identity, slot: &[u8]) -> Result<Slot<'_>, ManifestError> {
    if slot.iter().all(|byte| *byte == 0) {
        return Ok(Slot::Unwritten);
    }
    let (header, rest) = slot.split_first_chunk::<HEADER>().expect("a slot holds its header");
    if header[..8] != MAGIC {
        return Ok(Slot::Torn(ManifestError::Magic));
    }
    let claimed = u32::from_le_bytes([header[20], header[21], header[22], header[23]]);
    let body_len = usize::try_from(claimed).expect("u32 fits usize");
    let (Some(body), Some(crc)) = (rest.get(..body_len), rest.get(body_len..body_len + CRC)) else {
        return Ok(Slot::Torn(ManifestError::BodyLength { claimed, actual: rest.len() - CRC }));
    };
    if crc32fast::hash(&slot[..HEADER + body_len]).to_le_bytes() != crc {
        return Ok(Slot::Torn(ManifestError::Checksum));
    }
    if header[10] != identity.kind as u8 {
        return Err(ManifestError::Kind { expected: identity.kind, found: header[10] });
    }
    let format = u16::from_le_bytes([header[8], header[9]]);
    if format != identity.format {
        return Err(ManifestError::Format { expected: identity.format, found: format });
    }
    if header[11] != network_tag(identity.network) {
        return Err(ManifestError::Network { expected: identity.network, found: header[11] });
    }
    let seq = u64::from_le_bytes(header[12..20].try_into().expect("8 bytes"));
    Ok(Slot::Committed { seq, body })
}

/// The committed `(seq, body)` of a whole manifest file; `None` = no commit ever completed
///
/// - both slots committed: the higher `seq` (the other = the commit before it)
/// - one committed beside a torn one: the committed one (the torn write = a later commit a crash
///   interrupted before it was acknowledged)
/// - none committed, at most one torn: nothing committed yet (the first commit interrupted)
/// - both torn: an error (the file is created zeroed and synced before any commit, and a crash
///   tears only the slot being written, never the other)
/// - any other length: an error (an older layout; this file is only ever created whole)
pub fn latest(identity: Identity, file: &[u8]) -> Result<Option<(u64, &[u8])>, ManifestError> {
    if file.len() as u64 != FILE_LEN {
        return Err(ManifestError::Layout { len: file.len() as u64 });
    }
    let (first, second) = file.split_at(SLOT);
    let slots = [decode_slot(identity, first)?, decode_slot(identity, second)?];

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

/// `dir`'s committed body, read offline (plain read, no lock); `None` = never committed, a
/// manifest that will not decode as `identity` = `InvalidData`
pub fn read(dir: &std::path::Path, identity: Identity) -> std::io::Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(dir.join(crate::dir::MANIFEST)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    latest(identity, &bytes)
        .map(|committed| committed.map(|(_, body)| body.to_vec()))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Committed tip (`None` = nothing committed): every body's first 40 bytes, as the block count
/// from genesis ‖ the tip hash (zeros when empty)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub tip: Option<BlockRef>,
}

impl Committed {
    pub const EMPTY: Self = Self { tip: None };

    /// Last committed height, inclusive (`None` = nothing committed)
    pub fn height(&self) -> Option<Height> {
        self.tip.map(|tip| tip.height)
    }

    /// Blocks committed from genesis (what a file sized per height is checked against)
    pub fn count(&self) -> u64 {
        self.height().map_or(0, |height| u64::from(height) + 1)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.count().to_le_bytes());
        out.extend_from_slice(&self.tip.map_or([0; 32], |tip| <[u8; 32]>::from(tip.hash)));
    }

    pub fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
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
            .ok_or(ManifestError::Body("committed count past the height ceiling"))?;
        Ok(Self { tip: Some(BlockRef { hash: BlockHash::from(hash), height }) })
    }
}

/// Cursor over a body; every read is bounds-checked, [`finish`](Self::finish) refuses a tail
pub struct BodyReader<'a> {
    rest: &'a [u8],
}

impl<'a> BodyReader<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], ManifestError> {
        let (head, rest) =
            self.rest.split_first_chunk::<N>().ok_or(ManifestError::Body("truncated"))?;
        self.rest = rest;
        Ok(*head)
    }

    pub fn u32(&mut self) -> Result<u32, ManifestError> {
        self.array().map(u32::from_le_bytes)
    }

    pub fn u64(&mut self) -> Result<u64, ManifestError> {
        self.array().map(u64::from_le_bytes)
    }

    pub fn finish(self) -> Result<(), ManifestError> {
        match self.rest.is_empty() {
            true => Ok(()),
            false => Err(ManifestError::Body("trailing bytes")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot pinned byte for byte, alternating by `seq`, and the committed state read off every
    /// shape a crash, a foreign directory or an older layout can leave in the two slots
    #[test]
    fn manifest_slots_golden_bytes_and_which_commit_every_slot_pair_holds() {
        let identity =
            Identity { kind: IndexKind::TreeState, format: 3, network: NetworkType::Test };
        let encoded = encode(identity, 7, &[0xaa, 0xbb]);
        let mut expected = b"ZAINOMS\0".to_vec();
        expected.extend_from_slice(&[3, 0, 2, 1]);
        expected.extend_from_slice(&7u64.to_le_bytes());
        expected.extend_from_slice(&[2, 0, 0, 0, 0xaa, 0xbb]);
        expected.extend_from_slice(&crc32fast::hash(&expected).to_le_bytes());
        assert_eq!(encoded, expected);
        assert_eq!((slot_offset(7), slot_offset(8)), (SLOT as u64, 0), "odd → slot 1, even → 0");

        // the whole file as each slot holds it (`None` = never written)
        let file = |first: Option<Vec<u8>>, second: Option<Vec<u8>>| {
            let mut bytes = vec![0; 2 * SLOT];
            for (at, slot) in [(0, first), (SLOT, second)] {
                if let Some(slot) = slot {
                    bytes[at..at + slot.len()].copy_from_slice(&slot);
                }
            }
            bytes
        };
        let commit = |seq: u64, body: &[u8]| Some(encode(identity, seq, body));
        let torn = |seq: u64, body: &[u8]| {
            let mut slot = encode(identity, seq, body);
            slot[HEADER] ^= 1;
            Some(slot)
        };
        // body_len claims past the slot's end: torn on its length, before any CRC is read
        let mut overlong = encode(identity, 3, &[1]);
        overlong[20..24].copy_from_slice(&(SLOT as u32).to_le_bytes());

        type Commit = Option<(u64, Vec<u8>)>;
        let cases: [(&str, Vec<u8>, Commit); 6] = [
            ("created, never committed", file(None, None), None),
            ("first commit", file(None, commit(1, &[1])), Some((1, vec![1]))),
            ("two commits: the newer", file(commit(2, &[2]), commit(1, &[1])), Some((2, vec![2]))),
            ("newer torn: the older", file(torn(4, &[4]), commit(3, &[3])), Some((3, vec![3]))),
            ("first commit torn", file(None, torn(1, &[1])), None),
            ("overlong body = torn", file(None, Some(overlong.clone())), None),
        ];
        for (why, bytes, want) in cases {
            let found = latest(identity, &bytes).unwrap_or_else(|error| panic!("{why}: {error}"));
            assert_eq!(found.map(|(seq, body)| (seq, body.to_vec())), want, "{why}");
        }

        use ManifestError::{BodyLength, Checksum, Format, Kind, Layout, Network, Sequence};
        let (torn_pair, twin_pair) =
            (file(torn(2, &[2]), torn(1, &[1])), file(commit(1, &[1]), commit(1, &[2])));
        let overlong_pair = file(Some(overlong.clone()), Some(overlong));
        let both_torn = latest(identity, &torn_pair);
        let both_overlong = latest(identity, &overlong_pair);
        let twins = latest(identity, &twin_pair);
        let older_layout = latest(identity, &[1; 40]);
        let empty = latest(identity, &[]);
        let one = file(None, commit(1, &[1]));
        let (block, main) = (IndexKind::CompactBlock, NetworkType::Main);
        let kind = latest(Identity { kind: block, ..identity }, &one);
        let format = latest(Identity { format: 4, ..identity }, &one);
        let network = latest(Identity { network: main, ..identity }, &one);
        assert!(matches!(both_torn, Err(Checksum)), "a crash tears one slot, never both");
        let room = SLOT - HEADER - CRC;
        let named = matches!(both_overlong, Err(BodyLength { claimed, actual })
            if claimed as usize == SLOT && actual == room);
        assert!(named, "the claimed length beside the room the slot holds: {both_overlong:?}");
        assert!(matches!(twins, Err(Sequence { seq: 1 })));
        assert!(matches!(older_layout, Err(Layout { len: 40 })));
        assert!(matches!(empty, Err(Layout { len: 0 })), "only ever created whole");
        assert!(matches!(kind, Err(Kind { expected: IndexKind::CompactBlock, found: 2 })));
        assert!(matches!(format, Err(Format { expected: 4, found: 3 })));
        assert!(matches!(network, Err(Network { expected: NetworkType::Main, found: 1 })));
    }

    /// Tip presence follows the count on both sides of the codec; a zero hash is still a tip; on
    /// disk = block count from genesis (tip height + 1) ‖ hash
    #[test]
    fn committed_round_trips_and_refuses_a_tip_with_nothing_committed() {
        let tip = |height: u32, hash: u8| Committed {
            tip: Some(BlockRef {
                hash: [hash; 32].into(),
                height: Height::try_from(height).expect("h"),
            }),
        };
        let golden = [3u64.to_le_bytes().as_slice(), &[7; 32]].concat();
        let mut bytes = Vec::new();
        tip(2, 7).encode(&mut bytes);
        assert_eq!(bytes, golden, "tip at 2 = count 3 on disk");

        for committed in [Committed::EMPTY, tip(2, 7), tip(0, 0)] {
            let mut bytes = Vec::new();
            committed.encode(&mut bytes);
            let mut body = BodyReader::new(&bytes);
            assert_eq!(Committed::decode(&mut body).expect("decode"), committed);
            body.finish().expect("no tail");
        }

        let mut bytes = 0u64.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[7; 32]);
        let tip_only = Committed::decode(&mut BodyReader::new(&bytes));
        assert!(matches!(tip_only, Err(ManifestError::Body("tip hash with nothing committed"))));
        let tail = BodyReader::new(&[1]).finish();
        assert!(matches!(tail, Err(ManifestError::Body("trailing bytes"))));
    }
}
