//! Durable segment as its manifest records it ([`SegmentMeta`]) + segment-list codec

use crate::{
    manifest::{self, BodyReader},
    pages::Sealed,
};

/// Durable segment: file id, record count (`tombstones` of them on a `deletes()` table), its
/// file's seal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SegmentMeta {
    pub(crate) id: u32,
    pub(crate) records: u64,
    pub(crate) tombstones: u64,
    pub(crate) sealed: Sealed,
}

impl SegmentMeta {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.records.to_le_bytes());
        out.extend_from_slice(&self.tombstones.to_le_bytes());
        self.sealed.encode(out);
    }

    fn decode(body: &mut BodyReader<'_>) -> Self {
        let (id, records, tombstones) = (body.u32(), body.u64(), body.u64());
        Self { id, records, tombstones, sealed: Sealed::decode(body) }
    }

    /// Keys this segment adds to the map (each tombstone cancels one older value: insert-once,
    /// remove-once)
    pub(crate) fn live(&self) -> i64 {
        let count = |n: u64| i64::try_from(n).expect("segment counts < 2^63");
        let (records, tombstones) = (count(self.records), count(self.tombstones));
        records - 2 * tombstones
    }
}

/// `count u32 ‖ (id u32 ‖ records u64 ‖ tombstones u64 ‖ sealed len u64 ‖ tail crc u32 ‖ sums
/// u32)*` (36 B each), in list order
pub(crate) fn encode_list(segments: &[SegmentMeta], out: &mut Vec<u8>) {
    out.extend_from_slice(
        &u32::try_from(segments.len()).expect("segment count < 2^32").to_le_bytes(),
    );
    for segment in segments {
        segment.encode(out);
    }
}

/// Empty segment, tombstones > records, duplicate id panic (none ever committed)
pub(crate) fn decode_list(body: &mut BodyReader<'_>) -> Vec<SegmentMeta> {
    let count = body.u32();
    let mut segments = Vec::new();
    for _ in 0..count {
        let segment = SegmentMeta::decode(body);
        if segment.records == 0 {
            manifest::malformed("lists an empty segment")
        }
        if segment.tombstones > segment.records {
            manifest::malformed("lists a segment with more tombstones than records")
        }
        if segments.iter().any(|seen: &SegmentMeta| seen.id == segment.id) {
            manifest::malformed("lists a segment twice")
        }
        segments.push(segment);
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(id: u32, records: u64, tombstones: u64) -> SegmentMeta {
        SegmentMeta { id, records, tombstones, sealed: Sealed { len: 100, tail: 0xaa, sums: 0xbb } }
    }

    #[test]
    fn a_segment_list_is_its_golden_bytes() {
        let segments = [segment(4, 1 << 40, 3), segment(9, 1, 0)];
        let mut bytes = Vec::new();

        encode_list(&segments, &mut bytes);

        #[rustfmt::skip]
        let fields: [(&str, std::ops::Range<usize>, &[u8]); 13] = [
            ("count",                0..4,   &2u32.to_le_bytes()),
            ("segment 0 id",         4..8,   &4u32.to_le_bytes()),
            ("segment 0 records",    8..16,  &(1u64 << 40).to_le_bytes()),
            ("segment 0 tombstones", 16..24, &3u64.to_le_bytes()),
            ("segment 0 len",        24..32, &100u64.to_le_bytes()),
            ("segment 0 tail",       32..36, &0xaau32.to_le_bytes()),
            ("segment 0 sums",       36..40, &0xbbu32.to_le_bytes()),
            ("segment 1 id",         40..44, &9u32.to_le_bytes()),
            ("segment 1 records",    44..52, &1u64.to_le_bytes()),
            ("segment 1 tombstones", 52..60, &0u64.to_le_bytes()),
            ("segment 1 len",        60..68, &100u64.to_le_bytes()),
            ("segment 1 tail",       68..72, &0xaau32.to_le_bytes()),
            ("segment 1 sums",       72..76, &0xbbu32.to_le_bytes()),
        ];
        for (field, range, golden) in fields {
            assert_eq!(bytes.get(range.clone()), Some(golden), "{field}, bytes {range:?}");
        }
        assert_eq!(bytes.len(), 76, "list = count + 2 × 36-byte segments, nothing after");
        let mut body = BodyReader::new(&bytes);
        assert_eq!(decode_list(&mut body), segments);
        body.finish();
    }

    #[test]
    #[should_panic(expected = "manifest body passed CRC yet lists a segment twice")]
    fn a_segment_list_holding_an_id_twice_panics() {
        let mut bytes = Vec::new();
        encode_list(&[segment(1, 5, 0), segment(1, 6, 0)], &mut bytes);
        decode_list(&mut BodyReader::new(&bytes));
    }

    #[test]
    #[should_panic(expected = "manifest body passed CRC yet lists an empty segment")]
    fn a_segment_list_holding_an_empty_segment_panics() {
        let mut bytes = Vec::new();
        encode_list(&[segment(1, 0, 0)], &mut bytes);
        decode_list(&mut BodyReader::new(&bytes));
    }

    #[test]
    #[should_panic(
        expected = "manifest body passed CRC yet lists a segment with more tombstones than records"
    )]
    fn a_segment_list_holding_excess_tombstones_panics() {
        let mut bytes = Vec::new();
        encode_list(&[segment(1, 5, 6)], &mut bytes);
        decode_list(&mut BodyReader::new(&bytes));
    }
}
