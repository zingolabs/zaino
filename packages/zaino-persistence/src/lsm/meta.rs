//! Durable segment as its manifest records it ([`SegmentMeta`]) + segment-list codec

use crate::{
    manifest::{BodyReader, ManifestError},
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

    fn decode(body: &mut BodyReader<'_>) -> Result<Self, ManifestError> {
        let (id, records, tombstones) = (body.u32()?, body.u64()?, body.u64()?);
        Ok(Self { id, records, tombstones, sealed: Sealed::decode(body)? })
    }

    /// Keys this segment adds to the map (each tombstone cancels one older value: insert-once,
    /// remove-once)
    pub(crate) fn live(&self) -> i64 {
        let (records, tombstones) = (self.records as i64, self.tombstones as i64);
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

/// Duplicate id or empty segment refused (neither ever committed)
pub(crate) fn decode_list(body: &mut BodyReader<'_>) -> Result<Vec<SegmentMeta>, ManifestError> {
    let count = body.u32()?;
    let mut segments = Vec::new();
    for _ in 0..count {
        let segment = SegmentMeta::decode(body)?;
        if segment.records == 0 {
            return Err(ManifestError::Body("empty segment listed"));
        }
        if segment.tombstones > segment.records {
            return Err(ManifestError::Body("more tombstones than records"));
        }
        if segments.iter().any(|seen: &SegmentMeta| seen.id == segment.id) {
            return Err(ManifestError::Body("segment listed twice"));
        }
        segments.push(segment);
    }
    Ok(segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(id: u32, records: u64) -> SegmentMeta {
        SegmentMeta {
            id,
            records,
            tombstones: records / 4,
            sealed: Sealed { len: records * 3, tail: id * 7, sums: id * 11 },
        }
    }

    /// - list codec round trip, tombstones included
    /// - duplicate id, empty segment or more tombstones than records in a manifest refused
    #[test]
    fn segment_lists_round_trip_and_refuse_duplicates_and_empties() {
        let segments = vec![segment(4, 10), segment(9, 1), segment(2, 1 << 40)];
        let mut bytes = Vec::new();
        encode_list(&segments, &mut bytes);
        assert_eq!(bytes.len(), 4 + 3 * 36);
        let mut body = BodyReader::new(&bytes);
        assert_eq!(decode_list(&mut body).expect("decode"), segments);
        body.finish().expect("whole list consumed");

        let over = SegmentMeta { tombstones: 6, ..segment(1, 5) };
        for (bad, reason) in [
            (vec![segment(1, 5), segment(1, 6)], "segment listed twice"),
            (vec![segment(1, 0)], "empty segment listed"),
            (vec![over], "more tombstones than records"),
        ] {
            let mut bytes = Vec::new();
            encode_list(&bad, &mut bytes);
            let decoded = decode_list(&mut BodyReader::new(&bytes));
            assert!(matches!(decoded, Err(ManifestError::Body(r)) if r == reason), "{reason}");
        }
    }
}
