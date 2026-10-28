use super::*;

fn identity(name: &str, format: u16) -> Identity {
    Identity::new(name, format).expect("short name")
}

/// Names up to 255 bytes only (one length byte in the header)
#[test]
fn an_identity_name_fits_its_length_byte() {
    assert!(Identity::new(&"n".repeat(255), 1).is_ok());
    let long = Identity::new(&"n".repeat(256), 1);
    assert!(matches!(long, Err(ManifestError::NameLength { len: 256 })));
}

/// - slot pinned byte for byte, alternating by `seq`
/// - committed state read off every slot pair a crash, foreign directory or older layout leaves
#[test]
fn manifest_slots_golden_bytes_and_which_commit_every_slot_pair_holds() {
    let identity = identity("tree", 3);
    let encoded = encode(&identity, 7, &[0xaa, 0xbb]).expect("fits");
    let mut expected = b"ZAINOMS4".to_vec();
    expected.extend_from_slice(&[3, 0, 4]);
    expected.extend_from_slice(b"tree");
    expected.extend_from_slice(&7u64.to_le_bytes());
    expected.extend_from_slice(&[2, 0, 0, 0, 0xaa, 0xbb]);
    expected.extend_from_slice(&crc32fast::hash(&expected).to_le_bytes());
    assert_eq!(encoded, expected);
    assert_eq!(
        (slot_offset(7), slot_offset(8)),
        (SLOT as u64, 0),
        "odd → slot 1, even → 0"
    );
    let header = identity.prefix_len() + 8 + 4;

    // the whole file as each slot holds it (`None` = never written)
    let file = |first: Option<Vec<u8>>, second: Option<Vec<u8>>| {
        let mut bytes = vec![0; 2 * SLOT];
        for (at, slot) in [(0, first), (SLOT, second)] {
            if let Some(slot) = slot {
                bytes[at..at + slot.len()].copy_from_slice(&slot);
            }
        }
        bytes
    };
    let commit = |seq: u64, body: &[u8]| Some(encode(&identity, seq, body).expect("fits"));
    let torn = |seq: u64, body: &[u8]| {
        let mut slot = encode(&identity, seq, body).expect("fits");
        slot[header] ^= 1;
        Some(slot)
    };
    // body_len past the slot's end: torn on its length, before any CRC read
    let mut overlong = encode(&identity, 3, &[1]).expect("fits");
    overlong[header - 4..header].copy_from_slice(&(SLOT as u32).to_le_bytes());

    type Commit = Option<(u64, Vec<u8>)>;
    let cases: [(&str, Vec<u8>, Commit); 6] = [
        ("created, never committed", file(None, None), None),
        (
            "first commit",
            file(None, commit(1, &[1])),
            Some((1, vec![1])),
        ),
        (
            "two commits: the newer",
            file(commit(2, &[2]), commit(1, &[1])),
            Some((2, vec![2])),
        ),
        (
            "newer torn: the older",
            file(torn(4, &[4]), commit(3, &[3])),
            Some((3, vec![3])),
        ),
        ("first commit torn", file(None, torn(1, &[1])), None),
        (
            "overlong body = torn",
            file(None, Some(overlong.clone())),
            None,
        ),
    ];
    for (why, bytes, want) in cases {
        let found = latest(&identity, &bytes).unwrap_or_else(|error| panic!("{why}: {error}"));
        assert_eq!(found.map(|(seq, body)| (seq, body.to_vec())), want, "{why}");
    }

    use ManifestError::{BodyLength, Checksum, Format, Layout, Magic, Name, Sequence};
    let (torn_pair, twin_pair) = (
        file(torn(2, &[2]), torn(1, &[1])),
        file(commit(1, &[1]), commit(1, &[2])),
    );
    let overlong_pair = file(Some(overlong.clone()), Some(overlong));
    let both_torn = latest(&identity, &torn_pair);
    let both_overlong = latest(&identity, &overlong_pair);
    let twins = latest(&identity, &twin_pair);
    let older_layout = latest(&identity, &[1; 40]);
    let empty = latest(&identity, &[]);
    let one = file(None, commit(1, &[1]));
    let name = latest(&self::identity("blocks", 3), &one);
    let format = latest(&self::identity("tree", 4), &one);
    // the previous layout (no TAILS beside it): refused, never misread
    let mut foreign_magic = encode(&identity, 1, &[1]).expect("fits");
    foreign_magic[..8].copy_from_slice(b"ZAINOMS3");
    let foreign_pair = file(Some(foreign_magic.clone()), Some(foreign_magic));
    let older_magic = latest(&identity, &foreign_pair);
    assert!(
        matches!(both_torn, Err(Checksum)),
        "a crash tears one slot, never both"
    );
    let room = SLOT - header - CRC;
    let named = matches!(both_overlong, Err(BodyLength { claimed, actual })
        if claimed as usize == SLOT && actual == room);
    assert!(
        named,
        "the claimed length beside the room the slot holds: {both_overlong:?}"
    );
    assert!(matches!(twins, Err(Sequence { seq: 1 })));
    assert!(matches!(older_layout, Err(Layout { len: 40 })));
    assert!(
        matches!(empty, Err(Layout { len: 0 })),
        "only ever created whole"
    );
    assert!(
        matches!(&name, Err(Name { expected, found }) if expected == "blocks" && found == "tree"),
        "{name:?}"
    );
    assert!(matches!(
        format,
        Err(Format {
            expected: 4,
            found: 3
        })
    ));
    assert!(
        matches!(older_magic, Err(Magic)),
        "another manifest layout's magic"
    );
}

/// A body past the slot: refused, never written
#[test]
fn a_body_past_its_slot_is_refused() {
    let identity = identity("tree", 3);
    let room = SLOT - (identity.prefix_len() + 8 + 4 + CRC);
    assert!(encode(&identity, 1, &vec![0; room]).is_ok());
    let over = encode(&identity, 1, &vec![0; room + 1]);
    assert!(
        matches!(over, Err(ManifestError::Full { body }) if body == room + 1),
        "{over:?}"
    );
}

/// - tip presence follows the count both ways; zero hash = still a tip
/// - on disk = block count from genesis (tip height + 1) ‖ hash
#[test]
fn committed_round_trips_and_refuses_a_tip_with_nothing_committed() {
    let tip = |height: u32, hash: u8| Committed {
        tip: Some(BlockRef {
            hash: [hash; 32].into(),
            height: Height::try_from(height).expect("h"),
        }),
    };
    let golden = [3u64.to_le_bytes().as_slice(), &[7; 32]].concat();
    let mut bytes = Vec::new();
    tip(2, 7).encode(&mut bytes);
    assert_eq!(bytes, golden, "tip at 2 = count 3 on disk");

    for committed in [Committed::EMPTY, tip(2, 7), tip(0, 0)] {
        let mut bytes = Vec::new();
        committed.encode(&mut bytes);
        let mut body = BodyReader::new(&bytes);
        assert_eq!(Committed::decode(&mut body).expect("decode"), committed);
        body.finish().expect("no tail");
    }

    let mut bytes = 0u64.to_le_bytes().to_vec();
    bytes.extend_from_slice(&[7; 32]);
    let tip_only = Committed::decode(&mut BodyReader::new(&bytes));
    assert!(matches!(
        tip_only,
        Err(ManifestError::Body("tip hash with nothing committed"))
    ));
    let tail = BodyReader::new(&[1]).finish();
    assert!(matches!(tail, Err(ManifestError::Body("trailing bytes"))));
}
