//! Map's segments below the store: shapes, open, corruption, seek arithmetic, prefetch plans,
//! scope filters, merges, the cursor (store end to end: the engine's tests)

use std::{
    collections::BTreeMap,
    num::NonZeroU32,
    ops::Bound::{self, Excluded, Included, Unbounded},
    path::Path,
    sync::{atomic::AtomicBool, Arc},
};

use bytes::Bytes;

use super::{
    file::{Prefetch, SegmentFile},
    file_name,
    layout::{Row, Shape},
    writer::SegmentWriter,
    Breach, Cursor, Lookup, SegmentError, SegmentLog, Slots, Snapshot, Tuning,
};
use crate::{
    fs::{Access, Fs, SimFs},
    pages::{sums_path, PageError},
};

fn width(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("non-zero")
}

/// `account ‖ seq → seq · 1000`, ranges read per account (scope = the account)
fn scanned() -> Shape {
    let tuning = Tuning {
        scope: 8,
        cache_writes: false,
    };
    Shape::of("scanned", width(12), width(8), false, tuning).expect("shape")
}

/// Hash-like id → position, point lookups only
fn probed(deletes: bool) -> Shape {
    Shape::of("probed", width(16), width(4), deletes, Tuning::default()).expect("shape")
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
    id[..8].copy_from_slice(
        &u64::from(n)
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .to_be_bytes(),
    );
    id[12..].copy_from_slice(&n.to_be_bytes());
    (id, n.to_be_bytes().to_vec())
}

fn borrowed(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<(&[u8], Option<&[u8]>)> {
    rows.iter()
        .map(|(key, value)| (key.as_slice(), Some(value.as_slice())))
        .collect()
}

fn slots() -> Arc<Slots> {
    Arc::new(Slots::new(4, u64::MAX))
}

/// Owned `(key, value)` rows
type Rows = Vec<(Vec<u8>, Vec<u8>)>;

/// Every live row the cursor walks, owned
fn walk(
    snapshot: &Arc<Snapshot>,
    lower: Bound<&[u8]>,
    upper: Bound<&[u8]>,
    reverse: bool,
) -> Result<Rows, SegmentError> {
    let mut cursor = Cursor::new(snapshot, lower, upper, reverse)?;
    let mut rows = Vec::new();
    while let Some((key, value)) = cursor.current()? {
        rows.push((key.to_vec(), value.to_vec()));
        cursor.advance()?;
    }
    Ok(rows)
}

fn lookup(snapshot: &Snapshot, key: &[u8]) -> Lookup {
    snapshot
        .lookup_with(key, |value, removed| match (value, removed) {
            (Some(value), _) => Lookup::Value(Bytes::copy_from_slice(value)),
            (None, true) => Lookup::Removed,
            (None, false) => Lookup::Absent,
        })
        .expect("lookup")
}

/// - a batch row of the wrong widths, or a tombstone without deletes, is refused (never written,
///   never a panic); the log keeps working after
#[test]
fn a_row_the_map_cannot_hold_is_refused() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new("/m")).expect("dir");
    let mut log =
        SegmentLog::open(fs, Path::new("/m"), "scanned", scanned(), &[], 2, slots()).expect("open");
    type Refused = (&'static [u8], Option<&'static [u8]>);
    let rows: [Refused; 3] = [
        (&[0; 11], Some(&[0; 8])),
        (&[0; 12], Some(&[0; 7])),
        (&[0; 12], None),
    ];
    for (key, value) in rows {
        let refused = log.batch(vec![(key, value)]);
        assert!(
            matches!(refused, Err(SegmentError::Internal(_))),
            "{refused:?}"
        );
    }
    let (key, value) = scanned_row(1, 1);
    log.batch(vec![(key.as_slice(), Some(value.as_slice()))])
        .expect("a fitting row");
    log.committed().expect("committed");
}

