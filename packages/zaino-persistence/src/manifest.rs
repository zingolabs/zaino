//! `MANIFEST`: index's one commit point
//!
//! ```text
//! file = slot 0 ‖ slot 1                     2 × SLOT bytes, zero-filled when created
//! slot = magic b"ZAINOMS\0" ‖ format u16 ‖ kind u8 ‖ network u8 ‖ seq u64 ‖ body_len u32
//!        ‖ body ‖ crc32 ‖ (unused to the slot's end)
//! ```
//!
//! - little-endian throughout
//! - CRC-32 (IEEE) over the slot's bytes before it
//! - commit `seq` overwrites slot `seq % 2` in place (other slot = the commit before it)
//! - durable state = valid slot with the highest `seq` ([`latest`])
//! - in place, not a renamed file (overwrite below EOF = no metadata change: `fdatasync` never
//!   waits on the filesystem journal or every other file's writeback)
//! - slot codec only (create / commit / fdatasync I/O: `index_dir.rs`)

use std::path::PathBuf;

use zaino_primitives::types::BlockRef;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::BlockHash;

use crate::schema::IndexKind;

const MAGIC: [u8; 8] = *b"ZAINOMS\0";
const HEADER: usize = 24;
const CRC: usize = 4;

/// Slot capacity (largest body = segment store's lists: [`crate::manifest_fits`] bounds them)
pub(crate) const SLOT: usize = 64 << 10;

/// Both slots
pub(crate) const FILE_LEN: u64 = 2 * SLOT as u64;

/// What a directory must have been written as (any mismatch refused = chain-identity check)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) kind: IndexKind,
    pub(crate) format: u16,
    pub(crate) network: NetworkType,
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

    #[error("directory holds index kind {found}, expected {}", expected.name())]
    Kind { expected: IndexKind, found: u8 },

    #[error("directory holds format {found}, this build reads format {expected}")]
    Format { expected: u16, found: u16 },

    #[error("directory was built for network tag {found}, configured network is {expected:?}")]
    Network { expected: NetworkType, found: u8 },

    #[error("malformed manifest body: {0}")]
    Body(&'static str),

    #[error("{} holds data but the directory has no MANIFEST", path.display())]
    Unmanifested { path: PathBuf },
}

pub(crate) fn fits(body: usize) -> bool {
    HEADER + body + CRC <= SLOT
}

