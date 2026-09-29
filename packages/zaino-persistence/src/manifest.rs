//! `MANIFEST`: an index's one commit point (`docs/design/durability.md` §2)
//!
//! ```text
//! magic b"ZAINOMF\0" ‖ format u16 ‖ kind u8 ‖ network u8 ‖ body_len u32 ‖ body ‖ crc32
//! ```
//!
//! - little-endian throughout; CRC-32 (IEEE) over everything before it

use zaino_primitives::types::{BlockHash, Extent};
use zcash_protocol::consensus::NetworkType;

const MAGIC: [u8; 8] = *b"ZAINOMF\0";
const HEADER: usize = 16;
const CRC: usize = 4;

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

    #[error("MANIFEST is {len} bytes, shorter than its header and checksum")]
    Short { len: usize },

    #[error("MANIFEST does not start with the zaino manifest magic")]
    Magic,

    #[error("MANIFEST checksum mismatch")]
    Checksum,

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

pub fn encode(identity: Identity, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + body.len() + CRC);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&identity.format.to_le_bytes());
    out.push(identity.kind as u8);
    out.push(network_tag(identity.network));
    out.extend_from_slice(&u32::try_from(body.len()).expect("manifest body < 4 GiB").to_le_bytes());
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32fast::hash(&out).to_le_bytes());
    out
}

/// Body of a manifest written as `identity`
pub fn decode(identity: Identity, bytes: &[u8]) -> Result<&[u8], ManifestError> {
    let short = ManifestError::Short { len: bytes.len() };
    let (covered, crc) =
        bytes.split_last_chunk::<CRC>().ok_or(ManifestError::Short { len: bytes.len() })?;
    let (header, body) = covered.split_first_chunk::<HEADER>().ok_or(short)?;

    if header[..8] != MAGIC {
        return Err(ManifestError::Magic);
    }
    if crc32fast::hash(covered) != u32::from_le_bytes(*crc) {
        return Err(ManifestError::Checksum);
    }
    let claimed = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
    if usize::try_from(claimed).ok() != Some(body.len()) {
        return Err(ManifestError::BodyLength { claimed, actual: body.len() });
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

    Ok(body)
}

/// `dir`'s committed body, read offline (plain read, no lock); `None` = never committed, a
/// manifest that will not decode as `identity` = `InvalidData`
pub fn read(dir: &std::path::Path, identity: Identity) -> std::io::Result<Option<Vec<u8>>> {
    let bytes = match std::fs::read(dir.join(crate::dir::MANIFEST)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    decode(identity, &bytes)
        .map(|body| Some(body.to_vec()))
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Committed extent + its tip hash: every body's first 40 bytes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    pub extent: Extent,
    pub tip: Option<BlockHash>,
}

impl Committed {
    pub const EMPTY: Self = Self { extent: Extent::ZERO, tip: None };

    /// Tip present iff the extent is non-empty
    pub fn new(extent: Extent, tip: Option<BlockHash>) -> Self {
        let (non_empty, has_tip) = (extent.last().is_some(), tip.is_some());
        assert_eq!(non_empty, has_tip, "committed tip hash iff extent non-empty");
        Self { extent, tip }
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&u64::from(self.extent).to_le_bytes());
        out.extend_from_slice(&self.tip.map_or([0; 32], <[u8; 32]>::from));
    }

    pub fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        let count = body.u64()?;
        let tip = body.array::<32>()?;
        let extent = Extent::from_count(count)
            .map_err(|_| ManifestError::Body("committed count past the height ceiling"))?;
        // presence = count > 0 (a zero hash is a hash, not a sentinel)
        match extent.last() {
            None if tip == [0; 32] => Ok(Self::EMPTY),
            None => Err(ManifestError::Body("tip hash with nothing committed")),
            Some(_) => Ok(Self { extent, tip: Some(BlockHash::from(tip)) }),
        }
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

    /// Layout pinned byte for byte, and every header field checked on decode
    #[test]
    fn manifest_golden_bytes_round_trip_and_every_mismatch_is_refused() {
        let identity =
            Identity { kind: IndexKind::TreeState, format: 3, network: NetworkType::Test };
        let encoded = encode(identity, &[0xaa, 0xbb]);

        let mut expected = b"ZAINOMF\0".to_vec();
        expected.extend_from_slice(&[3, 0, 2, 1, 2, 0, 0, 0, 0xaa, 0xbb]);
        expected.extend_from_slice(&crc32fast::hash(&expected).to_le_bytes());
        assert_eq!(encoded, expected);
        assert_eq!(decode(identity, &encoded).expect("decode"), &[0xaa, 0xbb]);

        let mut flipped = encoded.clone();
        flipped[16] ^= 1;
        let mut magic = encoded.clone();
        magic[0] = b'X';
        // body_len says 3, then re-checksummed so only the length is wrong
        let mut long_body = encoded[..encoded.len() - CRC].to_vec();
        long_body[12] = 3;
        long_body.extend_from_slice(&crc32fast::hash(&long_body).to_le_bytes());

        assert!(matches!(decode(identity, &encoded[..10]), Err(ManifestError::Short { len: 10 })));
        assert!(matches!(decode(identity, &magic), Err(ManifestError::Magic)));
        assert!(matches!(decode(identity, &flipped), Err(ManifestError::Checksum)));
        use ManifestError::{BodyLength, Format, Kind, Network};
        let (block, main) = (IndexKind::CompactBlock, NetworkType::Main);
        let long = decode(identity, &long_body);
        let kind = decode(Identity { kind: block, ..identity }, &encoded);
        let format = decode(Identity { format: 4, ..identity }, &encoded);
        let network = decode(Identity { network: main, ..identity }, &encoded);
        assert!(matches!(long, Err(BodyLength { claimed: 3, actual: 2 })));
        assert!(matches!(kind, Err(Kind { expected: IndexKind::CompactBlock, found: 2 })));
        assert!(matches!(format, Err(Format { expected: 4, found: 3 })));
        assert!(matches!(network, Err(Network { expected: NetworkType::Main, found: 1 })));
    }

    /// Tip presence follows the count on both sides of the codec; a zero hash is still a tip
    #[test]
    fn committed_round_trips_and_refuses_a_tip_with_nothing_committed() {
        for committed in [
            Committed::EMPTY,
            Committed::new(Extent::from_count(3).expect("3"), Some([7; 32].into())),
            Committed::new(Extent::from_count(1).expect("1"), Some([0; 32].into())),
        ] {
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
