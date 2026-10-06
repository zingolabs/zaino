//! The on-disk run-log segment format.
//!
//! A deferred namespace's writes accumulate in a run log: a flat file of
//! **segments**, one appended per bulk commit. Each segment is a self-describing,
//! length-framed block — a fixed header followed by the batch's `(key, value)`
//! pairs sorted bytewise by key. Segments are never rewritten; a commit appends
//! one and fsyncs, and the manifest records the committed byte length so a torn
//! tail from a crash is truncated on reopen (see [`super`]).
//!
//! # Layout
//!
//! ```text
//! segment := header payload
//! header  := magic[4] version[2] entry_count[8] payload_len[8] checksum[8]   (30 bytes, big-endian)
//! payload := record*                                                          (payload_len bytes)
//! record  := key_len[4] key[key_len] value_len[4] value[value_len]           (big-endian lengths)
//! ```
//!
//! `checksum` is a non-cryptographic [FNV-1a] hash of `payload`. The header's
//! `payload_len` already detects a truncated tail (the file is shorter than the
//! header claims); the checksum additionally catches silent corruption inside a
//! segment. FNV-1a is hand-rolled — a few lines, no table — rather than pulling a
//! CRC crate: the job is integrity detection on sequential reads, not collision
//! resistance, and it keeps the segment format free of a new dependency.
//!
//! [FNV-1a]: https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function

use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use zaino_persistence::{RawKey, RawValue};

/// Segment magic: "Zaino Deferred SeGment".
const MAGIC: [u8; 4] = *b"ZDSG";

/// Segment format version. Bumped if the layout changes; a segment with an
/// unexpected version is rejected rather than misparsed.
const VERSION: u16 = 1;

/// Fixed header size: `magic(4) + version(2) + entry_count(8) + payload_len(8) + checksum(8)`.
pub(crate) const HEADER_LEN: usize = 4 + 2 + 8 + 8 + 8;

/// Size of a record's length prefix (one each for key and value).
const LEN_PREFIX: usize = 4;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Incremental FNV-1a 64-bit hash: fed the payload in one shot by the encoder and
/// chunk by chunk by the streaming [`SegmentCursor`], so the reader never holds a
/// whole segment's payload in memory just to checksum it.
#[derive(Debug, Clone, Copy)]
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(FNV_OFFSET)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
}

/// FNV-1a 64-bit hash of `bytes` — the segment payload checksum.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = Fnv1a::new();
    hash.update(bytes);
    hash.finish()
}

/// A failure decoding a run-log segment: corruption, an unexpected version, or a
/// length that runs past the available bytes.
///
/// Surfaced to the port as [`CommitError::DeferredLogCorrupt`](zaino_persistence::CommitError::DeferredLogCorrupt)
/// (as a boxed `#[source]`), naming the namespace whose log failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SegmentError {
    /// The segment header's magic bytes did not match.
    #[error("bad segment magic: expected {expected:02x?}, found {found:02x?}")]
    BadMagic {
        /// The magic the format expects.
        expected: [u8; 4],
        /// The magic read from disk.
        found: [u8; 4],
    },
    /// The segment header's version is not one this build understands.
    #[error("unsupported segment version {found} (this build writes {expected})")]
    BadVersion {
        /// The version this build writes and reads.
        expected: u16,
        /// The version read from disk.
        found: u16,
    },
    /// The available bytes are shorter than the header or the framed payload
    /// claims — a truncated or torn segment.
    #[error("truncated segment: need {need} bytes, have {have}")]
    Truncated {
        /// The byte count the frame requires.
        need: usize,
        /// The byte count actually available.
        have: usize,
    },
    /// The payload checksum did not match the header — silent corruption.
    #[error("segment checksum mismatch: header {expected:#018x}, computed {computed:#018x}")]
    ChecksumMismatch {
        /// The checksum stored in the header.
        expected: u64,
        /// The checksum computed over the payload bytes.
        computed: u64,
    },
    /// A key or value is longer than the 32-bit length prefix can frame.
    #[error("entry too large to frame: {len} bytes exceeds the u32 length prefix")]
    EntryTooLarge {
        /// The offending length.
        len: usize,
    },
}

