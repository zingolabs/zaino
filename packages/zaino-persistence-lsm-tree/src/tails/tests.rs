use std::path::Path;

use super::*;
use crate::fs::SimFs;

const DIR: &str = "/idx";

fn names() -> Vec<String> {
    [
        "blocks.dat",
        "blocks.dat.crc",
        "pool/nodes.dat",
        "pool/nodes.dat.crc",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn tail(offset: u64, byte: u8, len: usize) -> Tail {
    Tail {
        offset,
        bytes: vec![byte; len],
    }
}

/// A file of `pages` full pages of `0x11` then `tail` bytes of `0x22`
fn log_file(fs: &SimFs, name: &str, pages: usize, tail: usize) {
    let path = Path::new(DIR).join(name);
    fs.create_dir_all(path.parent().expect("parent"))
        .expect("dir");
    let file = fs.open(&path).expect("file");
    let mut bytes = vec![0x11; pages * PAGE];
    bytes.extend(vec![0x22; tail]);
    file.write_all_at(&bytes, 0).expect("write");
    file.sync_data().expect("sync");
}

/// A commit's saved tails restore exactly the bytes a crash garbled, and only those files
#[test]
fn a_garbled_tail_page_is_restored_from_its_commits_slot() {
    let fs = SimFs::new();
    log_file(&fs, "blocks.dat", 2, 100);
    log_file(&fs, "pool/nodes.dat", 0, 7);
    let tails = Tails::open(fs.as_ref(), Path::new(DIR), names()).expect("open");
    let saved = vec![
        ("blocks.dat".to_owned(), tail(2 * PAGE as u64, 0x22, 100)),
        ("pool/nodes.dat".to_owned(), tail(0, 0x22, 7)),
    ];
    tails.write(5, &saved).expect("write");

    let blocks = Path::new(DIR).join("blocks.dat");
    fs.corrupt(&blocks, |bytes| bytes[2 * PAGE + 10] ^= 0xff);
    assert_eq!(
        tails
            .restore(fs.as_ref(), Path::new(DIR), 5)
            .expect("restore"),
        1
    );
    let mut expected = vec![0x11; 2 * PAGE];
    expected.extend(vec![0x22; 100]);
    assert_eq!(fs.contents(&blocks).expect("blocks"), expected, "restored");
    assert_eq!(
        tails
            .restore(fs.as_ref(), Path::new(DIR), 5)
            .expect("restore"),
        0,
        "intact files are left alone"
    );
}

/// A slot of another commit, or torn, restores nothing: the files' own checks then decide
#[test]
fn another_commits_slot_or_a_torn_one_restores_nothing() {
    let fs = SimFs::new();
    log_file(&fs, "blocks.dat", 0, 9);
    let tails = Tails::open(fs.as_ref(), Path::new(DIR), names()).expect("open");
    tails
        .write(4, &[("blocks.dat".to_owned(), tail(0, 0x33, 9))])
        .expect("write");
    assert_eq!(
        tails
            .restore(fs.as_ref(), Path::new(DIR), 6)
            .expect("restore"),
        0,
        "slot 0 holds commit 4, not 6"
    );
    let path = Path::new(DIR).join(TAILS);
    fs.corrupt(&path, |bytes| bytes[HEADER + 3] ^= 1);
    assert_eq!(
        tails
            .restore(fs.as_ref(), Path::new(DIR), 4)
            .expect("restore"),
        0,
        "a torn slot"
    );
    assert_eq!(
        fs.contents(&Path::new(DIR).join("blocks.dat"))
            .expect("blocks"),
        vec![0x22; 9],
        "nothing written"
    );
}

/// Only declared log files within their last page are ever written
#[test]
fn only_declared_files_within_their_last_page_are_saved_or_restored() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).expect("dir");
    let tails = Tails::open(fs.as_ref(), Path::new(DIR), names()).expect("open");
    let refused = tails.write(1, &[("MANIFEST".to_owned(), tail(0, 0, 1))]);
    assert!(
        matches!(refused, Err(StoreError::Internal(_))),
        "{refused:?}"
    );
    let refused = tails.write(1, &[("blocks.dat".to_owned(), tail(0, 0, PAGE))]);
    assert!(
        matches!(refused, Err(StoreError::Internal(_))),
        "{refused:?}"
    );

    // a slot naming an undeclared file (another index's TAILS) is not used
    let other = Tails::open(fs.as_ref(), Path::new(DIR), ["other.dat".to_owned()]).expect("open");
    log_file(&fs, "other.dat", 0, 3);
    other
        .write(2, &[("other.dat".to_owned(), tail(0, 0x44, 3))])
        .expect("write");
    let reopened = Tails::open(fs.as_ref(), Path::new(DIR), names()).expect("open");
    assert_eq!(
        reopened
            .restore(fs.as_ref(), Path::new(DIR), 2)
            .expect("restore"),
        0
    );
    assert_eq!(
        fs.contents(&Path::new(DIR).join("other.dat"))
            .expect("other"),
        vec![0x22; 3]
    );
}

/// Sized for its files: written in place, recreated (zeroed, synced) when sized for others
#[test]
fn the_file_is_sized_for_its_log_files_and_written_in_place() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(DIR)).expect("dir");
    let path = Path::new(DIR).join(TAILS);
    let tails = Tails::open(fs.as_ref(), Path::new(DIR), names()).expect("open");
    let len = fs.contents(&path).expect("tails").len() as u64;
    assert_eq!(len, 2 * tails.slot_len);
    assert!(tails.slot_len.is_multiple_of(PAGE as u64));
    for seq in 1..=3 {
        tails.write(seq, &[]).expect("write");
        assert_eq!(
            fs.contents(&path).expect("tails").len() as u64,
            len,
            "in place"
        );
    }
    let fewer = Tails::open(fs.as_ref(), Path::new(DIR), ["blocks.dat".to_owned()]).expect("open");
    assert!(fewer.slot_len < tails.slot_len);
    assert_eq!(
        fs.contents(&path).expect("tails"),
        vec![0; 2 * fewer.slot_len as usize],
        "recreated zeroed"
    );
}