/// Commit `seq`'s slot bytes (written at [`slot_offset`]`(seq)`)
///
/// - rest of the slot untouched, outside the CRC
pub(crate) fn encode(identity: Identity, seq: u64, body: &[u8]) -> Vec<u8> {
    assert!(
        fits(body.len()),
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

/// Commit `seq`'s offset: alternating slots (never over the commit before it)
pub(crate) fn slot_offset(seq: u64) -> u64 {
    (seq % 2) * SLOT as u64
}

/// - `Unwritten` = all zeros (no commit used it yet)
/// - `Torn` = fails magic, length or CRC (write a crash interrupted)
/// - `Torn`'s `ManifestError` = why, for the error
#[derive(Debug)]
enum Slot<'a> {
    Unwritten,
    Torn(ManifestError),
    Valid { seq: u64, body: &'a [u8] },
}

/// `Err` = whole, checksummed slot of another index, format or network (refused outright, never
/// mistaken for a torn write)
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
    let seq = u64::from_le_bytes(std::array::from_fn(|at| header[12 + at]));
    Ok(Slot::Valid { seq, body })
}

/// Durable `(seq, body)` of a whole manifest file (`None` = no commit ever completed)
///
/// - both valid → higher `seq` (other = the commit before it)
/// - valid + torn → the valid one (torn = later commit interrupted before its ack)
///   - bit rot in the live slot reads as torn too → rollback one commit (CRC can't tell them apart)
/// - none valid, <= 1 torn → nothing durable yet (first commit interrupted)
/// - both torn → error (created zeroed + synced first, a crash tears only the slot written)
/// - other length → error (older layout, file only ever created whole)
pub(crate) fn latest(
    identity: Identity,
    file: &[u8],
) -> Result<Option<(u64, &[u8])>, ManifestError> {
    if file.len() as u64 != FILE_LEN {
        return Err(ManifestError::Layout { len: file.len() as u64 });
    }
    let (first, second) = file.split_at(SLOT);
    let slots = [decode_slot(identity, first)?, decode_slot(identity, second)?];

    let mut commits = slots.iter().filter_map(|slot| match slot {
        Slot::Valid { seq, body } => Some((*seq, *body)),
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

/// Durable tip (`None` = nothing durable): every body's first 40 bytes
///
/// - block count from genesis ‖ tip hash (zeros when empty)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DurableTip {
    pub(crate) tip: Option<BlockRef>,
}

impl DurableTip {
    pub(crate) const EMPTY: Self = Self { tip: None };

    /// Durable tip height, inclusive (`None` = nothing durable)
    pub(crate) fn height(&self) -> Option<BlockHeight> {
        self.tip.map(|tip| tip.height)
    }

    /// Durable blocks from genesis (what a file sized per height is checked against)
    pub(crate) fn count(&self) -> u64 {
        self.height().map_or(0, |height| u64::from(height) + 1)
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.count().to_le_bytes());
        out.extend_from_slice(&self.tip.map_or([0; 32], |tip| tip.hash.0));
    }

    pub(crate) fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        let count = body.u64()?;
        let hash = body.array::<32>()?;
        // presence = count > 0 (a zero hash is a hash, not a sentinel)
        let Some(last) = count.checked_sub(1) else {
            return match hash == [0; 32] {
                true => Ok(Self::EMPTY),
                false => Err(ManifestError::Body("tip hash with nothing durable")),
            };
        };
        let height = u32::try_from(last)
            .map(BlockHeight::from)
            .map_err(|_| ManifestError::Body("durable block count past the height ceiling"))?;
        Ok(Self { tip: Some(BlockRef { hash: BlockHash(hash), height }) })
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
        let (head, rest) =
            self.rest.split_first_chunk::<N>().ok_or(ManifestError::Body("truncated"))?;
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
mod tests {
    use super::*;

    use crate::golden::assert_fields;

    const IDENTITY: Identity =
        Identity { kind: IndexKind::TreeState, format: 3, network: NetworkType::Test };

    /// Whole file as each slot holds it (`None` = never written)
    fn file(first: Option<Vec<u8>>, second: Option<Vec<u8>>) -> Vec<u8> {
        let mut bytes = vec![0; 2 * SLOT];
        for (at, slot) in [(0, first), (SLOT, second)] {
            if let Some(slot) = slot {
                bytes[at..at + slot.len()].copy_from_slice(&slot);
            }
        }
        bytes
    }

    fn commit(seq: u64, body: &[u8]) -> Option<Vec<u8>> {
        Some(encode(IDENTITY, seq, body))
    }

    /// Commit `seq`'s slot, first body byte flipped (CRC fails)
    fn torn(seq: u64, body: &[u8]) -> Option<Vec<u8>> {
        let mut slot = encode(IDENTITY, seq, body);
        slot[HEADER] ^= 1;
        Some(slot)
    }

    /// Commit 3's slot claiming a body to the slot's end (torn on its length, before any CRC read)
    fn overlong() -> Option<Vec<u8>> {
        let mut slot = encode(IDENTITY, 3, &[1]);
        slot[20..24].copy_from_slice(&(SLOT as u32).to_le_bytes());
        Some(slot)
    }

    #[test]
    fn a_slot_is_its_golden_bytes() {
        let encoded = encode(IDENTITY, 7, &[0xaa, 0xbb]);

        #[rustfmt::skip]
        assert_fields(&encoded, &[
            ("magic",       b"ZAINOMS\0"),
            ("format",      &3u16.to_le_bytes()),
            ("kind",        &[2]),
            ("network tag", &[1]),
            ("seq",         &7u64.to_le_bytes()),
            ("body len",    &2u32.to_le_bytes()),
            ("body",        &[0xaa, 0xbb]),
            ("crc",         &0xc61c_cc94u32.to_le_bytes()),
        ]);
    }

    #[test]
    fn latest_reads_the_durable_commit_off_every_slot_pair_a_crash_leaves() {
        /// `(seq, body)` durable (`None` = nothing durable)
        type Durable = Option<(u64, Vec<u8>)>;
        // (case, file, durable)
        #[rustfmt::skip]
        let cases: [(&str, Vec<u8>, Durable); 6] = [
            ("created, never committed", file(None, None),                           None),
            ("first commit",             file(None, commit(1, &[1])),                Some((1, vec![1]))),
            ("two commits: the newer",   file(commit(2, &[2]), commit(1, &[1])),     Some((2, vec![2]))),
            ("newer torn: the older",    file(torn(4, &[4]), commit(3, &[3])),       Some((3, vec![3]))),
            ("first commit torn",        file(None, torn(1, &[1])),                  None),
            ("first commit overlong",    file(None, overlong()),                     None),
        ];
        for (case, bytes, durable) in cases {
            let found = latest(IDENTITY, &bytes).unwrap_or_else(|error| panic!("{case}: {error}"));
            assert_eq!(found.map(|(seq, body)| (seq, body.to_vec())), durable, "{case}");
        }
    }

    /// Commit 1 in slot 1, read as `identity` (else this file's), or another file shape
    #[test]
    fn latest_refuses_slot_pairs_no_crash_leaves_and_another_identity_s_directory() {
        let committed = file(None, commit(1, &[1]));
        let room = SLOT - HEADER - CRC;
        // (case, read as, file, refused)
        #[rustfmt::skip]
        let cases: [(&str, Identity, Vec<u8>, String); 8] = [
            ("both slots torn",      IDENTITY, file(torn(2, &[2]), torn(1, &[1])),     "MANIFEST checksum mismatch".into()),
            ("both slots overlong",  IDENTITY, file(overlong(), overlong()),           format!("MANIFEST body claims {SLOT} bytes, {room} present")),
            ("both slots commit 1",  IDENTITY, file(commit(1, &[1]), commit(1, &[2])), "MANIFEST slots both claim commit 1".into()),
            ("older 40-byte layout", IDENTITY, vec![1; 40],                            "MANIFEST is 40 bytes, not this build's two-slot layout: resync the index".into()),
            ("empty file",           IDENTITY, vec![],                                 "MANIFEST is 0 bytes, not this build's two-slot layout: resync the index".into()),
            ("another index kind",   Identity { kind: IndexKind::CompactBlock, ..IDENTITY }, committed.clone(), "directory holds index kind 2, expected compact_block".into()),
            ("another format",       Identity { format: 4, ..IDENTITY },                     committed.clone(), "directory holds format 3, this build reads format 4".into()),
            ("another network",      Identity { network: NetworkType::Main, ..IDENTITY },    committed,         "directory was built for network tag 1, configured network is Main".into()),
        ];
        for (case, identity, bytes, refused) in cases {
            let error = latest(identity, &bytes).map(|_| ()).expect_err(case);
            assert_eq!(error.to_string(), refused, "{case}");
        }
    }

    /// Presence = count > 0: a zero hash at height 0 is still a tip
    #[test]
    fn nothing_durable_and_a_zero_hash_at_genesis_round_trip_apart() {
        let genesis =
            DurableTip { tip: Some(BlockRef { hash: BlockHash([0; 32]), height: 0.into() }) };
        for durable in [DurableTip::EMPTY, genesis] {
            let mut bytes = Vec::new();
            durable.encode(&mut bytes);

            let decoded = DurableTip::decode(&mut BodyReader::new(&bytes)).expect("decode");

            assert_eq!(decoded, durable, "{durable:?} round trip");
        }
    }

    #[test]
    fn a_tip_hash_with_nothing_durable_is_refused() {
        let bytes = [0u64.to_le_bytes().as_slice(), &[7; 32]].concat();

        let refused =
            DurableTip::decode(&mut BodyReader::new(&bytes)).expect_err("count 0, hash 7s");

        assert_eq!(refused.to_string(), "malformed manifest body: tip hash with nothing durable");
    }

    #[test]
    fn a_body_reader_refuses_trailing_bytes() {
        let refused = BodyReader::new(&[1]).finish().expect_err("one byte unread");

        assert_eq!(refused.to_string(), "malformed manifest body: trailing bytes");
    }
}
