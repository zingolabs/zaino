//! Per-segment membership filter for probed sets: sharded binary fuse, 8-bit fingerprints
//!
//! - BinaryFuse8: ≈9 bits/key, FPR 2^-8
//! - shard = top bits of the key's first 8 bytes → monotone in key order (built while streaming)
//!   and ≤ [`SHARD_KEYS`] per build (bounded scratch, whatever the segment's size)

use std::{io, ops::Range};

use xorf::{BinaryFuse8, BinaryFuse8Ref, DmaSerializable, Filter, FilterRef};

use super::spill::Spill;

/// Target keys per shard (xorf build scratch ≈ 25 B/key → ≈ 26 MiB per shard)
pub(crate) const SHARD_KEYS: u64 = 1 << 20;

/// Serialized `Descriptor` (seed u64, three u32)
const DESCRIPTOR: usize = BinaryFuse8::DESCRIPTOR_LEN;

/// Per-shard table entry: descriptor ‖ fingerprint count u32 LE
const ENTRY: usize = DESCRIPTOR + 4;

/// Filter input for one encoded key (distinct keys → distinct inputs w.h.p.)
///
/// - dups deduped per shard
/// - first word = key's own uniform prefix
/// - later words folded through splitmix64's finalizer
pub(crate) fn probe_hash(key: &[u8]) -> u64 {
    let (first, rest) = key.split_at(key.len().min(8));
    let mut hash = word(first);
    for chunk in rest.chunks(8) {
        hash = mix(hash ^ word(chunk));
    }
    hash
}

/// Big-endian, zero-padded on the right
fn word(bytes: &[u8]) -> u64 {
    let mut padded = [0u8; 8];
    padded[..bytes.len()].copy_from_slice(bytes);
    u64::from_be_bytes(padded)
}

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Shard bits for a segment of `records` keys (`records / 2^bits ≤ SHARD_KEYS`)
pub(crate) fn shard_bits(records: u64) -> u8 {
    let shards = records.div_ceil(SHARD_KEYS).max(1);
    u8::try_from(shards.next_power_of_two().trailing_zeros()).expect("< 64 shard bits")
}