/// Encode one segment from a batch's sorted `(key, value)` pairs.
///
/// The [`BTreeMap`] both sorts keys bytewise and collapses a key written twice in
/// one batch to its last value — matching plain-`put` overwrite semantics, though
/// catch-up keys are unique by construction. Returns the segment bytes ready to
/// append to the run log.
pub(crate) fn encode_segment(
    entries: &BTreeMap<RawKey, RawValue>,
) -> Result<Vec<u8>, SegmentError> {
    let mut payload = Vec::new();
    for (key, value) in entries {
        let key_len =
            u32::try_from(key.len()).map_err(|_| SegmentError::EntryTooLarge { len: key.len() })?;
        let value_len = u32::try_from(value.len())
            .map_err(|_| SegmentError::EntryTooLarge { len: value.len() })?;
        payload.extend_from_slice(&key_len.to_be_bytes());
        payload.extend_from_slice(key);
        payload.extend_from_slice(&value_len.to_be_bytes());
        payload.extend_from_slice(value);
    }

    let entry_count = u64::try_from(entries.len())
        .map_err(|_| SegmentError::EntryTooLarge { len: entries.len() })?;
    let payload_len = u64::try_from(payload.len())
        .map_err(|_| SegmentError::EntryTooLarge { len: payload.len() })?;
    let checksum = fnv1a_64(&payload);

    let mut segment = Vec::with_capacity(HEADER_LEN + payload.len());
    segment.extend_from_slice(&MAGIC);
    segment.extend_from_slice(&VERSION.to_be_bytes());
    segment.extend_from_slice(&entry_count.to_be_bytes());
    segment.extend_from_slice(&payload_len.to_be_bytes());
    segment.extend_from_slice(&checksum.to_be_bytes());
    segment.extend_from_slice(&payload);
    Ok(segment)
}

/// A decoded segment header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentHeader {
    /// Number of `(key, value)` records in the payload.
    pub(crate) entry_count: u64,
    /// Byte length of the payload following the header.
    pub(crate) payload_len: u64,
    /// FNV-1a checksum of the payload.
    pub(crate) checksum: u64,
}

impl SegmentHeader {
    /// The segment's total byte length (header plus payload).
    pub(crate) fn total_len(&self) -> u64 {
        // HEADER_LEN is a small constant; a payload large enough to overflow a
        // u64 when added to it is not representable on disk.
        self.payload_len.saturating_add(HEADER_LEN as u64)
    }
}

/// Parse a segment header from the first [`HEADER_LEN`] bytes of `bytes`,
/// validating magic and version. Does not read the payload.
pub(crate) fn parse_header(bytes: &[u8]) -> Result<SegmentHeader, SegmentError> {
    if bytes.len() < HEADER_LEN {
        return Err(SegmentError::Truncated {
            need: HEADER_LEN,
            have: bytes.len(),
        });
    }
    let magic: [u8; 4] = bytes[0..4].try_into().expect("4-byte magic slice");
    if magic != MAGIC {
        return Err(SegmentError::BadMagic {
            expected: MAGIC,
            found: magic,
        });
    }
    let version = u16::from_be_bytes(bytes[4..6].try_into().expect("2-byte version slice"));
    if version != VERSION {
        return Err(SegmentError::BadVersion {
            expected: VERSION,
            found: version,
        });
    }
    let entry_count = u64::from_be_bytes(bytes[6..14].try_into().expect("8-byte entry count"));
    let payload_len = u64::from_be_bytes(bytes[14..22].try_into().expect("8-byte payload len"));
    let checksum = u64::from_be_bytes(bytes[22..30].try_into().expect("8-byte checksum"));
    Ok(SegmentHeader {
        entry_count,
        payload_len,
        checksum,
    })
}