/// - open removes unlisted segments (+ checksums) + any writer's scratch; lost segment refused
/// - flipped committed byte passes open (lengths only), fails the first read / merge touching
///   its page with `Corrupt`
/// - sealed segments are read-only files
#[test]
fn open_checks_lengths_and_a_corrupt_page_fails_on_first_touch() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let shape = scanned();
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    // enough rows that page 0 holds records only (open reads the summary, past them)
    let rows = |owner| {
        (0..600)
            .map(move |seq| scanned_row(owner, seq))
            .collect::<Vec<_>>()
    };
    let kept = writer
        .write(0, borrowed(&rows(1)))
        .expect("write")
        .expect("rows");
    let orphan = writer
        .write(1, borrowed(&[scanned_row(3, 0)]))
        .expect("write")
        .expect("rows");
    // merge cut short by a crash leaves its scratch, even under a listed segment's id
    for leftover in ["0000000000.fences.scratch", "0000000009.filter.scratch"] {
        fs.open(&dir.join(leftover))
            .expect("scratch")
            .write_all_at(&[1; 9], 0)
            .expect("write");
    }
    writer.sync_dir().expect("sync");
    let path = dir.join(file_name(kept.id));
    assert!(
        fs.is_read_only(&path) && fs.is_read_only(&sums_path(&path)),
        "a sealed segment and its checksums are read-only"
    );
    assert!(
        fs.open_existing(&path).is_err(),
        "never reopened for writing"
    );

    Snapshot::open(fs.as_ref(), dir, shape, &[kept]).expect("open");
    let mut kept_files = vec![file_name(kept.id), format!("{}.crc", file_name(kept.id))];
    kept_files.sort();
    assert_eq!(
        fs.list(dir).expect("list"),
        kept_files,
        "unlisted segment {} removed",
        orphan.id
    );

    let original = fs.contents(&path).expect("segment bytes");
    fs.corrupt(&path, |bytes| bytes.truncate(10));
    let short = Snapshot::open(fs.as_ref(), dir, shape, &[kept]).map(|_| ());
    assert!(matches!(
        short,
        Err(SegmentError::Page(PageError::Lost { have: 10, .. }))
    ));

    fs.corrupt(&path, |bytes| {
        *bytes = original.clone();
        bytes[shape.stride + 9] ^= 1;
    });
    let snapshot = Snapshot::open(fs.as_ref(), dir, shape, &[kept]).expect("lengths intact");
    let corrupt = |result: Result<(), SegmentError>| {
        assert!(
            matches!(
                result,
                Err(SegmentError::Page(PageError::Corrupt { page: 0, .. }))
            ),
            "{result:?}"
        );
    };
    corrupt(snapshot.lookup_with(&scanned_row(1, 1).0, |_, _| ()));

    // same tier as `kept` at fanout 2 → opening both launches their merge (reads the page)
    let second = writer
        .write(2, borrowed(&rows(4)))
        .expect("write")
        .expect("rows");
    let mut log = SegmentLog::open(
        fs.clone(),
        dir,
        "scanned",
        shape,
        &[kept, second],
        2,
        slots(),
    )
    .expect("merge");
    log.settle();
    corrupt(log.batch(Vec::new()).map(drop));

    fs.remove(&sums_path(&path)).expect("remove checksums");
    let unsummed = Snapshot::open(fs.as_ref(), dir, shape, &[kept]).map(|_| ());
    assert!(matches!(
        unsummed,
        Err(SegmentError::Page(PageError::Lost { .. }))
    ));

    // last byte = filter fingerprint; whole filter read + checked at map → open fails before
    // any probe
    let ids = Path::new("/ids");
    fs.create_dir_all(ids).expect("dir");
    let id_shape = probed(false);
    let id_rows: Vec<_> = (0..3_000).map(probed_row).collect();
    let id_writer = SegmentWriter::open(fs.clone(), ids, id_shape);
    let written = id_writer
        .write(0, borrowed(&id_rows))
        .expect("write")
        .expect("rows");
    fs.corrupt(&ids.join(file_name(written.id)), |bytes| {
        *bytes.last_mut().expect("bytes") ^= 1
    });
    let opened = Snapshot::open(fs.as_ref(), ids, id_shape, &[written]).map(|_| ());
    let page = ((written.sealed.len - 1) / 4096) as usize;
    assert!(
        matches!(&opened, Err(SegmentError::Page(PageError::Corrupt { page: p, .. })) if *p == page),
        "a corrupt filter never answers: {opened:?}"
    );
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
    let shape = scanned();
    let Shape {
        stride,
        key_len,
        block_rows,
        group_fences,
        ..
    } = shape;
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
    assert_eq!(file.seek(&below).expect("seek"), 0, "below every key");
    assert_eq!(file.seek(&above).expect("seek"), rows, "above every key");
    let (blocks, fences_at) = (rows.div_ceil(block_rows), rows * stride);
    let edges = [rows - 1, group_rows - 1, group_rows, 2 * group_rows];
    for slot in (0..rows).step_by(997).chain(edges) {
        let seq = 2 * slot as u32;
        assert_eq!(
            file.seek(&key(seq)).expect("seek"),
            slot,
            "key at slot {slot}"
        );
        assert_eq!(
            file.seek(&key(seq + 1)).expect("seek"),
            slot + 1,
            "gap after slot {slot}"
        );

        let (block, group) = (slot / block_rows, slot / block_rows / group_fences);
        let group_blocks = group * group_fences..((group + 1) * group_fences).min(blocks);
        let fences =
            fences_at + group_blocks.start * key_len..fences_at + group_blocks.end * key_len;
        let records = block * block_rows * stride..((block + 1) * block_rows).min(rows) * stride;
        let prefetched = |step| file.prefetch_range(step, &key(seq)).expect("prefetch");
        assert_eq!(
            prefetched(Prefetch::Fences),
            fences,
            "slot {slot}: its fence group"
        );
        assert_eq!(
            prefetched(Prefetch::Records),
            records,
            "slot {slot}: its block of records"
        );
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
    let shape = probed(false);
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    let spans = [0..20_000u32, 20_000..40_000, 40_000..60_000];
    let metas: Vec<_> = (0u32..)
        .zip(&spans)
        .map(|(id, span)| {
            let rows: Vec<_> = span.clone().map(probed_row).collect();
            writer
                .write(id, borrowed(&rows))
                .expect("write")
                .expect("rows")
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
    let mut sorted: Vec<(&[u8], usize)> = keys
        .iter()
        .enumerate()
        .map(|(at, key)| (key.as_slice(), at))
        .collect();
    sorted.sort_unstable();
    let holder = |n: u32| {
        spans
            .iter()
            .position(|span| span.contains(&n))
            .expect("held")
    };

    for step in [Prefetch::Fences, Prefetch::Records] {
        let plan = snapshot.prefetch_plan(step, &sorted).expect("plan");
        for segment in 0..spans.len() {
            let ranges: Vec<_> = plan
                .iter()
                .filter(|(s, _)| *s == segment)
                .map(|(_, r)| r)
                .collect();
            let whole_pages = ranges
                .iter()
                .all(|r| r.start % 4096 == 0 && r.end % 4096 == 0);
            let disjoint = ranges.windows(2).all(|pair| pair[0].end < pair[1].start);
            assert!(
                whole_pages && disjoint,
                "{step:?}, segment {segment}: {ranges:?}"
            );
        }
        for (&n, key) in asked.iter().zip(&keys) {
            let segment = holder(n);
            let wanted = files[segment].prefetch_range(step, key).expect("range");
            let covered = plan.iter().any(|(s, range)| {
                *s == segment && range.start <= wanted.start && wanted.end <= range.end
            });
            assert!(
                covered,
                "{step:?}: id {n}'s {wanted:?} in segment {segment}"
            );
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
    let shape = scanned();
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    // account 0 in every segment, accounts 100·s + 1 .. 100·s + 49 only in segment s
    let segments: Vec<_> = (0u64..6)
        .map(|s| {
            let accounts = std::iter::once(0).chain(100 * s + 1..100 * s + 50);
            let rows: Vec<_> = accounts
                .flat_map(|a| (0..20).map(move |seq| scanned_row(a, seq + 100 * s as u32)))
                .collect();
            writer
                .write(s as u32, borrowed(&rows))
                .expect("write")
                .expect("rows")
        })
        .collect();
    writer.sync_dir().expect("sync");

    let snapshot = Arc::new(Snapshot::open(fs.as_ref(), dir, shape, &segments).expect("open"));
    let range = |a: u64| {
        let (start, end) = (account(a).to_vec(), [&account(a)[..], &[0xff; 4]].concat());
        walk(&snapshot, Included(&start), Excluded(&end), false).expect("walk")
    };
    assert_eq!(
        range(0).len(),
        6 * 20,
        "the shared account from every segment"
    );
    assert_eq!(range(301).len(), 20, "an account from one segment");
    assert!(range(7_777).is_empty(), "an account no segment holds");

    let file =
        SegmentFile::open(fs.as_ref(), dir, &segments[0], shape, Access::Normal).expect("open");
    file.warm_filter().expect("warm");
    assert!(
        (1..50).all(|a| file.may_contain(&account(a)).expect("probe")),
        "no false negative"
    );
    let absent = 10_000u64;
    let passed = (1_000..1_000 + absent)
        .filter(|a| file.may_contain(&account(*a)).expect("probe"))
        .count();
    assert!(
        passed < 200,
        "{passed} of {absent} absent accounts passed (≈ 39 expected)"
    );
}

/// Merge of `deletes` segments, one row group per key (row `(n, live)` = `probed_row(n)`, its
/// value if `live`, else its tombstone; expected = output rows + pairs cancelled):
///
/// - value + tombstone → both dropped, in either input order; all dropped → no output segment
/// - lone tombstone → kept (its value lives outside the merge)
/// - two values, two tombstones, three rows of one key → `Contract`, naming the inputs holding it
#[test]
fn a_merge_cancels_value_tombstone_pairs_keeps_lone_tombstones_and_refuses_other_duplicates() {
    type Rows = &'static [(u32, bool)];
    type Merged = Result<(Rows, u64), usize>;
    let cases: [(&str, &[Rows], Merged); 7] = [
        (
            "pair, nothing left",
            &[&[(1, true)], &[(1, false)]],
            Ok((&[], 1)),
        ),
        (
            "tombstone first",
            &[&[(1, false)], &[(1, true)]],
            Ok((&[], 1)),
        ),
        (
            "pair among others",
            &[&[(1, true), (2, true)], &[(1, false), (3, true)]],
            Ok((&[(2, true), (3, true)], 1)),
        ),
        (
            "lone tombstone",
            &[&[(1, false)], &[(2, true)]],
            Ok((&[(1, false), (2, true)], 0)),
        ),
        ("two values", &[&[(1, true)], &[(1, true)]], Err(2)),
        ("two tombstones", &[&[(1, false)], &[(1, false)]], Err(2)),
        (
            "three rows",
            &[&[(1, true)], &[(1, false)], &[(1, true)]],
            Err(3),
        ),
    ];
    let shape = probed(true);
    let slots = Slots::new(1, u64::MAX);
    let cancel = AtomicBool::new(false);
    for (case, inputs, expected) in cases {
        let fs = SimFs::new();
        let dir = Path::new("/removable");
        fs.create_dir_all(dir).expect("dir");
        let writer = SegmentWriter::open(fs.clone(), dir, shape);
        let segments: Vec<_> = (0u32..)
            .zip(inputs)
            .map(|(id, rows)| {
                let rows: Vec<_> = rows
                    .iter()
                    .map(|&(n, live)| (probed_row(n), live))
                    .collect();
                let rows = rows
                    .iter()
                    .map(|((key, value), live)| (key.as_slice(), live.then_some(value.as_slice())));
                writer
                    .write(id, rows.collect())
                    .expect("write")
                    .expect("rows")
            })
            .collect();
        let slot = slots.acquire(0, &cancel).expect("free slot");
        let merged = writer.merge(99, &segments, &cancel, &slot).map(|merged| {
            let merged = merged.expect("never cancelled");
            let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> = merged.output.map_or(Vec::new(), |meta| {
                let file = SegmentFile::open(fs.as_ref(), dir, &meta, shape, Access::Sequential)
                    .expect("output");
                (0..file.records())
                    .map(|slot| match file.content(slot).expect("row") {
                        (key, Row::Value(value)) => (key.to_vec(), Some(value.to_vec())),
                        (key, Row::Tombstone) => (key.to_vec(), None),
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
            (Err(SegmentError::Contract { segments, breach }), Err(held)) => {
                let all: Vec<u32> = (0u32..).take(inputs.len()).collect();
                assert_eq!(segments, all, "{case}: every input holding the key");
                assert_eq!(breach, Breach::Repeated { held }, "{case}");
            }
            (answer, expected) => panic!("{case}: {answer:?}, expected {expected:?}"),
        }
    }
}

/// Value in one segment, its tombstone in a tombstone-only other, an unrelated third: whatever
/// the list order, and after a merge of any two (adjacent or not), the key reads removed through
/// `lookup_with` and `get_many`, and absent from every walk
#[test]
fn a_removed_key_stays_absent_under_every_segment_order_and_merge() {
    let fs = SimFs::new();
    let dir = Path::new("/removable");
    fs.create_dir_all(dir).expect("dir");
    let shape = probed(true);
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    let (removed, kept, other) = (probed_row(1), probed_row(2), probed_row(3));
    let held_rows = [removed.clone(), kept.clone()];
    let held = writer
        .write(0, borrowed(&held_rows))
        .expect("write")
        .expect("rows");
    let unrelated = writer
        .write(1, borrowed(std::slice::from_ref(&other)))
        .expect("write")
        .expect("rows");
    let tombstone = writer
        .write(2, vec![(removed.0.as_slice(), None)])
        .expect("write")
        .expect("rows");
    let slots = Slots::new(1, u64::MAX);
    let cancel = AtomicBool::new(false);
    let merge = |id, inputs: &[_]| {
        let slot = slots.acquire(0, &cancel).expect("free slot");
        let merged = writer
            .merge(id, inputs, &cancel, &slot)
            .expect("merge")
            .expect("finished");
        merged.output.expect("rows left")
    };
    let held_unrelated = merge(3, &[held, unrelated]);
    let unrelated_tombstone = merge(4, &[unrelated, tombstone]);
    let held_tombstone = merge(5, &[held, tombstone]);
    writer.sync_dir().expect("sync");

    let mut live = vec![
        (kept.0.clone(), kept.1.clone()),
        (other.0.clone(), other.1.clone()),
    ];
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
    let every = [
        held,
        unrelated,
        tombstone,
        held_unrelated,
        unrelated_tombstone,
        held_tombstone,
    ];
    let mapped = Snapshot::open(fs.as_ref(), dir, shape, &every).expect("open");
    for listed in lists {
        let ids: Vec<u32> = listed.iter().map(|meta| meta.id).collect();
        let snapshot = Arc::new(mapped.next(fs.as_ref(), dir, listed).expect("map"));
        let expected_removed = match listed {
            [_, _] if listed.contains(&held_tombstone) => Lookup::Absent,
            _ => Lookup::Removed,
        };
        assert_eq!(
            lookup(&snapshot, &removed.0),
            expected_removed,
            "{ids:?}: lookup"
        );
        let asked = [
            removed.0.as_slice(),
            kept.0.as_slice(),
            removed.0.as_slice(),
        ];
        let expected = vec![
            expected_removed.clone(),
            Lookup::Value(Bytes::from(kept.1.clone())),
            expected_removed.clone(),
        ];
        assert_eq!(
            snapshot.get_many(&asked).expect("get_many"),
            expected,
            "{ids:?}: get_many"
        );
        let forward = walk(&snapshot, Unbounded, Unbounded, false).expect("walk");
        assert_eq!(forward, live, "{ids:?}: forward walk");
        let mut reverse = walk(&snapshot, Unbounded, Unbounded, true).expect("walk");
        reverse.reverse();
        assert_eq!(reverse, live, "{ids:?}: reverse walk");
    }
}

/// Cursor over interleaved segments (keys unique across them), every bound kind, both
/// directions, against a sorted model; resumed after any prefix = the rest of the full walk
#[test]
fn a_cursor_walks_every_range_in_order_across_segments() {
    let fs = SimFs::new();
    let dir = Path::new("/walk");
    fs.create_dir_all(dir).expect("dir");
    let shape = scanned();
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    let mut model = BTreeMap::new();
    let segments: Vec<_> = (0u32..5)
        .map(|s| {
            // segment s holds seqs ≡ s (mod 5): every neighbour lives in another segment
            let rows: Vec<_> = (0..120u32)
                .filter(|seq| seq % 5 == s)
                .map(|seq| scanned_row(1, seq))
                .collect();
            model.extend(rows.iter().cloned());
            writer
                .write(s, borrowed(&rows))
                .expect("write")
                .expect("rows")
        })
        .collect();
    writer.sync_dir().expect("sync");
    let snapshot = Arc::new(Snapshot::open(fs.as_ref(), dir, shape, &segments).expect("open"));

    let key = |seq: u32| scanned_row(1, seq).0;
    type Bounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);
    let bounds: Vec<Bounds> = vec![
        (Unbounded, Unbounded),
        (Included(key(10)), Included(key(50))),
        (Excluded(key(10)), Excluded(key(50))),
        (Included(key(17)), Unbounded),
        (Unbounded, Excluded(key(3))),
        (Included(key(60)), Included(key(40))),
        (Excluded(key(30)), Included(key(30))),
        (
            Included(account(1).to_vec()),
            Excluded([&account(1)[..], &[0xff; 4]].concat()),
        ),
        (Included(account(2).to_vec()), Unbounded),
    ];
    for (lower, upper) in bounds {
        fn as_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
            bound.as_ref().map(Vec::as_slice)
        }
        let expected: Vec<_> = model
            .iter()
            .filter(|(key, _)| {
                let key = key.as_slice();
                let above = match as_ref(&lower) {
                    Included(low) => key >= low,
                    Excluded(low) => key > low,
                    Unbounded => true,
                };
                let below = match as_ref(&upper) {
                    Included(high) => key <= high,
                    Excluded(high) => key < high,
                    Unbounded => true,
                };
                above && below
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let forward = walk(&snapshot, as_ref(&lower), as_ref(&upper), false).expect("walk");
        assert_eq!(forward, expected, "{lower:?}..{upper:?} forward");
        let mut reverse = walk(&snapshot, as_ref(&lower), as_ref(&upper), true).expect("walk");
        reverse.reverse();
        assert_eq!(reverse, expected, "{lower:?}..{upper:?} reverse");
    }

    // one cursor, advanced part way, then a merge lands under a newer snapshot: it keeps going
    let mut cursor = Cursor::new(&snapshot, Unbounded, Unbounded, false).expect("cursor");
    for _ in 0..37 {
        cursor.advance().expect("advance");
    }
    let mut log = SegmentLog::open(fs.clone(), dir, "scanned", shape, &segments, 5, slots())
        .expect("merging");
    log.settle();
    log.batch(Vec::new()).expect("land");
    log.committed().expect("swap");
    let mut rest = Vec::new();
    while let Some((key, value)) = cursor.current().expect("row") {
        rest.push((key.to_vec(), value.to_vec()));
        cursor.advance().expect("advance");
    }
    let full: Vec<_> = model.into_iter().collect();
    assert_eq!(rest, full[37..], "resumed: the rest, on its own snapshot");
}

/// Without deletes, a key in two committed segments is a contract breach: the walk fails
#[test]
fn a_key_in_two_segments_fails_the_walk() {
    let fs = SimFs::new();
    let dir = Path::new("/twice");
    fs.create_dir_all(dir).expect("dir");
    let shape = scanned();
    let writer = SegmentWriter::open(fs.clone(), dir, shape);
    let rows = [scanned_row(1, 1)];
    let first = writer
        .write(0, borrowed(&rows))
        .expect("write")
        .expect("rows");
    let second = writer
        .write(1, borrowed(&rows))
        .expect("write")
        .expect("rows");
    let snapshot =
        Arc::new(Snapshot::open(fs.as_ref(), dir, shape, &[first, second]).expect("open"));
    let walked = walk(&snapshot, Unbounded, Unbounded, false);
    assert!(
        matches!(&walked, Err(SegmentError::Contract { segments, breach: Breach::Repeated { held: 2 } }) if segments.len() == 2 && segments.contains(&0) && segments.contains(&1)),
        "{walked:?}"
    );
}

/// Keys shorter than 8 bytes: written, merged, found and walked like any other
#[test]
fn short_keys_write_merge_and_read() {
    let fs = SimFs::new();
    let dir = Path::new("/short");
    fs.create_dir_all(dir).expect("dir");
    let shape = Shape::of("short", width(2), width(1), false, Tuning::default()).expect("shape");
    let rows = |range: std::ops::Range<u16>| -> Vec<(Vec<u8>, Vec<u8>)> {
        range
            .map(|n| (n.to_be_bytes().to_vec(), vec![n as u8]))
            .collect()
    };
    let mut log = SegmentLog::open(fs.clone(), dir, "short", shape, &[], 2, slots()).expect("open");
    for range in [0..500u16, 500..1_000] {
        let batch = rows(range);
        log.batch(borrowed(&batch)).expect("batch");
        log.committed().expect("committed");
    }
    log.settle();
    log.batch(Vec::new()).expect("land the merge");
    log.committed().expect("committed");
    assert_eq!(log.segments().len(), 1, "merged into one");
    let snapshot = Arc::clone(log.snapshot());
    assert_eq!(
        lookup(&snapshot, &700u16.to_be_bytes()),
        Lookup::Value(Bytes::from(vec![700u16 as u8]))
    );
    let wrong = snapshot.lookup_with(&[0xff], |_, _| ());
    assert!(
        matches!(wrong, Err(SegmentError::Internal(_))),
        "a wrong-width key: {wrong:?}"
    );
    let walked = walk(&snapshot, Unbounded, Unbounded, false).expect("walk");
    assert_eq!(walked, rows(0..1_000));
}
