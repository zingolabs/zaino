use std::num::NonZeroU32;

use super::*;
use crate::{fs::SimFs, lsm::Tuning, testing::SPILL};

fn shape() -> Shape {
    let width = |n| NonZeroU32::new(n).expect("non-zero");
    Shape::of("ids", width(16), width(4), false, Tuning::default(), SPILL).expect("shape")
}

/// A sealed segment and the digest of the rows written into it
fn written(fs: &Arc<SimFs>) -> (SegmentWriter, SegmentMeta, u32) {
    let dir = Path::new("/ids");
    fs.create_dir_all(dir).expect("dir");
    let writer = SegmentWriter::open(fs.clone(), dir, shape());
    let rows: Vec<(Vec<u8>, Vec<u8>)> = (0..100u32)
        .map(|n| {
            (
                [&[0; 12][..], &n.to_be_bytes()].concat(),
                n.to_le_bytes().to_vec(),
            )
        })
        .collect();
    let mut digest = crc32fast::Hasher::new();
    let mut row = Vec::new();
    for (key, value) in &rows {
        row.clear();
        encode_row(&writer.shape, key, Row::Value(value), &mut row).expect("row");
        digest.update(&row);
    }
    let borrowed = rows
        .iter()
        .map(|(key, value)| (key.as_slice(), Some(value.as_slice())))
        .collect();
    let written = writer.write(1, borrowed).expect("write").expect("rows");
    (writer, written.meta, digest.finalize())
}

/// The read-back proves what was written: as written = verified; a record count off the file =
/// its layout refused at open; a row digest off what was written = `SelfCheck`
#[test]
fn a_read_back_off_what_was_written_is_refused() {
    let fs = SimFs::new();
    let (writer, meta, digest) = written(&fs);
    assert!(writer.verify(&meta, digest, None).expect("as written"));

    let miscounted = SegmentMeta {
        records: meta.records + 1,
        ..meta
    };
    let refused = writer.verify(&miscounted, digest, None);
    assert!(
        matches!(&refused, Err(SegmentError::Corrupt { segment: 1, .. })),
        "{refused:?}"
    );
    let refused = writer.verify(&meta, digest ^ 1, None);
    assert!(
        matches!(&refused, Err(SegmentError::SelfCheck { segment: 1, reason }) if reason.contains("differ")),
        "{refused:?}"
    );
}