/// Decode a segment's payload into its `(key, value)` records, verifying the
/// checksum and that every record's length prefix stays within the payload.
///
/// This materialises the whole payload and is used by the format unit tests;
/// the production reader is the streaming [`SegmentCursor`], which never holds a
/// segment's payload in memory (the merge keeps a cursor per segment at once).
#[cfg(test)]
fn decode_payload(
    header: &SegmentHeader,
    payload: &[u8],
) -> Result<Vec<(RawKey, RawValue)>, SegmentError> {
    let payload_len = usize::try_from(header.payload_len).map_err(|_| SegmentError::Truncated {
        need: usize::MAX,
        have: payload.len(),
    })?;
    if payload.len() < payload_len {
        return Err(SegmentError::Truncated {
            need: payload_len,
            have: payload.len(),
        });
    }
    let payload = &payload[..payload_len];
    let computed = fnv1a_64(payload);
    if computed != header.checksum {
        return Err(SegmentError::ChecksumMismatch {
            expected: header.checksum,
            computed,
        });
    }

    let mut entries = Vec::new();
    let mut cursor = 0usize;
    while cursor < payload.len() {
        let key = read_framed(payload, &mut cursor)?;
        let value = read_framed(payload, &mut cursor)?;
        entries.push((key, value));
    }
    Ok(entries)
}

/// Read one length-framed byte string at `*cursor`, advancing it past the record.
#[cfg(test)]
fn read_framed(payload: &[u8], cursor: &mut usize) -> Result<Vec<u8>, SegmentError> {
    let have = payload.len().saturating_sub(*cursor);
    let prefix_end = cursor
        .checked_add(LEN_PREFIX)
        .filter(|end| *end <= payload.len())
        .ok_or(SegmentError::Truncated {
            need: LEN_PREFIX,
            have,
        })?;
    let len_bytes: [u8; LEN_PREFIX] = payload[*cursor..prefix_end]
        .try_into()
        .expect("LEN_PREFIX-byte length slice");
    let len = usize::try_from(u32::from_be_bytes(len_bytes)).expect("u32 fits usize");
    let data_end = prefix_end
        .checked_add(len)
        .filter(|end| *end <= payload.len())
        .ok_or(SegmentError::Truncated { need: len, have })?;
    let data = payload[prefix_end..data_end].to_vec();
    *cursor = data_end;
    Ok(data)
}

/// How many bytes a [`SegmentCursor`] reads from the log per positioned read.
/// Bounds the reader's memory: the merge keeps one cursor per segment live at
/// once, so this times the segment count is the merge's buffer footprint.
const READ_BUF_CAP: usize = 8 * 1024;

/// A streaming reader over one segment's payload, yielding its `(key, value)`
/// records in stored (bytewise key) order.
///
/// All cursors over one run log share a single file handle and read with
/// positioned reads ([`FileExt::read_at`]), so a merge over thousands of segments
/// needs one file descriptor, not one per segment. Each cursor holds only a small
/// [`READ_BUF_CAP`] buffer and the record it has peeked. The payload checksum is
/// folded in as bytes are read and verified once the payload is exhausted, so a
/// corrupt segment fails without the payload ever being fully resident.
pub(crate) struct SegmentCursor {
    /// The shared run-log handle.
    file: Arc<File>,
    /// Next absolute file offset to read from.
    pos: u64,
    /// Absolute offset one past the segment's payload.
    end: u64,
    /// Bytes read from the file but not yet consumed into a record.
    buf: Vec<u8>,
    /// Consumed offset within `buf`.
    off: usize,
    /// Running checksum of every payload byte read so far.
    hasher: Fnv1a,
    /// The checksum the header claims, verified when the payload is exhausted.
    expected_checksum: u64,
    /// Whether the exhausted-payload checksum has been verified.
    verified: bool,
    /// The record peeked but not yet taken.
    current: Option<(RawKey, RawValue)>,
}

