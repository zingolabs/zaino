//! Per-segment membership filter for probed sets: sharded binary fuse, 8-bit fingerprints
//!
//! - BinaryFuse8: ≈9 bits/key, FPR 2^-8; sizing in `docs/index-data-structures.md`
//! - shard = top bits of the key's first 8 bytes (zero-padded when shorter) → monotone in key
//!   order (built while streaming) and ≤ [`SHARD_KEYS`] per build when those bytes are uniform
//!   (bounded scratch, whatever the segment's size)

use std::{io, ops::Range};

use xorf::{BinaryFuse8, BinaryFuse8Ref, DmaSerializable, Filter, FilterRef};

use super::spill::Spill;

/// Target keys per shard (xorf build scratch ≈ 25 B/key → ≈ 26 MiB per shard)
pub(crate) const SHARD_KEYS: u64 = 1 << 20;

/// Serialized `Descriptor` (seed u64, three u32)
const DESCRIPTOR: usize = BinaryFuse8::DESCRIPTOR_LEN;

/// Per-shard table entry: descriptor ‖ fingerprint count u32 LE
const ENTRY: usize = DESCRIPTOR + 4;

/// Shard bits past which a table cannot be addressed (2^63 shards)
const MAX_BITS: u8 = 63;

/// Filter input for one encoded key (distinct keys → distinct inputs w.h.p.; dups deduped per
/// shard)
///
/// - first word = key's own uniform prefix; later words folded through splitmix64's finalizer
pub(crate) fn probe_hash(key: &[u8]) -> u64 {
    let (first, rest) = key.split_at(key.len().min(8));
    let mut hash = word(first);
    for chunk in rest.chunks(8) {
        hash = mix(hash ^ word(chunk));
    }
    hash
}

/// Big-endian, zero-padded on the right (first 8 bytes of `bytes`)
fn word(bytes: &[u8]) -> u64 {
    let mut padded = [0u8; 8];
    let len = bytes.len().min(8);
    if let (Some(to), Some(from)) = (padded.get_mut(..len), bytes.get(..len)) {
        to.copy_from_slice(from);
    }
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
    shards
        .checked_next_power_of_two()
        .map_or(MAX_BITS, |shards| shards.trailing_zeros() as u8)
        .min(MAX_BITS)
}

