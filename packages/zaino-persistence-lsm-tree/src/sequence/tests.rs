//! Positional files: records by position across seals, reopen at a seal, the manifest's
//! whole-record check

use std::{num::NonZeroU32, path::Path};

use super::*;
use crate::fs::SimFs;

fn width(n: u32) -> Width {
    Width::Fixed(NonZeroU32::new(n).expect("non-zero"))
}

/// Records appended over two seals read back by position; past the end = `None`; the second
/// view keeps the first's checked pages
#[test]
fn records_read_back_by_position_across_seals() {
    for (shape, record) in [
        (width(4), (|n: u8| vec![n; 4]) as fn(u8) -> Vec<u8>),
        (Width::Variable, |n: u8| vec![n; usize::from(n % 5)]),
    ] {
        let fs = SimFs::new();
        let dir = Path::new("/seq");
        fs.create_dir_all(dir).expect("dir");
        let mut file =
            SequenceFile::open(fs.as_ref(), dir, "s", shape, false, Seals::EMPTY).expect("open");
        for n in 0..10 {
            file.append(&record(n)).expect("append");
        }
        file.seal().expect("seal");
        let first = file.pages(None).expect("pages");
        for n in 10..20 {
            file.append(&record(n)).expect("append");
        }
        file.seal().expect("seal");
        let second = file.pages(Some(&first)).expect("pages");

        assert_eq!((first.len(), second.len()), (10, 20), "{shape:?}");
        for n in 0..20u8 {
            let read = second.record(u64::from(n)).expect("read");
            assert_eq!(read, Some(record(n).as_slice()), "{shape:?} record {n}");
        }
        assert_eq!(second.record(20).expect("read"), None, "past the end");
        second.will_need(0..20, 1, true).expect("advisory");
        second
            .will_need(30..40, 1, false)
            .expect("past the end: nothing");
    }
}

/// A fixed sequence refuses a record off its width (the rules check first: a bug here)
#[test]
fn a_record_off_its_width_is_refused() {
    let fs = SimFs::new();
    let dir = Path::new("/seq");
    fs.create_dir_all(dir).expect("dir");
    let mut file =
        SequenceFile::open(fs.as_ref(), dir, "s", width(4), false, Seals::EMPTY).expect("open");
    let refused = file.append(&[0; 3]);
    assert!(
        matches!(refused, Err(StoreError::Internal(_))),
        "{refused:?}"
    );
}

/// Reopen at an earlier seal: bytes past it dropped, records up to it intact
#[test]
fn reopen_at_a_seal_drops_what_followed() {
    let fs = SimFs::new();
    let dir = Path::new("/seq");
    fs.create_dir_all(dir).expect("dir");
    let mut file = SequenceFile::open(fs.as_ref(), dir, "s", Width::Variable, false, Seals::EMPTY)
        .expect("open");
    file.append(b"one").expect("append");
    let sealed = file.seal().expect("seal");
    file.append(b"two").expect("append");
    file.seal().expect("seal");
    drop(file);

    let file =
        SequenceFile::open(fs.as_ref(), dir, "s", Width::Variable, false, sealed).expect("reopen");
    let pages = file.pages(None).expect("pages");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages.record(0).expect("read"), Some(&b"one"[..]));
    assert_eq!(
        fs.contents(&dir.join("s.dat")).expect("data").len(),
        3,
        "tail dropped"
    );
}

/// Seals decode only as whole records of their width
#[test]
fn seals_decode_only_as_whole_records() {
    let seals = |len: u64| Seals {
        data: Sealed {
            len,
            tail: 0,
            sums: 0,
        },
        ends: Sealed::EMPTY,
    };
    for (len, whole) in [(8, true), (6, false)] {
        let mut bytes = Vec::new();
        seals(len).encode(width(4), &mut bytes);
        let decoded = Seals::decode(width(4), &mut BodyReader::new(&bytes));
        assert_eq!(decoded.is_ok(), whole, "{len} bytes of 4-byte records");
    }
}
