//! End-to-end coverage for `#[derive(PersistentRecord)]`.
//!
//! The production DTOs in `zaino-indexes` already pin most atoms byte-for-byte
//! through their golden tests. This suite exercises the derive over *every*
//! supported field type in one record — including `Vec<u8>` (length-framed) and
//! a non-32 byte array, which no derivable production DTO happens to use — and
//! covers the tuple-struct form.

use zaino_persistence_codec::{PersistentRecord, RecordLayout};

/// One field of each supported kind, in a fixed order so the expected bytes are
/// a simple concatenation.
#[derive(PersistentRecord, Debug, PartialEq, Eq)]
struct AllAtoms {
    byte: u8,
    le32: u32,
    #[persistent(be)]
    be32: u32,
    le64: u64,
    #[persistent(be)]
    be64: u64,
    hash: [u8; 32],
    short: [u8; 4],
    blob: Vec<u8>,
}

/// The tuple-struct form: a single little-endian `u64`.
#[derive(PersistentRecord, Debug, PartialEq, Eq)]
struct Single(u64);

#[test]
fn every_atom_encodes_to_its_pinned_layout() {
    let record = AllAtoms {
        byte: 0x11,
        le32: 0x2233_4455,
        be32: 0x2233_4455,
        le64: 0x0102_0304_0506_0708,
        be64: 0x0102_0304_0506_0708,
        hash: [0xAB; 32],
        short: [0xCD; 4],
        blob: vec![1, 2, 3],
    };

    let mut expected = Vec::new();
    expected.push(0x11u8); // byte
    expected.extend_from_slice(&0x2233_4455u32.to_le_bytes()); // le32
    expected.extend_from_slice(&0x2233_4455u32.to_be_bytes()); // be32
    expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes()); // le64
    expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_be_bytes()); // be64
    expected.extend_from_slice(&[0xAB; 32]); // hash
    expected.extend_from_slice(&[0xCD; 4]); // short
    expected.extend_from_slice(&3u32.to_le_bytes()); // blob length prefix
    expected.extend_from_slice(&[1, 2, 3]); // blob bytes

    assert_eq!(record.encode(), expected);

    let back = AllAtoms::decode(&record.encode()).expect("decode");
    assert_eq!(back, record);
}

#[test]
fn a_tuple_struct_round_trips() {
    let record = Single(0x0a0b_0c0d_0e0f_1011);
    assert_eq!(
        record.encode(),
        0x0a0b_0c0d_0e0f_1011u64.to_le_bytes().to_vec()
    );
    assert_eq!(Single::decode(&record.encode()).expect("decode"), record);
}

#[test]
fn a_short_buffer_is_rejected() {
    // A `Single` needs 8 bytes; anything shorter must fail rather than panic.
    assert!(Single::decode(&[0u8; 7]).is_err());
}

#[test]
fn a_trailing_tail_is_rejected() {
    let mut bytes = Single(1).encode();
    bytes.push(0xFF);
    assert!(Single::decode(&bytes).is_err());
}
