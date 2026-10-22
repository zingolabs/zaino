use super::*;
use crate::fs::SimFs;

/// - appends across page boundaries seal to a from-scratch hash's CRCs
/// - reopen continues the tail
/// - flipped committed byte = `Corrupt` on first touch + scrub fault
#[test]
fn seals_reopen_and_every_committed_byte_is_checked() {
    let fs = SimFs::new();
    let path = Path::new("/p/data");
    fs.create_dir_all(Path::new("/p")).expect("dir");
    let bytes: Vec<u8> = (0..3 * PAGE + 100).map(|n| (n * 7 % 251) as u8).collect();

    let mut file =
        PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
    file.append(&bytes[..PAGE - 3]).expect("append");
    let first = file.seal().expect("seal");
    let first_tail = page_sum(0, &bytes[..PAGE - 3]);
    assert_eq!(
        first,
        Sealed {
            len: (PAGE - 3) as u64,
            tail: first_tail,
            sums: 0
        }
    );
    drop(file);

    let mut file = PagedFile::open(fs.as_ref(), path, first, FileKind::Segment).expect("reopen");
    file.append(&bytes[PAGE - 3..]).expect("append");
    let sealed = file.seal().expect("seal");
    let sums: Vec<u8> = (0u64..)
        .zip(bytes.chunks(PAGE).take(3))
        .flat_map(|(index, page)| page_sum(index, page).to_le_bytes())
        .collect();
    let tail = page_sum(3, &bytes[3 * PAGE..]);
    let expected = Sealed {
        len: bytes.len() as u64,
        tail,
        sums: crc32fast::hash(&sums),
    };
    assert_eq!(
        sealed, expected,
        "one digest whether the checksums were sealed at once or not"
    );
    assert_eq!(fs.contents(&sums_path(path)).expect("sums"), sums);

    let pages = file.pages(sealed, None).expect("pages");
    let read = pages.read(PAGE - 10..PAGE + 10).expect("read");
    assert_eq!(read, &bytes[PAGE - 10..PAGE + 10]);
    assert_eq!(pages.bytes(0..bytes.len()).expect("bytes"), bytes);

    // longer file: uncommitted bytes past the seal dropped at open
    fs.corrupt(path, |data| data.extend_from_slice(&[9; 17]));
    drop(file);
    PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment).expect("reopen past orphans");
    assert_eq!(fs.contents(path).expect("data"), bytes);

    fs.corrupt(path, |data| data[PAGE + 5] ^= 1);
    let pages = Pages::open(fs.as_ref(), path, sealed, Access::Normal).expect("lengths intact");
    let untouched = pages.read(0..PAGE).expect("untouched page");
    assert_eq!(untouched, &bytes[..PAGE]);
    let touched = pages.read(PAGE..PAGE + 1);
    assert!(
        matches!(touched, Err(PageError::Corrupt { page: 1, .. })),
        "a corrupt page never serves: {touched:?}"
    );
    let again = pages.bytes(PAGE..PAGE + 1);
    assert!(
        matches!(again, Err(PageError::Corrupt { page: 1, .. })),
        "nor on a second touch"
    );
    fs.corrupt(path, |data| data[PAGE + 5] ^= 1);

    // pages 0 and 1 swapped together with their checksums: each still fails at its new index
    let swap = |data: &mut Vec<u8>, width: usize| {
        let (first, second) = data.split_at_mut(width);
        first.swap_with_slice(&mut second[..width]);
    };
    fs.corrupt(path, |data| swap(data, PAGE));
    fs.corrupt(&sums_path(path), |sums| swap(sums, SUM));
    let pages = Pages::open(fs.as_ref(), path, sealed, Access::Normal);
    assert!(
        matches!(pages, Err(PageError::Sums { .. })),
        "digest pins the checksum order"
    );
    let mut transposed = sealed;
    transposed.sums = crc32fast::hash(&fs.contents(&sums_path(path)).expect("sums"));
    let pages = Pages::open(fs.as_ref(), path, transposed, Access::Normal).expect("digest");
    assert!(
        matches!(pages.read(0..PAGE), Err(PageError::Corrupt { page: 0, .. })),
        "a page and its checksum moved together still fail"
    );
    fs.corrupt(path, |data| swap(data, PAGE));
    fs.corrupt(&sums_path(path), |sums| swap(sums, SUM));

    // checksum rewritten to match a changed page: manifest's digest refuses it
    fs.corrupt(path, |data| data[5] ^= 1);
    let forged = page_sum(0, &fs.contents(path).expect("data")[..PAGE]).to_le_bytes();
    fs.corrupt(&sums_path(path), |sums| {
        sums[..SUM].copy_from_slice(&forged)
    });
    assert!(matches!(
        PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment),
        Err(PageError::Sums { .. })
    ));
    let pages = Pages::open(fs.as_ref(), path, sealed, Access::Normal);
    assert!(matches!(pages, Err(PageError::Sums { .. })));
    fs.corrupt(path, |data| data[5] ^= 1);
    let original = page_sum(0, &bytes[..PAGE]).to_le_bytes();
    fs.corrupt(&sums_path(path), |sums| {
        sums[..SUM].copy_from_slice(&original)
    });

    fs.corrupt(path, |data| data[3 * PAGE + 1] ^= 1);
    assert!(matches!(
        PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment),
        Err(PageError::Tail { .. })
    ));
    fs.corrupt(path, |data| data.truncate(10));
    let short = PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment);
    assert!(matches!(short, Err(PageError::Lost { have: 10, .. })));
}

