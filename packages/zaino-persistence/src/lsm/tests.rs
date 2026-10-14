//! Map's segments below the store: shape checks, open, corruption, seek arithmetic, prefetch
//! plans, scope filters (store end to end: `disk/tests.rs`)

use std::{
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

use bytes::Bytes;

use super::{
    dir::SegmentDir,
    file::{Prefetch, SegmentFile},
    file_name,
    layout::Shape,
    writer::SegmentWriter,
    Fanout, SegmentError, SegmentLog, Slots, Snapshot,
};
use crate::{
    fs::{Access, Fs, SimFs},
    manifest::IndexKind,
    pages::{sums_path, PageError},
    port::{MapTable, Width},
};

/// Two same-tier segments merge (a merge in a handful of writes)
const FANOUT: Fanout = Fanout(2);

/// `account ‖ seq → seq · 1000`, ranges read per account (scope = the account)
fn scanned() -> MapTable {
    MapTable::scoped(0, "scanned", Width::fixed(12), Width::fixed(8), 8)
}

/// Hash-like id → position
fn probed() -> MapTable {
    MapTable::point(0, "probed", Width::fixed(16), Width::fixed(4))
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

fn borrowed(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<(&[u8], Option<&[u8]>)> {
    rows.iter().map(|(key, value)| (key.as_slice(), Some(value.as_slice()))).collect()
}

#[test]
#[should_panic(expected = "LSM map scanned: no Variable keys in the LSM")]
fn a_map_with_variable_keys_panics() {
    Shape::of(&MapTable { key: Width::Variable, ..scanned() });
}

#[test]
#[should_panic(expected = "LSM map scanned: no Variable values in the LSM")]
fn a_map_with_variable_values_panics() {
    Shape::of(&MapTable { value: Width::Variable, ..scanned() });
}

#[test]
#[should_panic(expected = "LSM map scanned: scope 13 > its 12-byte key")]
fn a_map_scoped_past_its_key_panics() {
    Shape::of(&MapTable { scope: 13, ..scanned() });
}

#[test]
#[should_panic(expected = "filter shards on 8 key bytes, has 4")]
fn a_map_scoped_under_the_filter_shard_panics() {
    Shape::of(&MapTable { scope: 4, ..scanned() });
}

#[test]
#[should_panic(expected = "filter shards on 8 key bytes")]
fn a_map_keyed_under_the_filter_shard_panics() {
    Shape::of(&MapTable { key: Width::fixed(6), scope: 0, ..scanned() });
}

#[test]
#[should_panic(expected = "LSM map scanned: key width")]
fn a_batch_row_off_its_key_width_panics() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    let slots = Arc::new(Slots::new(4, u64::MAX));
    let mut log = SegmentLog::open(
        fs,
        Path::new("/m"),
        IndexKind::CompactBlock,
        &scanned(),
        &[],
        FANOUT,
        slots,
    )
    .expect("open");
    let _ = log.batch(vec![(&[0; 11], Some(&[0; 8]))]);
}

#[test]
#[should_panic(expected = "LSM map scanned: value")]
fn a_batch_row_off_its_value_width_panics() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    let slots = Arc::new(Slots::new(4, u64::MAX));
    let mut log = SegmentLog::open(
        fs,
        Path::new("/m"),
        IndexKind::CompactBlock,
        &scanned(),
        &[],
        FANOUT,
        slots,
    )
    .expect("open");
    let _ = log.batch(vec![(&[0; 12], Some(&[0; 7]))]);
}

#[test]
#[should_panic(expected = "LSM map scanned: tombstone")]
fn a_tombstone_in_a_map_without_deletes_panics() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    let slots = Arc::new(Slots::new(4, u64::MAX));
    let mut log = SegmentLog::open(
        fs,
        Path::new("/m"),
        IndexKind::CompactBlock,
        &scanned(),
        &[],
        FANOUT,
        slots,
    )
    .expect("open");
    let _ = log.batch(vec![(&[0; 12], None)]);
}

/// - open publishes every `zaino_lsm_*` series of the map at zero (absent ≠ nothing happened yet)
#[test]
fn open_publishes_every_lsm_series_at_zero_before_any_batch_merge_or_stall() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let rendered = recorder.handle();
    let _recording = metrics::set_default_local_recorder(&recorder);

    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    SegmentLog::open(
        fs,
        Path::new("/m"),
        IndexKind::TransparentAddress,
        &scanned(),
        &[],
        FANOUT,
        Arc::new(Slots::new(4, u64::MAX)),
    )
    .expect("open");

    let scrape = rendered.render();
    let expected = [
        "zaino_lsm_stall_at{index=\"transparent_address\",table=\"scanned\"} 6",
        "zaino_lsm_batch_rows_total{index=\"transparent_address\",table=\"scanned\"} 0",
        "zaino_lsm_merge_rows_total{index=\"transparent_address\",table=\"scanned\"} 0",
        "zaino_lsm_merge_bytes_total{index=\"transparent_address\",table=\"scanned\"} 0",
        "zaino_lsm_merge_cancelled_total{index=\"transparent_address\",table=\"scanned\"} 0",
        "zaino_lsm_merge_duration_seconds_count{index=\"transparent_address\",table=\"scanned\",\
         tier=\"0\"} 0",
        "zaino_lsm_stall_duration_seconds_count{index=\"transparent_address\",table=\"scanned\"} 0",
    ];
    for line in expected {
        assert!(scrape.lines().any(|scraped| scraped == line), "{line:?} not in:\n{scrape}");
    }
}