fn shard(key: &[u8], bits: u8) -> usize {
    match bits {
        0 => 0,
        bits => usize::try_from(word(&key[..8]) >> (64 - u32::from(bits)))
            .expect("shard < 2^bits fits usize"),
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FilterError {
    #[error("{0}")]
    Build(&'static str),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Streams ascending keys into the serialized filter section
///
/// - in memory: only the shard being built + the table
/// - each closed shard's fingerprints → straight to `fingerprints`
pub(crate) struct FilterWriter {
    bits: u8,
    current: usize,
    hashes: Vec<u64>,
    table: Vec<u8>,
    fingerprints: Spill,
}

impl FilterWriter {
    pub(crate) fn new(records: u64, fingerprints: Spill) -> Self {
        Self {
            bits: shard_bits(records),
            current: 0,
            hashes: Vec::new(),
            table: Vec::new(),
            fingerprints,
        }
    }

    /// Keys in ascending order (shards then close in order)
    pub(crate) fn push(&mut self, key: &[u8]) -> Result<(), FilterError> {
        let at = shard(key, self.bits);
        assert!(at >= self.current, "filter keys arrive in key order");
        while self.current < at {
            self.close()?;
        }
        self.hashes.push(probe_hash(key));
        Ok(())
    }

    /// Section head `shard bits u8 ‖ 2^bits × (descriptor ‖ count u32 LE)` + the fingerprints
    /// after it
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, Spill), FilterError> {
        while self.current < 1 << self.bits {
            self.close()?;
        }
        let mut head = Vec::with_capacity(1 + self.table.len());
        head.push(self.bits);
        head.extend_from_slice(&self.table);
        Ok((head, self.fingerprints))
    }

    fn close(&mut self) -> Result<(), FilterError> {
        let mut entry = [0u8; ENTRY];
        if !self.hashes.is_empty() {
            self.hashes.sort_unstable();
            self.hashes.dedup();
            let built = BinaryFuse8::try_from(&self.hashes).map_err(FilterError::Build)?;
            built.dma_copy_descriptor_to(&mut entry[..DESCRIPTOR]);
            let fingerprints = built.dma_fingerprints();
            let count = u32::try_from(fingerprints.len()).expect("shard fingerprints < 2^32");
            entry[DESCRIPTOR..].copy_from_slice(&count.to_le_bytes());
            self.fingerprints.push(fingerprints)?;
            self.hashes.clear();
        }
        self.table.extend_from_slice(&entry);
        self.current += 1;
        Ok(())
    }
}

/// Filter section, parsed once (offsets within the section) and probed in place
#[derive(Debug)]
pub(crate) struct FilterLayout {
    bits: u8,
    len: usize,
    shard_ranges: Vec<[Range<usize>; 2]>,
}

impl FilterLayout {
    /// `read` = section's bytes by range (only the table read here, `None` = malformed)
    pub(crate) fn parse<'a>(read: impl Fn(Range<usize>) -> &'a [u8], len: usize) -> Option<Self> {
        let bits = *read(0..1.min(len)).first()?;
        if bits >= 32 {
            return None;
        }
        let count = 1usize << bits;
        let table_len = count.checked_mul(ENTRY)?;
        let table = read(1..1usize.checked_add(table_len).filter(|end| *end <= len)?);
        let mut at = 1 + table_len;
        let mut shard_ranges = Vec::with_capacity(count);
        for (index, entry) in table.chunks_exact(ENTRY).enumerate() {
            let shard_len = u32::from_le_bytes(entry[DESCRIPTOR..].try_into().ok()?) as usize;
            let (descriptor, end) = (1 + index * ENTRY, at.checked_add(shard_len)?);
            shard_ranges.push([descriptor..descriptor + DESCRIPTOR, at..end]);
            at = end;
        }
        (at == len).then_some(Self { bits, len, shard_ranges })
    }

    /// Whole section's length
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// - `false` = certainly absent
    /// - `true` = present or a false positive (≈ 2^-8)
    pub(crate) fn may_contain<'a>(
        &self,
        read: impl Fn(Range<usize>) -> &'a [u8],
        key: &[u8],
    ) -> bool {
        let [descriptor, fingerprints] = self.shard_ranges[shard(key, self.bits)].clone();
        if fingerprints.is_empty() {
            return false;
        }
        BinaryFuse8Ref::from_dma(read(descriptor), read(fingerprints)).contains(&probe_hash(key))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        fs::{Fs, SimFs},
        pages::{FileKind, PagedFile, Sealed},
    };

    /// Uniform first 8 bytes (`mix(n)`), `n` after
    fn key(n: u64) -> Vec<u8> {
        [mix(n).to_be_bytes(), n.to_be_bytes()].concat()
    }

    /// Filter section of `keys` (ascending) as a segment holds it: head, then the fingerprints
    fn section(keys: &[Vec<u8>]) -> Vec<u8> {
        let fs = SimFs::new();
        let dir = Path::new("/f");
        fs.create_dir_all(dir).expect("dir");
        let mut writer =
            FilterWriter::new(keys.len() as u64, Spill::new(fs.clone(), dir.join("spill")));
        for key in keys {
            writer.push(key).expect("push");
        }
        let (head, fingerprints) = writer.finish().expect("build");
        let path = dir.join("0.seg");
        let mut out =
            PagedFile::open(fs.as_ref(), &path, Sealed::EMPTY, FileKind::Segment).expect("segment");
        out.append(&head).expect("head");
        fingerprints.append_to(&mut out).expect("fingerprints");
        fs.contents(&path).expect("section")
    }

    /// Persisted filters stay readable only while these stay identical
    #[test]
    fn probe_hashes_are_golden() {
        // (case, key, hash)
        #[rustfmt::skip]
        let cases: [(&str, &[u8], u64); 4] = [
            ("8 bytes = the key itself",       &[0x01; 8],    0x0101_0101_0101_0101),
            ("short key, zero-padded right",   &[0x01, 0x02], 0x0102_0000_0000_0000),
            ("36 zero bytes",                  &[0; 36],      0),
            ("36 bytes, later words mixed in", &[0xab; 36],   0xad4a_2fdc_9f89_727d),
        ];
        for (case, key, hash) in cases {
            assert_eq!(probe_hash(key), hash, "{case}: {key:02x?}");
        }
        let outpoint =
            |vout: u32| probe_hash(&[[0x11; 32].as_slice(), &vout.to_be_bytes()].concat());
        assert_ne!(outpoint(0), outpoint(1), "outpoints 0 + 1 of one transaction stay distinct");
    }

    /// Shard = top `bits` of the key's first 8 bytes: monotone in key order
    #[test]
    fn shards_hold_at_most_shard_keys_and_follow_key_order() {
        // (records, shard bits)
        #[rustfmt::skip]
        let bits = [
            (0,              0),
            (1,              0),
            (SHARD_KEYS,     0),
            (SHARD_KEYS + 1, 1),
            (5 * SHARD_KEYS, 3),
        ];
        for (records, expected) in bits {
            assert_eq!(shard_bits(records), expected, "shard bits for {records} records");
        }
        // (key, shard bits, shard)
        #[rustfmt::skip]
        let shards: [([u8; 8], u8, usize); 4] = [
            ([0x00; 8], 3, 0),
            ([0x20; 8], 3, 1),
            ([0xff; 8], 3, 7),
            ([0xff; 8], 0, 0),
        ];
        for (key, bits, expected) in shards {
            assert_eq!(shard(&key, bits), expected, "key {key:02x?} at {bits} shard bits");
        }
    }

    /// 1.5 × `SHARD_KEYS` keys (2 shards), 200 000 strangers
    #[test]
    fn a_filter_finds_every_written_key_and_admits_strangers_at_its_false_positive_rate() {
        let mut keys: Vec<Vec<u8>> = (0..3 * SHARD_KEYS / 2).map(key).collect();
        keys.sort_unstable();
        let section = section(&keys);
        let read = |range: Range<usize>| &section[range];

        let layout = FilterLayout::parse(read, section.len()).expect("parses");

        assert_eq!(layout.bits, 1, "shard bits");
        let missed = keys.iter().filter(|key| !layout.may_contain(read, key)).count();
        assert_eq!(missed, 0, "written keys refused");
        let strangers = 200_000u64;
        let admitted =
            (0..strangers).map(|n| key((n + 1) << 40)).filter(|key| layout.may_contain(read, key));
        let admitted = admitted.count();
        assert!(
            admitted < 2 * 200_000 / 256,
            "{admitted} of {strangers} strangers admitted (2^-8)"
        );
    }

    #[test]
    fn an_empty_filter_admits_nothing() {
        let empty = section(&[]);
        let read = |range: Range<usize>| &empty[range];

        let layout = FilterLayout::parse(read, empty.len()).expect("empty parses");

        assert!(!layout.may_contain(read, &key(1)), "key 1 admitted");
    }

    /// Filter of 100 keys, its section cut by a byte or one byte longer
    #[test]
    fn a_filter_section_of_the_wrong_length_is_malformed() {
        let section = section(&(0..100).map(key).collect::<Vec<_>>());
        let longer = [&section[..], &[0]].concat();

        let short = FilterLayout::parse(|range| &section[range], section.len() - 1);
        let trailing = FilterLayout::parse(|range| &longer[range], longer.len());

        assert!(short.is_none(), "a byte short parsed: {short:?}");
        assert!(trailing.is_none(), "a trailing byte parsed: {trailing:?}");
    }
}