/// Reads outside the sealed bytes (past the end, inverted): `OutOfRange`, never a panic
#[test]
fn reads_outside_the_seal_are_errors() {
    let fs = SimFs::new();
    let path = Path::new("/p/data");
    fs.create_dir_all(Path::new("/p")).expect("dir");
    let mut file =
        PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
    file.append(&vec![3; 2 * PAGE + 5]).expect("append");
    let sealed = file.seal().expect("seal");
    let pages = file.pages(sealed, None).expect("pages");
    let len = 2 * PAGE + 5;

    let past = pages.read(len - 1..len + 1);
    assert!(
        matches!(past, Err(PageError::OutOfRange { end, .. }) if end == len + 1),
        "{past:?}"
    );
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = pages.bytes(9..3);
    assert!(
        matches!(
            inverted,
            Err(PageError::OutOfRange {
                start: 9,
                end: 3,
                ..
            })
        ),
        "{inverted:?}"
    );
    assert_eq!(pages.read(PAGE..PAGE + 1).expect("in range"), &[3]);
}

/// Open's tail-page + checksum reads: failure = the read's `Err` (never a panic, never a file
/// opened on unread bytes)
#[test]
fn a_failed_read_at_open_surfaces_as_an_error() {
    let fs = SimFs::new();
    let path = Path::new("/p/data");
    fs.create_dir_all(Path::new("/p")).expect("dir");
    let mut file =
        PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, FileKind::Segment).expect("open");
    file.append(&vec![7; 2 * PAGE + 5]).expect("append");
    let sealed = file.seal().expect("seal");
    drop(file);

    for fail_at in 0..2 {
        let fs = fs.restarted();
        fs.fail_reads_from(fail_at);
        let error = PagedFile::open(fs.as_ref(), path, sealed, FileKind::Segment)
            .expect_err("a read failed");
        assert!(
            error.to_string().contains("injected read EIO"),
            "read {fail_at}: {error}"
        );
    }
    PagedFile::open(fs.restarted().as_ref(), path, sealed, FileKind::Segment)
        .expect("healthy reads open");
}