/// - retain removes unlisted segments (+ checksums) + any writer's scratch
/// - short or unsummed segment refused; flipped committed byte passes open (lengths only)
#[test]
fn retain_removes_strays_and_open_checks_lengths_leaving_pages_to_the_first_touch() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
    // enough rows that page 0 holds records only (open reads the summary, past them)
    let rows = |owner| (0..600).map(move |seq| scanned_row(owner, seq)).collect::<Vec<_>>();
    let kept = writer.write(0, borrowed(&rows(1))).expect("write").expect("rows");
    let orphan = writer.write(1, borrowed(&[scanned_row(3, 0)])).expect("write").expect("rows");
    // merge cut short by a crash leaves its scratch, even under a listed segment's id
    for leftover in ["0000000000.fences.scratch", "0000000009.filter.scratch"] {
        let scratch = fs.open(&dir.join(leftover)).expect("scratch").file;
        scratch.write_all_at(&[1; 9], 0).expect("write");
    }
    writer.dir().sync().expect("sync");

    writer.dir().retain(&[kept]).expect("retain");
    let path = dir.join(file_name(kept.id));
    let mut kept_files = vec![file_name(kept.id), format!("{}.crc", file_name(kept.id))];
    kept_files.sort();
    assert_eq!(fs.list(dir).expect("list"), kept_files, "unlisted segment {} removed", orphan.id);

    let original = fs.contents(&path).expect("segment bytes");
    fs.corrupt(&path, |bytes| bytes.truncate(10));
    let short = Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &[kept]).map(|_| ());
    assert!(matches!(short, Err(SegmentError::Page(PageError::Lost { have: 10, .. }))));

    fs.corrupt(&path, |bytes| {
        *bytes = original.clone();
        bytes[shape.stride + 9] ^= 1;
    });
    Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &[kept]).expect("lengths intact");

    fs.remove(&sums_path(&path)).expect("remove checksums");
    let unsummed = Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &[kept]).map(|_| ());
    assert!(matches!(unsummed, Err(SegmentError::Page(PageError::Lost { .. }))));
}

#[test]
#[should_panic(expected = "page 0: checksum mismatch (on-disk corruption: stop zainod, run")]
fn a_read_of_a_corrupt_page_panics_naming_the_page_and_the_fix() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let rows: Vec<_> = (0..600).map(|seq| scanned_row(1, seq)).collect();
    let kept = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape)
        .write(0, borrowed(&rows))
        .expect("write")
        .expect("rows");
    // page 0 = records only (600 rows: summary past it)
    fs.corrupt(&dir.join(file_name(kept.id)), |bytes| bytes[shape.stride + 9] ^= 1);
    let snapshot =
        Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &[kept]).expect("lengths intact");

    snapshot.get(&scanned_row(1, 1).0);
}

/// Same tier at fanout 2 → open launches their merge (reads the corrupt page on its thread)
#[test]
#[should_panic(expected = "page 0: checksum mismatch")]
fn a_merge_over_a_corrupt_page_panics_at_the_next_batch() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
    let rows = |owner| (0..600).map(move |seq| scanned_row(owner, seq)).collect::<Vec<_>>();
    let kept = writer.write(0, borrowed(&rows(1))).expect("write").expect("rows");
    let second = writer.write(2, borrowed(&rows(4))).expect("write").expect("rows");
    fs.corrupt(&dir.join(file_name(kept.id)), |bytes| bytes[shape.stride + 9] ^= 1);
    let slots = Arc::new(Slots::new(4, u64::MAX));
    let index = IndexKind::CompactBlock;
    let mut log =
        SegmentLog::open(fs.clone(), dir, index, &scanned(), &[kept, second], FANOUT, slots)
            .expect("open");
    log.settle();

    let _ = log.batch(Vec::new());
}