impl SegmentCursor {
    /// A cursor over the segment whose `header` was parsed at `header_offset`
    /// (its payload begins `HEADER_LEN` bytes later).
    pub(crate) fn new(file: Arc<File>, header: &SegmentHeader, header_offset: u64) -> Self {
        let payload_start = header_offset.saturating_add(HEADER_LEN as u64);
        Self {
            file,
            pos: payload_start,
            end: payload_start.saturating_add(header.payload_len),
            buf: Vec::new(),
            off: 0,
            hasher: Fnv1a::new(),
            expected_checksum: header.checksum,
            verified: false,
            current: None,
        }
    }

    /// The next record without consuming it, or `None` at the segment's end.
    pub(crate) fn peek(&mut self) -> Result<Option<&(RawKey, RawValue)>, SegmentError> {
        self.load()?;
        Ok(self.current.as_ref())
    }

    /// Consume and return the next record.
    pub(crate) fn take(&mut self) -> Result<Option<(RawKey, RawValue)>, SegmentError> {
        self.load()?;
        Ok(self.current.take())
    }

    /// Ensure `current` holds the next record, reading one if needed. At the end
    /// of the payload, verifies the checksum once and leaves `current` `None`.
    fn load(&mut self) -> Result<(), SegmentError> {
        if self.current.is_some() {
            return Ok(());
        }
        if self.off == self.buf.len() && self.pos >= self.end {
            if !self.verified {
                self.verified = true;
                let computed = self.hasher.finish();
                if computed != self.expected_checksum {
                    return Err(SegmentError::ChecksumMismatch {
                        expected: self.expected_checksum,
                        computed,
                    });
                }
            }
            return Ok(());
        }
        let key = self.read_record()?;
        let value = self.read_record()?;
        self.current = Some((key, value));
        Ok(())
    }

    /// Read one length-framed record from the buffered stream.
    fn read_record(&mut self) -> Result<Vec<u8>, SegmentError> {
        let len_bytes = self.read_exact(LEN_PREFIX)?;
        let len = usize::try_from(u32::from_be_bytes(
            len_bytes.as_slice().try_into().expect("LEN_PREFIX bytes"),
        ))
        .expect("u32 fits usize");
        self.read_exact(len)
    }

    /// Read exactly `n` bytes from the buffered stream, refilling from the file as
    /// needed. A short read before `n` bytes are available means a truncated
    /// segment — impossible within the reopen-truncated committed region, so it is
    /// genuine corruption.
    fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, SegmentError> {
        while self.buf.len() - self.off < n {
            if self.pos >= self.end {
                return Err(SegmentError::Truncated {
                    need: n,
                    have: self.buf.len() - self.off,
                });
            }
            self.refill()?;
        }
        let out = self.buf[self.off..self.off + n].to_vec();
        self.off += n;
        Ok(out)
    }

