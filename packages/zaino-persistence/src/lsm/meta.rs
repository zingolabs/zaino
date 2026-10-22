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
    use crate::golden::assert_fields;

    fn segment(id: u32, records: u64, tombstones: u64) -> SegmentMeta {
        SegmentMeta { id, records, tombstones, sealed: Sealed { len: 100, tail: 0xaa, sums: 0xbb } }
    }

    #[test]
    fn a_segment_list_is_its_golden_bytes() {
        let segments = [segment(4, 1 << 40, 3), segment(9, 1, 0)];
        let mut bytes = Vec::new();

        encode_list(&segments, &mut bytes);

        #[rustfmt::skip]
        assert_fields(&bytes, &[
            ("count",                &2u32.to_le_bytes()),
            ("segment 0 id",         &4u32.to_le_bytes()),
            ("segment 0 records",    &(1u64 << 40).to_le_bytes()),
            ("segment 0 tombstones", &3u64.to_le_bytes()),
            ("segment 0 len",        &100u64.to_le_bytes()),
            ("segment 0 tail",       &0xaau32.to_le_bytes()),
            ("segment 0 sums",       &0xbbu32.to_le_bytes()),
            ("segment 1 id",         &9u32.to_le_bytes()),
            ("segment 1 records",    &1u64.to_le_bytes()),
            ("segment 1 tombstones", &0u64.to_le_bytes()),
            ("segment 1 len",        &100u64.to_le_bytes()),
            ("segment 1 tail",       &0xaau32.to_le_bytes()),
            ("segment 1 sums",       &0xbbu32.to_le_bytes()),
        ]);
        let mut body = BodyReader::new(&bytes);
        assert_eq!(decode_list(&mut body).expect("golden bytes decode"), segments);
        body.finish().expect("whole list consumed");
    }

    /// None ever committed: a manifest listing one = malformed
    #[test]
    fn a_segment_list_refuses_a_duplicate_id_an_empty_segment_and_excess_tombstones() {
        #[rustfmt::skip]
        let cases = [
            ([segment(1, 5, 0), segment(1, 6, 0)].to_vec(), "segment listed twice"),
            ([segment(1, 0, 0)].to_vec(),                    "empty segment listed"),
            ([segment(1, 5, 6)].to_vec(),                    "more tombstones than records"),
        ];
        for (listed, refused) in cases {
            let mut bytes = Vec::new();
            encode_list(&listed, &mut bytes);

            let error = decode_list(&mut BodyReader::new(&bytes)).expect_err(refused);

            assert_eq!(
                error.to_string(),
                format!("malformed manifest body: {refused}"),
                "{listed:?}"
            );
        }
    }
}
