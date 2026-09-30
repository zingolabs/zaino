//! Per-segment membership filter for probed sets: sharded binary fuse, 8-bit fingerprints
//!
//! - BinaryFuse8: ≈9 bits/key, FPR 2^-8; sizing in `docs/design/index-data-structures.md` §7
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

/// Filter input for one encoded key (distinct keys → distinct inputs w.h.p.; dups deduped per shard)
///
/// - first word = the key's own uniform prefix; later words folded through splitmix64's finalizer
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
        bits => usize::try_from(word(&key[..8]) >> (64 - u32::from(bits))).expect("shard index"),
    }
}

/// Where a filter's fingerprints go as each shard closes: memory, or a segment's [`Spill`]
pub(crate) trait Fingerprints {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()>;
}

impl Fingerprints for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}

impl Fingerprints for Spill {
    fn put(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.push(bytes)
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
/// - only the shard being built and the table stay in memory; each closed shard's fingerprints
///   go straight to `S`
pub(crate) struct FilterWriter<S> {
    bits: u8,
    current: usize,
    hashes: Vec<u64>,
    table: Vec<u8>,
    fingerprints: S,
}

impl<S: Fingerprints> FilterWriter<S> {
    pub(crate) fn new(records: u64, fingerprints: S) -> Self {
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

    /// The section's head, `shard bits u8 ‖ 2^bits × (descriptor ‖ count u32 LE)`, and the
    /// fingerprints that follow it
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, S), FilterError> {
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
            self.fingerprints.put(fingerprints)?;
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
    /// `read` = the section's bytes by range (only the table is read); `None` = malformed
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

    /// The whole section's length
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// `false` = certainly absent; `true` = present or a false positive (≈ 2^-8)
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
    use super::*;

    /// Golden inputs (a persisted filter is only readable while these stay byte-identical);
    /// sharding monotone in key order; every written key found, strangers mostly refused
    #[test]
    fn hashes_are_stable_shards_follow_key_order_and_written_keys_are_found() {
        assert_eq!(probe_hash(&[0x01; 8]), 0x0101_0101_0101_0101);
        assert_eq!(probe_hash(&[0x01, 0x02]), 0x0102_0000_0000_0000);
        assert_eq!(probe_hash(&[0u8; 36]), 0);
        assert_eq!(probe_hash(&[0xab; 36]), 0xad4a_2fdc_9f89_727d);
        let outpoint =
            |vout: u32| probe_hash(&[[0x11; 32].as_slice(), &vout.to_be_bytes()].concat());
        assert_ne!(outpoint(0), outpoint(1), "outpoints of one transaction stay distinct");

        let bits = [0, 1, SHARD_KEYS, SHARD_KEYS + 1, 5 * SHARD_KEYS].map(shard_bits);
        assert_eq!(bits, [0, 0, 0, 1, 3]);

        let key = |n: u64| -> Vec<u8> {
            let mut key = mix(n).to_be_bytes().to_vec();
            key.extend_from_slice(&n.to_be_bytes());
            key
        };
        let mut keys: Vec<Vec<u8>> = (0..3 * SHARD_KEYS / 2).map(key).collect();
        keys.sort_unstable();
        let mut writer = FilterWriter::new(keys.len() as u64, Vec::new());
        for key in &keys {
            writer.push(key).expect("push");
        }
        let (head, fingerprints) = writer.finish().expect("build");
        let section = [head, fingerprints].concat();
        let read = |range: Range<usize>| &section[range];
        let layout = FilterLayout::parse(read, section.len()).expect("parses");
        assert_eq!(layout.bits, 1);
        assert!(keys.iter().all(|key| layout.may_contain(read, key)));

        let strangers = 200_000u64;
        let false_positives = (0..strangers)
            .map(|n| key((n + 1) << 40))
            .filter(|key| layout.may_contain(read, key))
            .count();
        let rate_ok = (false_positives as f64) < strangers as f64 * 2.0 / 256.0;
        assert!(rate_ok, "{false_positives} false positives in {strangers}");

        let (head, fingerprints) = FilterWriter::new(0, Vec::new()).finish().expect("empty");
        let empty = [head, fingerprints].concat();
        let layout = FilterLayout::parse(|range| &empty[range], empty.len()).expect("empty");
        assert!(!layout.may_contain(|range| &empty[range], &key(1)));
        assert!(FilterLayout::parse(read, section.len() - 1).is_none(), "short");
        let longer = [&section[..], &[0]].concat();
        assert!(FilterLayout::parse(|range| &longer[range], longer.len()).is_none(), "trailing");
    }
}