/// Last byte = filter fingerprint; whole filter read + checked at map → open dies before any probe
#[test]
#[should_panic(expected = "checksum mismatch")]
fn a_corrupt_filter_panics_at_open() {
    let fs = SimFs::new();
    let ids = Path::new("/ids");
    fs.create_dir_all(ids).expect("dir");
    let shape = Shape::of(&probed());
    let rows: Vec<_> = (0..3_000).map(probed_row).collect();
    let written = SegmentWriter::new(SegmentDir::new(fs.clone(), ids), shape)
        .write(0, borrowed(&rows))
        .expect("write")
        .expect("rows");
    fs.corrupt(&ids.join(file_name(written.id)), |bytes| *bytes.last_mut().expect("bytes") ^= 1);

    let _ = Snapshot::open(&SegmentDir::new(fs.clone(), ids), shape, &[written]);
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
    let meta = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape)
        .write(0, borrowed(&written))
        .expect("write")
        .expect("rows");
    let file = SegmentFile::open(&SegmentDir::new(fs.clone(), dir), &meta, shape, Access::Normal)
        .expect("open");

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
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
    let spans = [0..20_000u32, 20_000..40_000, 40_000..60_000];
    let metas: Vec<_> = (0u32..)
        .zip(&spans)
        .map(|(id, span)| {
            let rows: Vec<_> = span.clone().map(probed_row).collect();
            writer.write(id, borrowed(&rows)).expect("write").expect("rows")
        })
        .collect();
    writer.dir().sync().expect("sync");
    let snapshot = Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &metas).expect("open");
    let files: Vec<SegmentFile> = metas
        .iter()
        .map(|meta| {
            SegmentFile::open(&SegmentDir::new(fs.clone(), dir), meta, shape, Access::Normal)
        })
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
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
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
    writer.dir().sync().expect("sync");

    let snapshot =
        Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &segments).expect("open");
    let range = |a: u64| {
        let (start, end) = (account(a).to_vec(), [&account(a)[..], &[0xff; 4]].concat());
        snapshot.range(&start, &end, usize::MAX).expect("unbounded")
    };
    assert_eq!(range(0).len(), 6 * 20, "the shared account from every segment");
    assert_eq!(range(301).len(), 20, "an account from one segment");
    assert!(range(7_777).is_empty(), "an account no segment holds");

    let file =
        SegmentFile::open(&SegmentDir::new(fs.clone(), dir), &segments[0], shape, Access::Normal)
            .expect("open");
    file.warm_filter();
    assert!((1..50).all(|a| file.may_contain(&account(a))), "no false negative");
    let absent = 10_000u64;
    let passed = (1_000..1_000 + absent).filter(|a| file.may_contain(&account(*a))).count();
    assert!(passed < 200, "{passed} of {absent} absent accounts passed (≈ 39 expected)");
}

/// Merge of `deletes()` segments, one row group per key (row `(n, live)` = `probed_row(n)`, its
/// value if `live`, else its tombstone; expected = output rows + pairs cancelled):
///
/// - value + tombstone → both dropped, in either input order; all dropped → no output segment
/// - lone tombstone → kept (its value lives outside the merge)
/// - two values, two tombstones, three rows of one key → `Contract`, naming the rows
#[test]
fn a_merge_cancels_value_tombstone_pairs_keeps_lone_tombstones_and_refuses_other_duplicates() {
    type Rows = &'static [(u32, bool)];
    type Merged = Result<(Rows, u64), &'static str>;
    let cases: [(&str, &[Rows], Merged); 7] = [
        ("pair, nothing left", &[&[(1, true)], &[(1, false)]], Ok((&[], 1))),
        ("tombstone first", &[&[(1, false)], &[(1, true)]], Ok((&[], 1))),
        (
            "pair among others",
            &[&[(1, true), (2, true)], &[(1, false), (3, true)]],
            Ok((&[(2, true), (3, true)], 1)),
        ),
        ("lone tombstone", &[&[(1, false)], &[(2, true)]], Ok((&[(1, false), (2, true)], 0))),
        ("two values", &[&[(1, true)], &[(1, true)]], Err("key held 2 times: value, value")),
        (
            "two tombstones",
            &[&[(1, false)], &[(1, false)]],
            Err("key held 2 times: tombstone, tombstone"),
        ),
        (
            "three rows",
            &[&[(1, true)], &[(1, false)], &[(1, true)]],
            Err("key held 3 times: value, tombstone, value"),
        ),
    ];
    let shape = Shape::of(&probed().deletes());
    let slots = Slots::new(1, u64::MAX);
    let cancel = AtomicBool::new(false);
    for (case, inputs, expected) in cases {
        let fs = SimFs::new();
        let dir = Path::new("/removable");
        fs.create_dir_all(dir).expect("dir");
        let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
        let segments: Vec<_> = (0u32..)
            .zip(inputs)
            .map(|(id, rows)| {
                let rows: Vec<_> = rows.iter().map(|&(n, live)| (probed_row(n), live)).collect();
                let rows = rows
                    .iter()
                    .map(|((key, value), live)| (key.as_slice(), live.then_some(value.as_slice())));
                writer.write(id, rows.collect()).expect("write").expect("rows")
            })
            .collect();
        let slot = slots.acquire(0, &cancel).expect("free slot");
        let merged = writer.merge(99, &segments, &cancel, &slot).map(|merged| {
            let merged = merged.expect("never cancelled");
            let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = merged.output.map_or(Vec::new(), |meta| {
                let file = SegmentFile::open(
                    &SegmentDir::new(fs.clone(), dir),
                    &meta,
                    shape,
                    Access::Sequential,
                )
                .expect("output");
                (0..file.records())
                    .map(|slot| {
                        let (key, value) = file.entry(slot);
                        (key.to_vec(), value.map(|value| value.to_vec()))
                    })
                    .collect()
            });
            (rows, merged.cancelled)
        });
        match (merged, expected) {
            (Ok(answer), Ok((rows, cancelled))) => {
                let mut rows: Vec<_> = rows
                    .iter()
                    .map(|&(n, live)| {
                        let (key, value) = probed_row(n);
                        (key, live.then_some(value))
                    })
                    .collect();
                rows.sort();
                assert_eq!(answer, (rows, cancelled), "{case}");
            }
            (Err(SegmentError::Contract { segment: 99, reason }), Err(named)) => {
                assert!(reason.contains(named), "{case}: {reason}");
            }
            (answer, expected) => panic!("{case}: {answer:?}, expected {expected:?}"),
        }
    }
}

