//! Map's segments below the store: shape checks, open, corruption, seek arithmetic, prefetch
//! plans, scope filters (store end to end: `disk/tests.rs`)

use std::{
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

use super::{
    dir::SegmentDir,
    file::{Prefetch, SegmentFile},
    file_name,
    layout::Shape,
    writer::SegmentWriter,
    Fanout, SegmentError, SegmentMeta, SegmentSet, SegmentsView, Slots,
};
use crate::{
    fs::{Access, Fs, SimFs},
    pages::{sums_path, PAGE},
    schema::{IndexKind, MapTable},
};

/// Two same-tier segments merge (a merge in a handful of writes)
const FANOUT: Fanout = Fanout(2);

/// `account ‖ seq → seq · 1000`, ranges read per account (scope = the account)
fn scanned() -> MapTable {
    MapTable::scoped(0, "scanned", 12, 8, 8)
}

/// Hash-like id → position
fn probed() -> MapTable {
    MapTable::point(0, "probed", 16, 4)
}

/// Uniform 8 bytes per account (the filter shards on them)
fn account(n: u64) -> [u8; 8] {
    n.wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

fn scanned_row(owner: u64, seq: u32) -> (Vec<u8>, Vec<u8>) {
    let key = [&account(owner)[..], &seq.to_be_bytes()].concat();
    (key, (u64::from(seq) * 1000).to_be_bytes().to_vec())
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`), value `n`
fn probed_row(n: u32) -> (Vec<u8>, Vec<u8>) {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    (id, n.to_be_bytes().to_vec())
}

/// `probed_row` key or value back to its `n` (failure output names ids, not bytes)
fn id(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(*bytes.last_chunk().expect("4 trailing bytes"))
}

fn borrowed(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<(&[u8], Option<&[u8]>)> {
    rows.iter().map(|(key, value)| (key.as_slice(), Some(value.as_slice()))).collect()
}

/// Empty set of `table` in `/m` (merge cap 4, unpaced)
fn empty_set(table: &MapTable) -> SegmentSet {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    let slots = Arc::new(Slots::new(4, u64::MAX));
    SegmentSet::open(fs, Path::new("/m"), IndexKind::CompactBlock, table, &[], FANOUT, slots)
        .expect("open")
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
#[should_panic(expected = "filter shards on 8 key bytes, has 6")]
fn a_map_keyed_under_the_filter_shard_panics() {
    Shape::of(&MapTable::point(0, "keyed", 6, 8));
}

#[test]
#[should_panic(expected = "LSM map scanned: key width")]
fn a_batch_row_off_its_key_width_panics() {
    let _ = empty_set(&scanned()).batch(vec![(&[0; 11], Some(&[0; 8]))]);
}

#[test]
#[should_panic(expected = "LSM map scanned: value")]
fn a_batch_row_off_its_value_width_panics() {
    let _ = empty_set(&scanned()).batch(vec![(&[0; 12], Some(&[0; 7]))]);
}

#[test]
#[should_panic(expected = "LSM map scanned: tombstone")]
fn a_tombstone_in_a_map_without_deletes_panics() {
    let _ = empty_set(&scanned()).batch(vec![(&[0; 12], None)]);
}

/// - open publishes every `zaino_lsm_*` series of the index at zero (absent ≠ nothing happened yet)
/// - an index's maps sum into one series per metric, a dropped map's share withdrawn
#[test]
fn an_index_s_maps_sum_into_one_series_and_a_dropped_map_withdraws_its_share() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let rendered = recorder.handle();
    let _recording = metrics::set_default_local_recorder(&recorder);
    let fs = SimFs::new();
    let open = |dir: &str| {
        fs.create_dir_all(Path::new(dir)).expect("dir");
        SegmentSet::open(
            fs.clone(),
            Path::new(dir),
            IndexKind::TransparentAddress,
            &scanned(),
            &[],
            FANOUT,
            Arc::new(Slots::new(4, u64::MAX)),
        )
        .expect("open")
    };
    let scraped = |expected: &[&str]| {
        let scrape = rendered.render();
        for line in expected {
            assert!(scrape.lines().any(|scraped| scraped == *line), "{line:?} not in:\n{scrape}");
        }
        assert!(!scrape.contains("table="), "per-map label leaked:\n{scrape}");
    };

    let mut first = open("/a");
    let mut second = open("/b");
    scraped(&[
        "zaino_lsm_segments{index=\"transparent_address\"} 0",
        "zaino_lsm_merging{index=\"transparent_address\"} 0",
        "zaino_lsm_live_rows{index=\"transparent_address\"} 0",
        "zaino_lsm_merge_bytes_total{index=\"transparent_address\"} 0",
        "zaino_lsm_merge_duration_seconds_count{index=\"transparent_address\"} 0",
        "zaino_lsm_stall_duration_seconds_count{index=\"transparent_address\"} 0",
    ]);

    // one segment of 1 key in `first`, one of 2 keys in `second`
    for (set, rows) in [
        (&mut first, vec![scanned_row(1, 0)]),
        (&mut second, vec![scanned_row(2, 0), scanned_row(2, 1)]),
    ] {
        set.batch(borrowed(&rows)).expect("batch");
        set.on_manifest_durable().expect("manifest");
    }
    scraped(&[
        "zaino_lsm_segments{index=\"transparent_address\"} 2",
        "zaino_lsm_live_rows{index=\"transparent_address\"} 3",
    ]);

    drop(second);
    scraped(&[
        "zaino_lsm_segments{index=\"transparent_address\"} 1",
        "zaino_lsm_live_rows{index=\"transparent_address\"} 1",
    ]);
}

/// Listed segment 0, unlisted segment 1, scratch a crash left behind (one under a listed id)
#[test]
fn retain_removes_unlisted_segments_their_checksums_and_any_writers_scratch() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), Shape::of(&scanned()));
    let kept = writer.write(0, borrowed(&[scanned_row(1, 0)])).expect("write").expect("rows");
    writer.write(1, borrowed(&[scanned_row(3, 0)])).expect("write").expect("rows");
    for leftover in ["0000000000.fences.scratch", "0000000009.filter.scratch"] {
        let scratch = fs.open(&dir.join(leftover)).expect("scratch").file;
        scratch.write_all_at(&[1; 9], 0).expect("write");
    }
    writer.dir().sync().expect("sync");

    writer.dir().retain(&[kept]).expect("retain");

    let left = fs.list(dir).expect("list");
    assert_eq!(left, ["0000000000.seg", "0000000000.seg.crc"], "only the listed segment");
}

/// Segment of 600 rows (page 0 = records only: the summary open reads sits past it)
#[test]
fn open_refuses_a_listed_segment_cut_short_or_without_checksums() {
    struct Damaged {
        case: &'static str,
        damage: fn(&SimFs, &Path),
        refused: &'static str,
    }
    let cases = [
        Damaged {
            case: "cut to 10 bytes",
            damage: |fs, path| fs.corrupt(path, |bytes| bytes.truncate(10)),
            refused: "/segments/0000000000.seg is 10 bytes, the durable state needs 12085",
        },
        Damaged {
            case: "checksums removed",
            damage: |fs, path| fs.remove(&sums_path(path)).expect("remove checksums"),
            refused: "/segments/0000000000.seg.crc is 0 bytes, the durable state needs 8",
        },
    ];
    for Damaged { case, damage, refused } in cases {
        let fs = SimFs::new();
        let dir = Path::new("/segments");
        fs.create_dir_all(dir).expect("dir");
        let shape = Shape::of(&scanned());
        let rows: Vec<_> = (0..600).map(|seq| scanned_row(1, seq)).collect();
        let segment = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape)
            .write(0, borrowed(&rows))
            .expect("write")
            .expect("rows");
        damage(&fs, &dir.join(file_name(segment.id)));

        let opened = SegmentsView::open(&SegmentDir::new(fs.clone(), dir), shape, &[segment]);

        let error = opened.map(|_| ()).expect_err(case);
        assert_eq!(error.to_string(), refused, "{case}");
    }
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
    let segments_view = SegmentsView::open(&SegmentDir::new(fs.clone(), dir), shape, &[kept])
        .expect("open checks lengths only");

    segments_view.get(&scanned_row(1, 1).0);
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
    let mut set =
        SegmentSet::open(fs.clone(), dir, index, &scanned(), &[kept, second], FANOUT, slots)
            .expect("open");
    set.settle();

    let _ = set.batch(Vec::new());
}

/// - last byte = filter fingerprint
/// - whole filter read + checked at map → open dies before any probe
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

    let _ = SegmentsView::open(&SegmentDir::new(fs.clone(), ids), shape, &[written]);
}

/// Account 7's even seqs (every odd seq = a gap), 3 summary groups + 999 rows past them
///
/// - prefetch fences round: the key's whole summary group of fences
/// - prefetch records round: the key's whole block of records (the last block short)
#[test]
fn seek_finds_each_slot_and_prefetch_names_the_fence_group_then_the_record_block_it_reads() {
    let fs = SimFs::new();
    let dir = Path::new("/wide");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let layout = (shape.stride, shape.key_len, shape.block_rows, shape.group_fences);
    assert_eq!(layout, (20, 12, 204, 341), "(stride, key bytes, rows a block, fences a group)");
    let rows = 3 * 69_564 + 999;
    let written: Vec<_> = (0..rows).map(|slot| scanned_row(7, 2 * slot as u32)).collect();
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
    for slot in (0..rows).step_by(997).chain([69_563, 69_564, 139_128, rows - 1]) {
        let seq = 2 * slot as u32;
        assert_eq!(file.seek(&key(seq)), slot, "seq {seq}: its own slot");
        assert_eq!(
            file.seek(&key(seq + 1)),
            slot + 1,
            "seq {} in the gap after slot {slot}",
            seq + 1
        );
    }

    let fences_at = rows * 20;
    struct Case {
        slot: usize,
        group_blocks: std::ops::Range<usize>,
        block_rows: std::ops::Range<usize>,
    }
    #[rustfmt::skip]
    let cases = [
        Case { slot: 0,       group_blocks: 0..341,     block_rows: 0..204 },
        Case { slot: 69_563,  group_blocks: 0..341,     block_rows: 69_360..69_564 },
        Case { slot: 69_564,  group_blocks: 341..682,   block_rows: 69_564..69_768 },
        Case { slot: 139_128, group_blocks: 682..1_023, block_rows: 139_128..139_332 },
        Case { slot: 209_690, group_blocks: 1_023..1_028, block_rows: 209_508..209_691 },
    ];
    for Case { slot, group_blocks, block_rows } in cases {
        let key = scanned_row(7, 2 * slot as u32).0;
        let fences = fences_at + group_blocks.start * 12..fences_at + group_blocks.end * 12;
        let records = block_rows.start * 20..block_rows.end * 20;
        let what = format!("slot {slot}: fence blocks {group_blocks:?}");
        assert_eq!(file.prefetch_range(Prefetch::Fences, &key), fences, "{what}");
        let what = format!("slot {slot}: rows {block_rows:?}");
        assert_eq!(file.prefetch_range(Prefetch::Records, &key), records, "{what}");
    }
}

/// Batch's prefetch plan over three segments (ids 0..20k, 20k..40k, 40k..60k), asked ids held by
/// the first and the last
///
/// - each round covers, in the holding segment, every asked key's range, nothing in the middle one
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
    let segments_view =
        SegmentsView::open(&SegmentDir::new(fs.clone(), dir), shape, &metas).expect("open");
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

    for step in [Prefetch::Fences, Prefetch::Records] {
        let plan = segments_view.prefetch_plan(step, &sorted);

        let planned = |segment: usize| -> Vec<std::ops::Range<usize>> {
            plan.iter().filter(|(s, _)| *s == segment).map(|(_, r)| r.clone()).collect()
        };
        assert_eq!(planned(1), [], "{step:?}: segment 1 holds no asked id");
        for segment in [0, 2] {
            let ranges = planned(segment);
            let off_page: Vec<_> =
                ranges.iter().filter(|r| r.start % PAGE != 0 || r.end % PAGE != 0).collect();
            let what = format!("{step:?}, segment {segment}");
            assert!(off_page.is_empty(), "{what}: ranges off whole pages {off_page:?}");
            let overlapping: Vec<_> =
                ranges.windows(2).filter(|pair| pair[0].end >= pair[1].start).collect();
            assert!(
                overlapping.is_empty(),
                "{step:?}, segment {segment}: not disjoint + ascending {overlapping:?}"
            );
        }
        for (&n, key) in asked.iter().zip(&keys) {
            let segment = if n < 20_000 { 0 } else { 2 };
            let wanted = files[segment].prefetch_range(step, key);
            let covered = planned(segment)
                .iter()
                .any(|range| range.start <= wanted.start && wanted.end <= range.end);
            assert!(
                covered,
                "{step:?}: id {n}'s {wanted:?} in segment {segment}: plan {:?}",
                planned(segment)
            );
        }
    }
}

/// Account 0 in every segment, accounts `100·s + 1 .. 100·s + 49` only in segment `s`
///
/// - ranges within one account → every row across segments
/// - segment's filter turns away accounts it never held at ≈ its false-positive rate (2^-8)
#[test]
fn a_scope_filter_skips_segments_without_the_scope_and_misses_nothing() {
    let fs = SimFs::new();
    let dir = Path::new("/accounts");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of(&scanned());
    let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
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

    let segments_view =
        SegmentsView::open(&SegmentDir::new(fs.clone(), dir), shape, &segments).expect("open");
    let range = |a: u64| {
        let (start, end) = (account(a).to_vec(), [&account(a)[..], &[0xff; 4]].concat());
        segments_view.range(&start, &end, usize::MAX).expect("unbounded").len()
    };
    assert_eq!(range(0), 6 * 20, "the shared account: 20 rows from every segment");
    assert_eq!(range(301), 20, "an account from one segment");
    assert_eq!(range(7_777), 0, "an account no segment holds");

    let file =
        SegmentFile::open(&SegmentDir::new(fs.clone(), dir), &segments[0], shape, Access::Normal)
            .expect("open");
    file.warm_filter();
    let refused: Vec<u64> = (1..50).filter(|a| !file.may_contain(&account(*a))).collect();
    assert_eq!(refused, [], "accounts segment 0 holds, refused by its filter");
    let absent = 10_000u64;
    let passed = (1_000..1_000 + absent).filter(|a| file.may_contain(&account(*a))).count();
    assert!(passed < 200, "{passed} of {absent} absent accounts passed (≈ 39 expected)");
}

/// Probed row as a merge test writes it: `probed_row(n)`'s value, or its tombstone
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Held {
    Value(u32),
    Tombstone(u32),
}

/// One `Held` row group per key
///
/// - value + tombstone → both dropped, in either input order (counted as cancelled)
/// - all dropped → no output segment
/// - lone tombstone → kept (its value lives outside the merge)
/// - two values, two tombstones, three rows of one key → `Contract`, naming the rows
#[test]
fn a_merge_cancels_value_tombstone_pairs_keeps_lone_tombstones_and_refuses_other_duplicates() {
    use Held::{Tombstone, Value};
    struct Case {
        case: &'static str,
        inputs: &'static [&'static [Held]],
        merged: Result<(&'static [Held], u64), &'static str>,
    }
    #[rustfmt::skip]
    let cases = [
        Case { case: "pair, nothing left", inputs: &[&[Value(1)], &[Tombstone(1)]], merged: Ok((&[], 1)) },
        Case { case: "tombstone first",    inputs: &[&[Tombstone(1)], &[Value(1)]], merged: Ok((&[], 1)) },
        Case {
            case: "pair among others",
            inputs: &[&[Value(1), Value(2)], &[Tombstone(1), Value(3)]],
            merged: Ok((&[Value(2), Value(3)], 1)),
        },
        Case {
            case: "lone tombstone",
            inputs: &[&[Tombstone(1)], &[Value(2)]],
            merged: Ok((&[Tombstone(1), Value(2)], 0)),
        },
        Case { case: "two values",     inputs: &[&[Value(1)], &[Value(1)]],         merged: Err("key held 2 times: value, value") },
        Case { case: "two tombstones", inputs: &[&[Tombstone(1)], &[Tombstone(1)]], merged: Err("key held 2 times: tombstone, tombstone") },
        Case {
            case: "three rows",
            inputs: &[&[Value(1)], &[Tombstone(1)], &[Value(1)]],
            merged: Err("key held 3 times: value, tombstone, value"),
        },
    ];
    let shape = Shape::of(&probed().deletes());
    let slots = Slots::new(1, u64::MAX);
    let cancel = AtomicBool::new(false);
    for Case { case, inputs, merged: expected } in cases {
        let fs = SimFs::new();
        let dir = Path::new("/removable");
        fs.create_dir_all(dir).expect("dir");
        let writer = SegmentWriter::new(SegmentDir::new(fs.clone(), dir), shape);
        let segments: Vec<_> = (0u32..)
            .zip(inputs)
            .map(|(id, rows)| {
                let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = (rows.iter())
                    .map(|held| match *held {
                        Value(n) => (probed_row(n).0, Some(probed_row(n).1)),
                        Tombstone(n) => (probed_row(n).0, None),
                    })
                    .collect();
                let rows = rows.iter().map(|(key, value)| (key.as_slice(), value.as_deref()));
                writer.write(id, rows.collect()).expect("write").expect("rows")
            })
            .collect();
        let slot = slots.acquire(0, &cancel).expect("free slot");

        let merged = writer.merge(99, &segments, &cancel, &slot).map(|merged| {
            let merged = merged.expect("never cancelled");
            let rows: Vec<Held> = merged.output.map_or(Vec::new(), |meta| {
                let file = SegmentFile::open(
                    &SegmentDir::new(fs.clone(), dir),
                    &meta,
                    shape,
                    Access::Sequential,
                )
                .expect("output");
                (0..file.records())
                    .map(|slot| match file.entry(slot) {
                        (key, Some(value)) => {
                            assert_eq!(id(&key), id(&value), "{case}: id {}'s value", id(&key));
                            Value(id(&key))
                        }
                        (key, None) => Tombstone(id(&key)),
                    })
                    .collect()
            });
            (rows, merged.cancelled)
        });

        match (merged, expected) {
            (Ok((mut rows, cancelled)), Ok((expected, expected_cancelled))) => {
                rows.sort();
                let mut expected = expected.to_vec();
                expected.sort();
                let what = format!("{case}: (output rows sorted, pairs cancelled)");
                assert_eq!((rows, cancelled), (expected, expected_cancelled), "{what}");
            }
            (Err(SegmentError::Contract { segment: 99, reason }), Err(named)) => {
                assert!(reason.contains(named), "{case}: refused {reason:?}, want {named:?}");
            }
            (answer, expected) => panic!("{case}: merged {answer:?}, want {expected:?}"),
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
    let held = writer.write(0, borrowed(&[removed.clone(), kept.clone()])).expect("write");
    let held = held.expect("rows");
    let unrelated = writer.write(1, borrowed(&[other])).expect("write").expect("rows");
    let tombstone = writer.write(2, vec![(removed.0.as_slice(), None)]).expect("write");
    let tombstone = tombstone.expect("rows");
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
    // every segment kept on disk (`open` removes what its list leaves out), each list mapped next
    let every = [held, unrelated, tombstone, held_unrelated, unrelated_tombstone, held_tombstone];
    let mapped =
        SegmentsView::open(&SegmentDir::new(fs.clone(), dir), shape, &every).expect("open");

    let lists: [&[_]; 7] = [
        &[held, unrelated, tombstone],
        &[tombstone, unrelated, held],
        &[unrelated, tombstone, held],
        &[held_unrelated, tombstone],
        &[tombstone, held_unrelated],
        &[held, unrelated_tombstone],
        &[held_tombstone, unrelated],
    ];
    for listed in lists {
        let segments: Vec<u32> = listed.iter().map(|meta| meta.id).collect();
        let segments_view = mapped.next(&SegmentDir::new(fs.clone(), dir), listed).expect("map");
        let live_rows: i64 = listed.iter().map(SegmentMeta::live).sum();
        assert_eq!(live_rows, 2, "segments {segments:?}: live rows = ids 2 + 3");
        assert_eq!(segments_view.get(&removed.0), None, "segments {segments:?}: get id 1");
        let asked = [removed.0.as_slice(), kept.0.as_slice(), removed.0.as_slice()];
        let values: Vec<Option<u32>> =
            segments_view.get_many(&asked).iter().map(|value| value.as_deref().map(id)).collect();
        assert_eq!(values, [None, Some(2), None], "segments {segments:?}: get_many ids [1, 2, 1]");
        let range = |limit| {
            let rows = segments_view.range(&[0; 16], &[0xff; 16], limit)?;
            Some(rows.iter().map(|(key, _)| id(key)).collect::<Vec<_>>())
        };
        let what = format!("segments {segments:?}: ids in range, at most");
        assert_eq!(
            range(2).map(|mut ids| {
                ids.sort();
                ids
            }),
            Some(vec![2, 3]),
            "{what} 2 (live rows)"
        );
        assert_eq!(range(1), None, "{what} 1 (under the live rows)");
    }
}