/// Log beside a segment fed the same bytes:
///
/// - log linked durably at creation (either of its two files missing counts)
/// - appends inside a zeroed reserve (size fixed between growth steps: seal = no metadata)
/// - seals + reads = the segment's; reopen drops the reserve, next append rebuilds it
#[test]
fn a_log_grows_into_a_reserve_its_seals_never_see() {
    let fs = SimFs::new();
    for dir in ["/segment", "/log", "/half"] {
        fs.create_dir_all(Path::new(dir)).expect("dir");
    }
    fs.sync_dir(Path::new("/")).expect("link every directory");

    // data durably linked, checksums never created (crash between the two): still linked
    let half = Path::new("/half/data");
    fs.open(half).expect("data only");
    fs.sync_dir(Path::new("/half")).expect("link the data");
    PagedFile::open(fs.as_ref(), half, Sealed::EMPTY, FileKind::Log).expect("open half");
    assert!(
        fs.power_loss().contents(&sums_path(half)).is_some(),
        "the missing .crc linked"
    );
    let (segment_path, log_path) = (Path::new("/segment/data"), Path::new("/log/data"));
    let bytes: Vec<u8> = (0..3 * PAGE + 100).map(|n| (n * 7 % 251) as u8).collect();
    let len = |path: &Path| fs.contents(path).expect("file").len() as u64;

    let open = |path, kind| PagedFile::open(fs.as_ref(), path, Sealed::EMPTY, kind);
    let mut segment = open(segment_path, FileKind::Segment).expect("open segment");
    let mut log = open(log_path, FileKind::Log).expect("open log");
    let crashed = fs.power_loss();
    let linked = |path: &Path| crashed.contents(path).is_some();
    assert!(
        linked(log_path) && linked(&sums_path(log_path)),
        "a log is linked at creation"
    );
    assert!(
        !linked(segment_path),
        "a segment waits for its owner's directory sync"
    );

    for piece in [&bytes[..10], &bytes[10..]] {
        segment.append(piece).expect("append segment");
        log.append(piece).expect("append log");
        assert_eq!(
            len(log_path),
            MIN_RESERVE,
            "the first step's room, written in place"
        );
    }
    let sealed = segment.seal().expect("seal segment");
    assert_eq!(
        log.seal().expect("seal log"),
        sealed,
        "the reserve is never sealed"
    );
    assert_eq!(
        len(&sums_path(log_path)),
        MIN_RESERVE,
        "the checksums grow the same way"
    );
    let stored = fs.contents(log_path).expect("log");
    assert_eq!(&stored[..bytes.len()], &bytes[..]);
    assert!(
        stored[bytes.len()..].iter().all(|byte| *byte == 0),
        "the rest is zeros"
    );
    let read = |file: &PagedFile| {
        let pages = file.pages(sealed, None).expect("pages");
        pages.bytes(0..bytes.len()).expect("bytes")
    };
    assert_eq!(read(&log), read(&segment));

    log.append(&[1; MIN_RESERVE as usize])
        .expect("past the reserve");
    assert_eq!(
        len(log_path),
        2 * MIN_RESERVE,
        "the next step = the file's size again"
    );
    drop(log);

    let mut reopened =
        PagedFile::open(fs.as_ref(), log_path, sealed, FileKind::Log).expect("reopen");
    assert_eq!(
        len(log_path),
        sealed.len,
        "open truncates to the seal, reserve included"
    );
    reopened.append(&[2]).expect("append after reopen");
    let rebuilt = len(log_path);
    assert!(
        rebuilt > sealed.len + 1 && rebuilt % PAGE as u64 == 0,
        "rebuilt: {rebuilt}"
    );
}

/// Offline scrub, real file past `WRITE_BEHIND` (writeback hint issued): clean, bad page
/// named, orphans counted, short file truncated, absent file missing; mapped onto the port's
/// report
#[test]
fn scrub_names_bad_pages_orphans_and_losses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fs = crate::fs::RealFs::shared();
    let path = dir.path().join("data");
    let bytes: Vec<u8> = (0..WRITE_BEHIND as usize + 2 * PAGE + 9)
        .map(|n| n as u8)
        .collect();
    let mut file =
        PagedFile::open(fs.as_ref(), &path, Sealed::EMPTY, FileKind::Segment).expect("open");
    file.append(&bytes).expect("append");
    let sealed = file.seal().expect("seal");

    let clean = scrub(fs.as_ref(), dir.path(), "data", sealed).expect("scrub");
    assert!(clean.is_clean(), "{clean:?}");
    assert_eq!(clean.committed_bytes, bytes.len() as u64);
    let unit = clean.into_unit();
    assert_eq!((unit.name.as_str(), unit.problems), ("data", Vec::new()));

    let mut damaged = bytes.clone();
    damaged[PAGE + 1] ^= 1;
    damaged.extend_from_slice(&[0; 5]);
    std::fs::write(&path, &damaged).expect("write");
    let report = scrub(fs.as_ref(), dir.path(), "data", sealed).expect("scrub");
    assert_eq!(
        (report.bad_pages.clone(), report.orphaned_bytes),
        (vec![1], 5)
    );
    assert!(!report.bad_sums, "the checksums themselves are intact");
    assert_eq!(
        report.into_unit().problems,
        [
            Problem::ChecksumMismatch { page: 1 },
            Problem::Orphaned { bytes: 5 }
        ]
    );

    let sums_at = sums_path(&path);
    let mut sums = std::fs::read(&sums_at).expect("sums");
    sums[0] ^= 1;
    std::fs::write(&sums_at, &sums).expect("write sums");
    let report = scrub(fs.as_ref(), dir.path(), "data", sealed).expect("scrub");
    assert!(report.bad_sums && !report.is_clean(), "{report:?}");

    std::fs::write(&path, &bytes[..PAGE]).expect("truncate");
    let truncated = scrub(fs.as_ref(), dir.path(), "data", sealed).expect("scrub");
    assert_eq!(truncated.lost, Some(Lost::Truncated));
    assert_eq!(truncated.into_unit().problems, [Problem::Truncated]);
    std::fs::remove_file(&path).expect("remove");
    let missing = scrub(fs.as_ref(), dir.path(), "data", sealed).expect("scrub");
    assert_eq!(missing.into_unit().problems, [Problem::Missing]);
}
