//! Map's segments below the store: shape checks, open, corruption, seek arithmetic, prefetch
//! plans, scope filters (store end to end: `disk/tests.rs`)

use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::Arc,
};

use super::{
    file::{Prefetch, SegmentFile},
    file_name,
    layout::Shape,
    writer::SegmentWriter,
    SegmentError, SegmentLog, Slots, Snapshot,
};
use crate::{
    fs::{Access, Fs, SimFs},
    pages::{sums_path, PageError},
    port::{MapTable, Width},
};

/// `account ‖ seq → seq · 1000`, ranges read per account (scope = the account)
fn scanned() -> MapTable {
    MapTable::new(0, "scanned", Width::fixed(12), Width::fixed(8), 8)
}

/// Hash-like id → position, point lookups only
fn probed() -> MapTable {
    MapTable::new(0, "probed", Width::fixed(16), Width::fixed(4), 0)
}

/// Uniform 8 bytes per account (the filter shards on them)
fn account(n: u64) -> [u8; 8] {
    n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

fn scanned_row(owner: u64, seq: u32) -> (Vec<u8>, Vec<u8>) {
    let key = [&account(owner)[..], &seq.to_be_bytes()].concat();
    (key, (u64::from(seq) * 1000).to_be_bytes().to_vec())
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`)
fn probed_row(n: u32) -> (Vec<u8>, Vec<u8>) {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    (id, n.to_be_bytes().to_vec())
}

fn borrowed(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<(&[u8], &[u8])> {
    rows.iter().map(|(key, value)| (key.as_slice(), value.as_slice())).collect()
}

/// Panic payload text (`panic!` with arguments → `String`, a literal → `&str`)
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => {
            payload.downcast::<&str>().map(|message| message.to_string()).unwrap_or_default()
        }
    }
}

/// - map the LSM cannot hold panics at construction, naming the map and why
/// - batch row of the wrong widths panics naming the map
#[test]
fn a_map_or_row_the_lsm_cannot_hold_panics_naming_the_map() {
    let cases = [
        (MapTable { key: Width::Variable, ..scanned() }, "LSM map scanned: keys must be Fixed"),
        (MapTable { value: Width::Variable, ..scanned() }, "LSM map scanned: values must be Fixed"),
        (MapTable { scope: 13, ..scanned() }, "LSM map scanned: scope 13 > its 12-byte key"),
        (MapTable { scope: 4, ..scanned() }, "filter shards on 8 key bytes, has 4"),
        (MapTable { key: Width::fixed(6), scope: 0, ..scanned() }, "filter shards on 8 key bytes"),
    ];
    for (table, expected) in cases {
        let message = panic_message(catch_unwind(|| Shape::of(&table)).expect_err(expected));
        assert!(message.contains(expected), "expected {expected:?}, got {message:?}");
    }

    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    let mut log =
        SegmentLog::open(fs, Path::new("/m"), &scanned(), &[], 2, Arc::new(Slots::new(4)))
            .expect("open");
    let short_value = catch_unwind(AssertUnwindSafe(|| log.batch(vec![(&[0; 12], &[0; 7])])));
    let message = panic_message(short_value.expect_err("a 7-byte value"));
    assert!(message.contains("LSM map scanned: (key, value) widths"), "{message}");
}

/// - open removes unlisted segments (+ checksums) + any writer's scratch; lost segment refused
/// - flipped committed byte passes open (lengths only), dies on the first read / merge touching
///   its page
#[test]
fn open_checks_lengths_and_a_corrupt_page_dies_on_first_touch() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    // enough rows that page 0 holds records only (open reads the summary, past them)
    let rows = |owner| (0..600).map(move |seq| scanned_row(owner, seq)).collect::<Vec<_>>();
    let kept = writer.write(0, borrowed(&rows(1))).expect("write").expect("rows");
    let orphan = writer.write(1, borrowed(&[scanned_row(3, 0)])).expect("write").expect("rows");
    // merge cut short by a crash leaves its scratch, even under a listed segment's id
    for leftover in ["0000000000.fences.scratch", "0000000009.filter.scratch"] {
        fs.open(&dir.join(leftover)).expect("scratch").write_all_at(&[1; 9], 0).expect("write");
    }
    writer.sync_dir().expect("sync");

    Snapshot::open(fs.as_ref(), dir, shape, &[kept]).expect("open");
    let path = dir.join(file_name(kept.id));
    let mut kept_files = vec![file_name(kept.id), format!("{}.crc", file_name(kept.id))];
    kept_files.sort();
    assert_eq!(fs.list(dir).expect("list"), kept_files, "unlisted segment {} removed", orphan.id);

    let original = fs.contents(&path).expect("segment bytes");
    fs.corrupt(&path, |bytes| bytes.truncate(10));
    let short = Snapshot::open(fs.as_ref(), dir, shape, &[kept]).map(|_| ());
    assert!(matches!(short, Err(SegmentError::Page(PageError::Lost { have: 10, .. }))));

    fs.corrupt(&path, |bytes| {
        *bytes = original.clone();
        bytes[shape.stride + 9] ^= 1;
    });
    let snapshot = Snapshot::open(fs.as_ref(), dir, shape, &[kept]).expect("lengths intact");
    let died = |touch: &dyn Fn()| {
        let message =
            panic_message(catch_unwind(AssertUnwindSafe(touch)).expect_err("never serves"));
        let named =
            message.contains("page 0: checksum mismatch") && message.contains("zainod verify");
        assert!(named, "{message}");
    };
    died(&|| {
        snapshot.get(&scanned_row(1, 1).0);
    });

    // same tier as `kept` at fanout 2 → opening both launches their merge (reads the page)
    let second = writer.write(2, borrowed(&rows(4))).expect("write").expect("rows");
    let log =
        SegmentLog::open(fs.clone(), dir, &scanned(), &[kept, second], 2, Arc::new(Slots::new(4)))
            .expect("merge");
    log.settle();
    let log = std::sync::Mutex::new(log);
    died(&|| {
        let _ = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).batch(Vec::new());
    });

    fs.remove(&sums_path(&path)).expect("remove checksums");
    let unsummed = Snapshot::open(fs.as_ref(), dir, shape, &[kept]).map(|_| ());
    assert!(matches!(unsummed, Err(SegmentError::Page(PageError::Lost { .. }))));

    // last byte = filter fingerprint; whole filter read + checked at map → open dies before
    // any probe
    let ids = Path::new("/ids");
    fs.create_dir_all(ids).expect("dir");
    let id_shape = Shape::of(&probed());
    let id_rows: Vec<_> = (0..3_000).map(probed_row).collect();
    let id_writer = SegmentWriter::open(fs.clone(), ids, id_shape);
    let written = id_writer.write(0, borrowed(&id_rows)).expect("write").expect("rows");
    fs.corrupt(&ids.join(file_name(written.id)), |bytes| *bytes.last_mut().expect("bytes") ^= 1);
    let message = panic_message(
        catch_unwind(AssertUnwindSafe(|| {
            Snapshot::open(fs.as_ref(), ids, id_shape, &[written]).map(|_| ())
        }))
        .expect_err("a corrupt filter never answers"),
    );
    let page = (written.sealed.len - 1) / 4096;
    assert!(message.contains(&format!("page {page}: checksum mismatch")), "{message}");
}

/// Several summary groups:
///
/// - `seek` finds every key's slot, the slot after a gap, both ends (summary + one fence page)
/// - prefetch names exactly what seek reads: key block's whole fence group, then its whole block
///   of records (layout arithmetic written out independently)
#[test]
fn seek_crosses_summary_groups_to_the_right_slot_and_prefetch_names_its_pages() {
    let fs = SimFs::new();
    let dir = Path::new("/wide");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let Shape { stride, key_len, block_rows, group_fences, .. } = shape;
    let group_rows = block_rows * group_fences;
    let rows = 3 * group_rows + 999;
    // even seqs only: every odd seq sits in a gap
    let written: Vec<_> = (0..rows).map(|n| scanned_row(7, 2 * n as u32)).collect();
    let meta = SegmentWriter::open(fs.clone(), dir, shape)
        .write(0, borrowed(&written))
        .expect("write")
        .expect("rows");
    let file = SegmentFile::open(fs.as_ref(), dir, &meta, shape, Access::Normal).expect("open");

    let key = |seq: u32| scanned_row(7, seq).0;
    let (below, above) = (account(7).to_vec(), [&account(7)[..], &[0xff; 4]].concat());
    assert_eq!(file.seek(&below), 0, "below every key");
    assert_eq!(file.seek(&above), rows, "above every key");
    let (blocks, fences_at) = (rows.div_ceil(block_rows), rows * stride);
    let edges = [rows - 1, group_rows - 1, group_rows, 2 * group_rows];
    for slot in (0..rows).step_by(997).chain(edges) {
        let seq = 2 * slot as u32;
        assert_eq!(file.seek(&key(seq)), slot, "key at slot {slot}");
        assert_eq!(file.seek(&key(seq + 1)), slot + 1, "gap after slot {slot}");

        let (block, group) = (slot / block_rows, slot / block_rows / group_fences);
        let group_blocks = group * group_fences..((group + 1) * group_fences).min(blocks);
        let fences =
            fences_at + group_blocks.start * key_len..fences_at + group_blocks.end * key_len;
        let records = block * block_rows * stride..((block + 1) * block_rows).min(rows) * stride;
        let prefetched = |step| file.prefetch_range(step, &key(seq));
        assert_eq!(prefetched(Prefetch::Fences), fences, "slot {slot}: its fence group");
        assert_eq!(prefetched(Prefetch::Records), records, "slot {slot}: its block of records");
    }
}

/// Batch's prefetch plan over three segments, asked keys held by the first and the last
///
/// - each round covers, in the holding segment, every asked key's fence group, then its records
/// - whole pages, ascending + disjoint per segment
#[test]
fn a_prefetch_plan_covers_every_asked_key_where_it_lives() {
    let fs = SimFs::new();
    let dir = Path::new("/probed");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&probed());
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    let spans = [0..20_000u32, 20_000..40_000, 40_000..60_000];
    let metas: Vec<_> = (0u32..)
        .zip(&spans)
        .map(|(id, span)| {
            let rows: Vec<_> = span.clone().map(probed_row).collect();
            writer.write(id, borrowed(&rows)).expect("write").expect("rows")
        })
        .collect();
    writer.sync_dir().expect("sync");
    let snapshot = Snapshot::open(fs.as_ref(), dir, shape, &metas).expect("open");
    let files: Vec<SegmentFile> = metas
        .iter()
        .map(|meta| SegmentFile::open(fs.as_ref(), dir, meta, shape, Access::Normal))
        .collect::<Result<_, _>>()
        .expect("each segment, mapped on its own");

    let asked: Vec<u32> = (1_000..1_100).chain(45_000..45_100).collect();
    let keys: Vec<Vec<u8>> = asked.iter().map(|n| probed_row(*n).0).collect();
    let mut sorted: Vec<(&[u8], usize)> =
        keys.iter().enumerate().map(|(at, key)| (key.as_slice(), at)).collect();
    sorted.sort_unstable();
    let holder = |n: u32| spans.iter().position(|span| span.contains(&n)).expect("held");

    for step in [Prefetch::Fences, Prefetch::Records] {
        let plan = snapshot.prefetch_plan(step, &sorted);
        for segment in 0..spans.len() {
            let ranges: Vec<_> =
                plan.iter().filter(|(s, _)| *s == segment).map(|(_, r)| r).collect();
            let whole_pages = ranges.iter().all(|r| r.start % 4096 == 0 && r.end % 4096 == 0);
            let disjoint = ranges.windows(2).all(|pair| pair[0].end < pair[1].start);
            assert!(whole_pages && disjoint, "{step:?}, segment {segment}: {ranges:?}");
        }
        for (&n, key) in asked.iter().zip(&keys) {
            let segment = holder(n);
            let wanted = files[segment].prefetch_range(step, key);
            let covered = plan.iter().any(|(s, range)| {
                *s == segment && range.start <= wanted.start && wanted.end <= range.end
            });
            assert!(covered, "{step:?}: id {n}'s {wanted:?} in segment {segment}");
        }
    }
}

/// - ranges within one account → every row across segments
/// - segment's filter turns away accounts it never held at ≈ its false-positive rate (2^-8)
#[test]
fn a_scope_filter_skips_segments_without_the_scope_and_misses_nothing() {
    let fs = SimFs::new();
    let dir = Path::new("/accounts");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    // account 0 in every segment, accounts 100·s + 1 .. 100·s + 49 only in segment s
    let segments: Vec<_> = (0u64..6)
        .map(|s| {
            let accounts = std::iter::once(0).chain(100 * s + 1..100 * s + 50);
            let rows: Vec<_> = accounts
                .flat_map(|a| (0..20).map(move |seq| scanned_row(a, seq + 100 * s as u32)))
                .collect();
            writer.write(s as u32, borrowed(&rows)).expect("write").expect("rows")
        })
        .collect();
    writer.sync_dir().expect("sync");

    let snapshot = Snapshot::open(fs.as_ref(), dir, shape, &segments).expect("open");
    let range = |a: u64| {
        let (start, end) = (account(a).to_vec(), [&account(a)[..], &[0xff; 4]].concat());
        snapshot.range(&start, &end, usize::MAX).expect("unbounded")
    };
    assert_eq!(range(0).len(), 6 * 20, "the shared account from every segment");
    assert_eq!(range(301).len(), 20, "an account from one segment");
    assert!(range(7_777).is_empty(), "an account no segment holds");

    let file =
        SegmentFile::open(fs.as_ref(), dir, &segments[0], shape, Access::Normal).expect("open");
    file.warm_filter();
    assert!((1..50).all(|a| file.may_contain(&account(a))), "no false negative");
    let absent = 10_000u64;
    let passed = (1_000..1_000 + absent).filter(|a| file.may_contain(&account(*a))).count();
    assert!(passed < 200, "{passed} of {absent} absent accounts passed (≈ 39 expected)");
}