/// Value in one segment, its tombstone in a tombstone-only other, an unrelated third: whatever
/// the list order, and after a merge of any two (adjacent or not), the key reads absent through
/// `get`, `get_many` and `range`, and a range's limit counts live rows only
#[test]
fn a_removed_key_stays_absent_under_every_segment_order_and_merge() {
    let fs = SimFs::new();
    let dir = Path::new("/removable");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&probed().deletes());
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
    let (removed, kept, other) = (probed_row(1), probed_row(2), probed_row(3));
    let held_rows = [removed.clone(), kept.clone()];
    let held = writer.write(0, borrowed(&held_rows)).expect("write").expect("rows");
    let unrelated =
        writer.write(1, borrowed(std::slice::from_ref(&other))).expect("write").expect("rows");
    let tombstone =
        writer.write(2, vec![(removed.0.as_slice(), None)]).expect("write").expect("rows");
    let slots = Slots::new(1, u64::MAX);
    let cancel = AtomicBool::new(false);
    let merge = |id, inputs: &[_]| {
        let slot = slots.acquire(0, &cancel).expect("free slot");
        let merged = writer.merge(id, inputs, &cancel, &slot).expect("merge").expect("finished");
        merged.output.expect("rows left")
    };
    let held_unrelated = merge(3, &[held, unrelated]);
    let unrelated_tombstone = merge(4, &[unrelated, tombstone]);
    let held_tombstone = merge(5, &[held, tombstone]);
    writer.dir().sync().expect("sync");

    let mut live = vec![(kept.0.clone(), kept.1.clone()), (other.0.clone(), other.1.clone())];
    live.sort();
    let lists: [&[_]; 7] = [
        &[held, unrelated, tombstone],
        &[tombstone, unrelated, held],
        &[unrelated, tombstone, held],
        &[held_unrelated, tombstone],
        &[tombstone, held_unrelated],
        &[held, unrelated_tombstone],
        &[held_tombstone, unrelated],
    ];
    // every segment kept on disk (`open` removes what its list leaves out), each list mapped next
    let every = [held, unrelated, tombstone, held_unrelated, unrelated_tombstone, held_tombstone];
    let mapped = Snapshot::open(&SegmentDir::new(fs.clone(), dir), shape, &every).expect("open");
    for listed in lists {
        let ids: Vec<u32> = listed.iter().map(|meta| meta.id).collect();
        let snapshot = mapped.next(&SegmentDir::new(fs.clone(), dir), listed).expect("map");
        assert_eq!(snapshot.get(&removed.0), None, "{ids:?}: get");
        let asked = [removed.0.as_slice(), kept.0.as_slice(), removed.0.as_slice()];
        let expected = vec![None, Some(Bytes::from(kept.1.clone())), None];
        assert_eq!(snapshot.get_many(&asked), expected, "{ids:?}: get_many");
        let range = |limit| {
            let rows = snapshot.range(&[0; 16], &[0xff; 16], limit)?;
            Some(rows.into_iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect::<Vec<_>>())
        };
        assert_eq!(range(2), Some(live.clone()), "{ids:?}: range at its live size");
        assert_eq!(range(1), None, "{ids:?}: range under its live size");
    }
}