/// Key's shard: top `bits` of its first 8 bytes (zero-padded: keys shorter than 8 bytes shard
/// too)
fn shard(key: &[u8], bits: u8) -> usize {
    let shift = 64u32.saturating_sub(u32::from(bits));
    match word(key).checked_shr(shift) {
        Some(top) => usize::try_from(top).unwrap_or(usize::MAX),
        None => 0,
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
        if at < self.current {
            return Err(FilterError::Build("filter keys out of key order"));
        }
        while self.current < at {
            self.close()?;
        }
        self.hashes.push(probe_hash(key));
        Ok(())
    }

    /// Section head `shard bits u8 ‖ 2^bits × (descriptor ‖ count u32 LE)` + the fingerprints
    /// after it
    pub(crate) fn finish(mut self) -> Result<(Vec<u8>, S), FilterError> {
        let shards = 1usize
            .checked_shl(u32::from(self.bits))
            .ok_or(FilterError::Build("shard count past usize"))?;
        while self.current < shards {
            self.close()?;
        }
        let mut head = Vec::with_capacity(self.table.len().saturating_add(1));
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
            let (descriptor, count_at) = entry.split_at_mut(DESCRIPTOR);
            built.dma_copy_descriptor_to(descriptor);
            let fingerprints = built.dma_fingerprints();
            let count = u32::try_from(fingerprints.len())
                .map_err(|_| FilterError::Build("shard fingerprints past 2^32"))?;
            count_at.copy_from_slice(&count.to_le_bytes());
            self.fingerprints.put(fingerprints)?;
            self.hashes.clear();
        }
        self.table.extend_from_slice(&entry);
        self.current = self.current.saturating_add(1);
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
    /// `read` = the section's bytes by range (only the table is read); `Ok(None)` = malformed
    pub(crate) fn parse<'a, E>(
        read: impl Fn(Range<usize>) -> Result<&'a [u8], E>,
        len: usize,
    ) -> Result<Option<Self>, E> {
        if len == 0 {
            return Ok(None);
        }
        let Some(&bits) = read(0..1)?.first() else {
            return Ok(None);
        };
        if bits >= 32 {
            return Ok(None);
        }
        let count = 1usize << bits;
        let Some(table_end) = count
            .checked_mul(ENTRY)
            .and_then(|table| table.checked_add(1))
            .filter(|end| *end <= len)
        else {
            return Ok(None);
        };
        let table = read(1..table_end)?;
        let mut at = table_end;
        let mut shard_ranges = Vec::with_capacity(count);
        for (index, entry) in table.chunks_exact(ENTRY).enumerate() {
            let parsed = (|| {
                let count = entry.get(DESCRIPTOR..)?.try_into().ok()?;
                let shard_len = usize::try_from(u32::from_le_bytes(count)).ok()?;
                let descriptor = index.checked_mul(ENTRY)?.checked_add(1)?;
                let end = at.checked_add(shard_len)?;
                Some((
                    [descriptor..descriptor.checked_add(DESCRIPTOR)?, at..end],
                    end,
                ))
            })();
            let Some((ranges, end)) = parsed else {
                return Ok(None);
            };
            shard_ranges.push(ranges);
            at = end;
        }
        Ok((at == len).then_some(Self {
            bits,
            len,
            shard_ranges,
        }))
    }

    /// Whole section's length
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// `false` = certainly absent; `true` = present or a false positive (≈ 2^-8)
    ///
    /// - a shard past the table answers `true` (never a false negative)
    pub(crate) fn may_contain<'a, E>(
        &self,
        read: impl Fn(Range<usize>) -> Result<&'a [u8], E>,
        key: &[u8],
    ) -> Result<bool, E> {
        let Some([descriptor, fingerprints]) = self.shard_ranges.get(shard(key, self.bits)) else {
            return Ok(true);
        };
        if fingerprints.is_empty() {
            return Ok(false);
        }
        let filter =
            BinaryFuse8Ref::from_dma(read(descriptor.clone())?, read(fingerprints.clone())?);
        Ok(filter.contains(&probe_hash(key)))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    /// - golden inputs (persisted filter readable only while these stay byte-identical)
    /// - sharding monotone in key order; every written key found, strangers mostly refused
    #[test]
    fn hashes_are_stable_shards_follow_key_order_and_written_keys_are_found() {
        assert_eq!(probe_hash(&[0x01; 8]), 0x0101_0101_0101_0101);
        assert_eq!(probe_hash(&[0x01, 0x02]), 0x0102_0000_0000_0000);
        assert_eq!(probe_hash(&[0u8; 36]), 0);
        assert_eq!(probe_hash(&[0xab; 36]), 0xad4a_2fdc_9f89_727d);
        let outpoint =
            |vout: u32| probe_hash(&[[0x11; 32].as_slice(), &vout.to_be_bytes()].concat());
        assert_ne!(
            outpoint(0),
            outpoint(1),
            "outpoints of one transaction stay distinct"
        );

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
        let read = |range: Range<usize>| Ok::<_, Infallible>(&section[range]);
        let layout = FilterLayout::parse(read, section.len())
            .expect("read")
            .expect("parses");
        assert_eq!(layout.bits, 1);
        assert!(keys
            .iter()
            .all(|key| layout.may_contain(read, key).expect("read")));

        let strangers = 200_000u64;
        let false_positives = (0..strangers)
            .map(|n| key((n + 1) << 40))
            .filter(|key| layout.may_contain(read, key).expect("read"))
            .count();
        let rate_ok = (false_positives as f64) < strangers as f64 * 2.0 / 256.0;
        assert!(rate_ok, "{false_positives} false positives in {strangers}");

        let (head, fingerprints) = FilterWriter::new(0, Vec::new()).finish().expect("empty");
        let empty = [head, fingerprints].concat();
        let read_empty = |range: Range<usize>| Ok::<_, Infallible>(&empty[range]);
        let layout = FilterLayout::parse(read_empty, empty.len())
            .expect("read")
            .expect("empty");
        assert!(!layout.may_contain(read_empty, &key(1)).expect("read"));
        let short = FilterLayout::parse(read, section.len() - 1).expect("read");
        assert!(short.is_none(), "short");
        let longer = [&section[..], &[0]].concat();
        let read_longer = |range: Range<usize>| Ok::<_, Infallible>(&longer[range]);
        let trailing = FilterLayout::parse(read_longer, longer.len()).expect("read");
        assert!(trailing.is_none(), "trailing");
    }

    /// Keys under 8 bytes shard on their zero-padded prefix: in key order, found again
    #[test]
    fn short_keys_shard_in_key_order_and_are_found() {
        let keys: Vec<Vec<u8>> = (0u32..300_000).map(|n| n.to_be_bytes().to_vec()).collect();
        let bits = shard_bits(SHARD_KEYS * 4);
        let shards: Vec<usize> = keys.iter().map(|key| shard(key, bits)).collect();
        assert!(shards.is_sorted(), "monotone in key order");

        let mut writer = FilterWriter::new(SHARD_KEYS * 4, Vec::new());
        for key in &keys {
            writer.push(key).expect("push");
        }
        let (head, fingerprints) = writer.finish().expect("build");
        let section = [head, fingerprints].concat();
        let read = |range: Range<usize>| Ok::<_, Infallible>(&section[range]);
        let layout = FilterLayout::parse(read, section.len())
            .expect("read")
            .expect("parses");
        assert!(keys
            .iter()
            .all(|key| layout.may_contain(read, key).expect("read")));
        assert_eq!(shard(&[], bits), 0, "an empty key = the first shard");
    }
}