    /// Read a [`READ_BUF_CAP`]-bounded chunk from the file into `buf`, folding it
    /// into the checksum. Compacts the already-consumed prefix first.
    fn refill(&mut self) -> Result<(), SegmentError> {
        if self.off > 0 {
            self.buf.drain(..self.off);
            self.off = 0;
        }
        let remaining = self.end - self.pos;
        let want = usize::try_from(remaining)
            .unwrap_or(READ_BUF_CAP)
            .min(READ_BUF_CAP);
        let mut tmp = vec![0u8; want];
        let read = self
            .file
            .read_at(&mut tmp, self.pos)
            .map_err(|_| SegmentError::Truncated {
                need: want,
                have: 0,
            })?;
        if read == 0 {
            // EOF before the region end: the file is shorter than the header
            // claims. Clamp so the next `read_exact` reports truncation.
            self.pos = self.end;
            return Ok(());
        }
        self.hasher.update(&tmp[..read]);
        self.buf.extend_from_slice(&tmp[..read]);
        self.pos += u64::try_from(read).expect("read length fits u64");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch of pairs whose insertion order differs from bytewise key order, so
    /// the round trip proves the encoder sorts.
    fn sample() -> BTreeMap<RawKey, RawValue> {
        let mut map = BTreeMap::new();
        map.insert(vec![0x02, 0x00], b"last-by-bytes".to_vec());
        map.insert(vec![0x00, 0x02], b"first-by-bytes".to_vec());
        map.insert(vec![0x01, 0x01], b"middle".to_vec());
        map.insert(vec![], b"empty-key".to_vec());
        map.insert(vec![0xff], Vec::new());
        map
    }

    /// Decode a standalone segment from the front of `bytes`: header then payload.
    fn decode_segment(bytes: &[u8]) -> Result<Vec<(RawKey, RawValue)>, SegmentError> {
        let header = parse_header(bytes)?;
        decode_payload(&header, &bytes[HEADER_LEN..])
    }

    #[test]
    fn encode_decode_round_trip_is_sorted() {
        let entries = sample();
        let bytes = encode_segment(&entries).expect("encode");
        let decoded = decode_segment(&bytes).expect("decode");

        let want: Vec<(RawKey, RawValue)> = entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(
            decoded, want,
            "round trip preserves sorted (key, value) pairs"
        );
    }

    #[test]
    fn header_records_entry_count_and_payload_len() {
        let entries = sample();
        let bytes = encode_segment(&entries).expect("encode");
        let header = parse_header(&bytes).expect("header");
        assert_eq!(header.entry_count, entries.len() as u64);
        assert_eq!(
            header.total_len() as usize,
            bytes.len(),
            "total_len covers the whole segment"
        );
    }

    #[test]
    fn empty_segment_round_trips() {
        let bytes = encode_segment(&BTreeMap::new()).expect("encode empty");
        assert_eq!(bytes.len(), HEADER_LEN, "an empty segment is header-only");
        assert!(decode_segment(&bytes).expect("decode empty").is_empty());
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = encode_segment(&sample()).expect("encode");
        bytes[0] ^= 0xff;
        assert!(matches!(
            decode_segment(&bytes),
            Err(SegmentError::BadMagic { .. })
        ));
    }

    #[test]
    fn bad_version_is_rejected() {
        let mut bytes = encode_segment(&sample()).expect("encode");
        // Flip a bit in the version field (bytes 4..6).
        bytes[5] ^= 0x01;
        assert!(matches!(
            decode_segment(&bytes),
            Err(SegmentError::BadVersion { .. })
        ));
    }

    #[test]
    fn corrupt_payload_fails_the_checksum() {
        let mut bytes = encode_segment(&sample()).expect("encode");
        // Corrupt one payload byte, leaving the header (and its checksum) intact.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        assert!(matches!(
            decode_segment(&bytes),
            Err(SegmentError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn truncated_header_is_rejected() {
        let bytes = encode_segment(&sample()).expect("encode");
        assert!(matches!(
            decode_segment(&bytes[..HEADER_LEN - 1]),
            Err(SegmentError::Truncated { .. })
        ));
    }

    #[test]
    fn truncated_payload_tail_is_rejected() {
        let bytes = encode_segment(&sample()).expect("encode");
        // Drop the final payload byte: the header still claims the full length.
        assert!(matches!(
            decode_segment(&bytes[..bytes.len() - 1]),
            Err(SegmentError::Truncated { .. })
        ));
    }

    #[test]
    fn checksum_distinguishes_payloads() {
        let mut other = BTreeMap::new();
        other.insert(vec![0x00, 0x02], b"first-by-bytes".to_vec());
        let a = parse_header(&encode_segment(&sample()).expect("a")).expect("ha");
        let b = parse_header(&encode_segment(&other).expect("b")).expect("hb");
        assert_ne!(
            a.checksum, b.checksum,
            "different payloads hash differently"
        );
    }
}
