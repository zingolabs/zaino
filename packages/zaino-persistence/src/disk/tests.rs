//! `DiskEngine` files-only concerns on `SimFs` (random histories: `model_tests.rs`): every crash
//! state, every failed I/O call, open's trimming + refusals, golden manifest bytes, offline
//! verify, each guard's panic

use std::{num::NonZeroU32, path::Path, sync::Arc};

use zcash_protocol::consensus::NetworkType;

use super::model::{
    assert_tiers, block_at, block_record, open, probed_key, scanned_key, sim_fs, with_flush, Model,
    Writes, BLOCKS, HEIGHTS, PROBED, ROOT, SCANNED, SCHEMA, TABLES,
};
use super::*;
use crate::{
    fs::{RealFs, SimFs},
    golden::assert_fields,
    schema::{IndexKind, MapTable, SequenceTable, Tables, Width},
    sequence::Records,
};

#[test]
fn a_store_at_the_smallest_write_buffer_commits_inside_apply() {
    let mut store = DiskEngine::with_fanout(SimFs::new(), 2)
        .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MIN)
        .expect("open");
    let mut changes = store.changes(block_at(0));
    changes.sequence(BLOCKS).append(&[7]);

    store.apply(changes);

    assert_eq!(store.durable().tip(), Some(block_at(0)), "apply committed: the durable tip");
    assert_eq!(store.buffered_bytes(), 0, "apply committed: nothing left buffered");
}

/// Fanout 2, merges launched + landed mid-history, one value + tombstone pair cancelled, per-file
/// fsync and one-flush (`Barrier`) commits
///
/// - per crash state: recovered = an acked or the attempted commit
/// - every table = the model at that commit (a removed row never back), only listed files
/// - commits continue
#[test]
fn every_crash_state_reopens_to_exactly_an_acknowledged_or_the_attempted_commit() {
    for flush_device in [false, true] {
        crash_states_reopen_to_a_commit(flush_device);
    }
}

fn crash_states_reopen_to_a_commit(flush_device: bool) {
    let (fs, recording) = SimFs::recording();
    let fs = with_flush(fs, flush_device);
    // scanned seqs: commit 1 issues 1-3, commit 2 issues 4-5
    let commits = [
        Writes { records: 2, owners: vec![1, 2, 1], ids: 2, removed: vec![] },
        Writes { records: 1, owners: vec![1, 3], ids: 1, removed: vec![1] },
        Writes { records: 0, owners: vec![], ids: 1, removed: vec![2, 4] },
        Writes { records: 3, owners: vec![4], ids: 0, removed: vec![] },
    ];
    let mut models = vec![Model::default()];
    {
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        for (acked, writes) in (1u64..).zip(&commits) {
            store.settle();
            let mut model = models.last().expect("seeded").clone();
            store.apply(model.advance(writes));
            store.commit().expect("commit");
            models.push(model);
            fs.set_tag(acked);
        }
        let segments = |map: usize| -> Vec<(u64, u64)> {
            store.maps[map].segments().iter().map(|meta| (meta.records, meta.tombstones)).collect()
        };
        assert_eq!(
            segments(0),
            [(4, 0), (2, 2), (1, 0)],
            "scanned (records, tombstones) per segment: commits 1 + 2 merged (seq 1's pair \
             cancelled), commit 3's tombstones, commit 4's row"
        );
        assert_eq!(
            segments(1),
            [(2, 0), (2, 0)],
            "probed (records, tombstones) per segment: commit 1, commits 2 + 3 merged"
        );
    }

    let states = recording.crash_states();
    assert!(states.len() > 50, "enumerated only {} crash states", states.len());
    for state in states {
        let acked = usize::try_from(state.tag).expect("tag = commits acknowledged");
        let label = &format!("flush_device {flush_device}, {}, {acked} acknowledged", state.label);
        let mut store = DiskEngine::with_fanout(state.fs.clone(), 2)
            .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX)
            .unwrap_or_else(|error| panic!("{label}: open: {error}"));
        let recovered = store.durable().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        assert!(
            [acked, acked + 1].contains(&recovered),
            "{label}: recovered {recovered} commits, want {acked} or the attempted {}",
            acked + 1
        );
        models[recovered].assert_view(&store.durable(), label);
        assert_tiers(&store, &state.fs, 2, true, label);

        let mut model = models[recovered].clone();
        store.apply(model.advance(&Writes {
            records: 1,
            owners: vec![9],
            ids: 1,
            removed: vec![],
        }));
        store.commit().unwrap_or_else(|error| panic!("{label}: commit after recovery: {error}"));
        model.assert_view(&store.durable(), &format!("{label}: commit after recovery"));
    }
}

/// Op 0, 1, 2, … failed until the workload succeeds (Pebble `errorfs`), per-file fsync and
/// one-flush (`Barrier`) commits (a failed `write_back` included)
///
/// - each failure = the injected `Err` (never a panic, never swallowed)
/// - restart = acked or attempted commit
#[test]
fn every_failed_io_call_surfaces_poisons_the_store_and_recovers_to_a_durable_state() {
    for flush_device in [false, true] {
        failed_io_surfaces_and_recovers(flush_device);
    }
}

fn failed_io_surfaces_and_recovers(flush_device: bool) {
    // - scanned seqs: commit 1 issues 1-2, commit 2 issues 3, commit 3 issues 4
    // - commit 4: no map rows (launches no merge, surfaces a failed one commit 3 launched)
    let commits = [
        Writes { records: 2, owners: vec![1, 2], ids: 1, removed: vec![] },
        Writes { records: 1, owners: vec![1], ids: 1, removed: vec![1] },
        Writes { records: 1, owners: vec![3], ids: 2, removed: vec![3] },
        Writes::default(),
    ];
    let mut models = vec![Model::default()];
    for writes in &commits {
        let mut model = models.last().expect("seeded").clone();
        model.advance(writes);
        models.push(model);
    }

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 10_000, "workload never succeeded");
        let fs = sim_fs(flush_device);
        fs.fail_from(fail_at);
        let label = format!("flush_device {flush_device}, op {fail_at} failed");

        let (acked, error) = match commit_all(&fs, &models, &commits) {
            Ok(()) => {
                let ops = fs.mutations();
                assert!(ops <= fail_at, "{label}: every commit succeeded, yet {ops} ops ran");
                break;
            }
            Err(failed) => failed,
        };
        failures += 1;
        assert!(error.contains("injected EIO"), "{label}: surfaced {error:?}, want the injection");

        let fs = fs.restarted();
        let mut store = DiskEngine::with_fanout(fs.clone(), 2)
            .open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX)
            .unwrap_or_else(|error| panic!("{label}: reopen, {acked} acknowledged: {error}"));
        let recovered = store.durable().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        assert!(
            [acked, acked + 1].contains(&recovered),
            "{label}: recovered {recovered} commits, want {acked} or the attempted {}",
            acked + 1
        );
        models[recovered].assert_view(&store.durable(), &label);
        store.apply(models[recovered].clone().advance(&Writes { records: 1, ..Writes::default() }));
        store.commit().unwrap_or_else(|error| panic!("{label}: commit after restart: {error}"));
    }
    assert!(failures > 40, "only {failures} failure points exercised");
}

/// Opens a store on `fs`, commits `commits` in turn (settled first: merges land mid-run)
///
/// - `Err((acked, error))` = first failure: open's, a commit's, or a merge's (surfaced by the
///   commit after it)
fn commit_all(
    fs: &Arc<SimFs>,
    models: &[Model],
    commits: &[Writes],
) -> Result<(), (usize, String)> {
    let engine = DiskEngine::with_fanout(fs.clone(), 2);
    let mut store = (engine.open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX))
        .map_err(|error| (0, error.to_string()))?;
    for (acked, writes) in commits.iter().enumerate() {
        store.settle();
        store.apply(models[acked].clone().advance(writes));
        store.commit().map_err(|error| (acked, error.to_string()))?;
    }
    Ok(())
}

/// Read 0, 1, 2, … failed while reopening a durable store until the open succeeds
///
/// - each failure = the injected `Err`, never a panic
/// - successful open finds every commit
#[test]
fn every_failed_read_at_open_surfaces_and_a_clean_open_finds_every_commit() {
    let fs = SimFs::new();
    let mut model = Model::default();
    {
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        for owner in 0..3u8 {
            store.apply(model.advance(&Writes {
                records: 2,
                owners: vec![owner],
                ids: 1,
                removed: vec![],
            }));
            store.commit().expect("commit");
        }
    }

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 1_000, "open never succeeded");
        let fs = fs.restarted();
        fs.fail_reads_from(fail_at);
        let engine = DiskEngine::with_fanout(fs.clone(), 2);
        match engine.open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX) {
            Ok(store) => {
                model.assert_view(&store.durable(), &format!("read {fail_at} failed, reopened"));
                break;
            }
            Err(error) => {
                let error = error.to_string();
                assert!(error.contains("injected read EIO"), "read {fail_at}: surfaced {error:?}");
                failures += 1;
            }
        }
    }
    assert!(failures > 0, "open read nothing through positional reads (the manifest at least)");
}

// guards: each fires on the bug it guards, naming it (RocksDB: never seen firing = not known to work)

#[test]
#[should_panic(expected = "heights: a 7-byte item, width 8")]
fn a_sequence_item_off_its_width_panics() {
    BlockChanges::new(block_at(0), SCHEMA).sequence(HEIGHTS).append(&[0; 7]);
}

#[test]
#[should_panic(expected = "scanned: a 11-byte item, width 12")]
fn a_map_key_off_its_width_panics() {
    BlockChanges::new(block_at(0), SCHEMA).map(SCANNED).insert(&[0; 11], &[0; 8]);
}

#[test]
#[should_panic(expected = "compact_block: sequence stray not in its schema")]
fn a_write_to_a_sequence_outside_the_schema_panics() {
    let stray = SequenceTable::new(3, "stray", Width::Variable);
    BlockChanges::new(block_at(0), SCHEMA).sequence(stray).append(&[]);
}

#[test]
#[should_panic(expected = "compact_block: map probed not in its schema")]
fn a_write_to_a_map_outside_the_schema_panics() {
    let stray = MapTable::point(1, "probed", 16, 8);
    BlockChanges::new(block_at(0), SCHEMA).map(stray).insert(&[0; 16], &[0; 8]);
}

#[test]
#[should_panic(expected = "compact_block: map probed not in its schema")]
fn a_read_of_a_map_outside_the_schema_panics() {
    let stray = MapTable::point(1, "probed", 16, 8);
    open(&DiskEngine::with_fanout(SimFs::new(), 2)).durable().map(stray).value(&[0; 16]);
}

#[test]
#[should_panic(expected = "sequence id != its position")]
fn tables_with_a_sequence_id_off_its_position_panic() {
    static SKIPPED: [SequenceTable; 1] = [SequenceTable::new(1, "skipped", Width::Variable)];
    Tables::new(&SKIPPED, &[]);
}

#[test]
#[should_panic(expected = "changes built for another schema")]
fn changes_for_another_schema_panic() {
    let other = Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, TABLES);
    open(&DiskEngine::with_fanout(SimFs::new(), 2)).apply(BlockChanges::new(block_at(0), other));
}

#[test]
#[should_panic(expected = "apply at height 0, not above the last applied Some(BlockHeight(0))")]
fn apply_at_a_height_not_above_the_last_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    store.apply(store.changes(block_at(0)));
    store.commit().expect("commit");
    store.apply(store.changes(block_at(0)));
}

#[test]
#[should_panic(expected = "scanned: a map key inserted twice")]
fn a_key_inserted_twice_by_one_block_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut changes = store.changes(block_at(0));
    changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
    changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
    store.apply(changes);
}

#[test]
#[should_panic(expected = "scanned: a map key removed twice")]
fn a_key_removed_twice_by_one_block_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut changes = store.changes(block_at(0));
    changes.map(SCANNED).remove(&scanned_key(1, 0));
    changes.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(changes);
}

#[test]
#[should_panic(expected = "scanned: a map key inserted and removed by one block")]
fn a_key_inserted_and_removed_by_one_block_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut changes = store.changes(block_at(0));
    changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
    changes.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(changes);
}

#[test]
#[should_panic(expected = "probed: a map key held twice")]
fn a_key_the_buffer_holds_inserted_again_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut first = store.changes(block_at(0));
    first.map(PROBED).insert(&probed_key(0), &[0; 4]);
    store.apply(first);
    let mut again = store.changes(block_at(1));
    again.map(PROBED).insert(&probed_key(0), &[1; 4]);
    store.apply(again);
}

#[test]
#[should_panic(expected = "scanned: a map key removed twice")]
fn a_key_the_buffer_removed_removed_again_panics() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let mut first = store.changes(block_at(0));
    first.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(first);
    let mut again = store.changes(block_at(1));
    again.map(SCANNED).remove(&scanned_key(1, 0));
    store.apply(again);
}

#[test]
#[should_panic(expected = "probed: remove on a table without `deletes()`")]
fn a_removal_from_a_table_without_deletes_panics() {
    BlockChanges::new(block_at(0), SCHEMA).map(PROBED).remove(&probed_key(0));
}

// same key inserted by two commits: neither layer above durable can see the first

#[test]
#[should_panic(expected = "a key held 2 times across durable segments")]
fn an_insert_only_key_in_two_durable_segments_panics_on_read() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    for height in [0, 1] {
        let mut changes = store.changes(block_at(height));
        changes.map(PROBED).insert(&probed_key(0), &[0; 4]);
        store.apply(changes);
        store.commit().expect("commit");
    }
    store.durable().map(PROBED).range(&[0; 16], &[0xff; 16], usize::MAX);
}

#[test]
#[should_panic(expected = "a key held 2 times across durable segments")]
fn a_deletes_key_held_in_two_durable_segments_panics_on_read() {
    let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
    for height in [0, 1] {
        let mut changes = store.changes(block_at(height));
        changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
        store.apply(changes);
        store.commit().expect("commit");
    }
    store.durable().map(SCANNED).range(&[0; 12], &[0xff; 12], usize::MAX);
}

/// - two 1-row segments at fanout 2 = a merge
/// - its duplicate = the merge's `Contract` error, surfaced at the next commit (either table kind)
#[test]
fn a_merge_of_a_key_held_twice_fails_the_next_commit() {
    #[rustfmt::skip]
    let tables = [
        (PROBED,  probed_key(0),     vec![0; 4]),
        (SCANNED, scanned_key(1, 0), vec![0; 8]),
    ];
    for (table, key, value) in tables {
        let mut store = open(&DiskEngine::with_fanout(SimFs::new(), 2));
        for height in [0, 1] {
            let mut changes = store.changes(block_at(height));
            changes.map(table).insert(&key, &value);
            store.apply(changes);
            store.commit().expect("commit");
        }
        store.settle();
        store.apply(store.changes(block_at(2)));

        let refused = store.commit().expect_err("a merge of two values of one key").to_string();

        let name = table.name;
        assert!(refused.contains("key held 2 times: value, value"), "{name}: refused {refused:?}");
    }
}

#[test]
#[should_panic(expected = "commit on a store a failed commit poisoned")]
fn a_commit_after_a_failed_one_panics() {
    let fs = SimFs::new();
    let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
    fs.fail_from(fs.mutations());
    let mut changes = store.changes(block_at(0));
    changes.sequence(BLOCKS).append(&[1]);
    store.apply(changes);
    store.commit().expect_err("injected EIO");
    let _ = store.commit();
}

#[test]
#[should_panic(expected = "apply on a store a failed commit poisoned")]
fn an_apply_after_a_failed_commit_panics() {
    let fs = SimFs::new();
    let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
    fs.fail_from(fs.mutations());
    let mut changes = store.changes(block_at(0));
    changes.sequence(BLOCKS).append(&[1]);
    store.apply(changes);
    store.commit().expect_err("injected EIO");
    store.apply(store.changes(block_at(1)));
}

#[test]
fn open_drops_bytes_past_the_manifest() {
    let fs = SimFs::new();
    let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
    store.apply(Model::default().advance(&Writes { records: 4, ..Writes::default() }));
    store.commit().expect("commit");
    drop(store);
    let blocks = Path::new(ROOT).join("blocks.dat");
    fs.corrupt(&blocks, |bytes| bytes.extend_from_slice(&[0xa5; 64]));

    let store = open(&DiskEngine::with_fanout(fs.clone(), 2));

    let len = fs.contents(&blocks).expect("blocks.dat").len();
    assert_eq!(len, 6, "blocks.dat truncated to records 0..4 (0 + 1 + 2 + 3 bytes)");
    let records = store.durable().sequence(BLOCKS).records(0..4);
    assert_eq!(records, (0..4).map(block_record).collect::<Vec<_>>(), "durable records intact");
}

/// Store of one commit (4 `blocks` records = 8 `pool/nodes` records), one file damaged
#[test]
fn open_refuses_a_lost_or_torn_durable_file_naming_it() {
    struct Damaged {
        file: &'static str,
        damage: fn(&mut Vec<u8>),
        refused: &'static str,
    }
    let cases = [
        Damaged {
            file: "pool/nodes.dat",
            damage: |bytes| bytes.truncate(10),
            refused: "/idx/pool/nodes.dat is 10 bytes, the durable state needs 32",
        },
        Damaged {
            file: "blocks.idx",
            damage: |bytes| bytes[3] ^= 1,
            refused: "/idx/blocks.idx: tail page fails its checksum",
        },
    ];
    for Damaged { file, damage, refused } in cases {
        let fs = SimFs::new();
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        store.apply(Model::default().advance(&Writes { records: 4, ..Writes::default() }));
        store.commit().expect("commit");
        drop(store);
        fs.corrupt(&Path::new(ROOT).join(file), damage);

        let opened =
            DiskEngine::with_fanout(fs, 2).open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX);

        let error = opened.map(|_| ()).expect_err(file).to_string();
        assert_eq!(error, refused, "{file} damaged");
    }
}

#[test]
fn open_refuses_data_in_a_directory_that_never_committed() {
    let fs = SimFs::new();
    fs.create_dir_all(Path::new(ROOT)).expect("dir");
    let heights = fs.open(&Path::new(ROOT).join("heights.dat")).expect("file").file;
    heights.write_all_at(&[1], 0).expect("write");

    let opened = DiskEngine::with_fanout(fs, 2).open(Path::new(ROOT), &SCHEMA, NonZeroUsize::MAX);

    let error = opened.map(|_| ()).expect_err("data with no manifest").to_string();
    assert_eq!(error, "/idx/heights.dat holds data but the directory has no MANIFEST");
}

/// Body = durable tip ‖ each sequence's seals in schema order (two for a variable one) ‖ each
/// map's segment list (seal `n` = len `n`, tail `n << 8`, sums `n << 16`)
#[test]
fn a_manifest_body_is_its_golden_bytes() {
    const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(4));
    const VARIABLE: SequenceTable = SequenceTable::new(1, "variable", Width::Variable);
    const MAP: MapTable = MapTable::point(0, "map", 8, 1);
    const TABLES: Tables = Tables::new(&[FIXED, VARIABLE], &[MAP]);
    let schema = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, TABLES);
    let seal = |n: u32| Sealed { len: u64::from(n), tail: n << 8, sums: n << 16 };
    let body = Body {
        durable: DurableTip { tip: Some(block_at(2)) },
        sequences: vec![
            Seals { data: seal(4), records: Records::Fixed(NonZeroU32::new(4).expect("4")) },
            Seals { data: seal(5), records: Records::Variable(seal(8)) },
        ],
        maps: vec![vec![SegmentMeta { id: 7, records: 2, tombstones: 1, sealed: seal(9) }]],
    };
    #[rustfmt::skip]
    let golden: [(&str, &[u8]); 15] = [
        ("durable block count",       &3u64.to_le_bytes()),
        ("durable tip hash",          &[2; 32]),
        ("fixed: data len",           &4u64.to_le_bytes()),
        ("fixed: data tail",          &0x400u32.to_le_bytes()),
        ("fixed: data sums",          &0x4_0000u32.to_le_bytes()),
        ("variable: data len",        &5u64.to_le_bytes()),
        ("variable: data tail",       &0x500u32.to_le_bytes()),
        ("variable: data sums",       &0x5_0000u32.to_le_bytes()),
        ("variable: ends len",        &8u64.to_le_bytes()),
        ("variable: ends tail",       &0x800u32.to_le_bytes()),
        ("variable: ends sums",       &0x8_0000u32.to_le_bytes()),
        ("map: segment count",        &1u32.to_le_bytes()),
        ("map: segment 0 id",         &7u32.to_le_bytes()),
        ("map: segment 0 records",    &2u64.to_le_bytes()),
        ("map: segment 0 tombstones", &1u64.to_le_bytes()),
    ];
    #[rustfmt::skip]
    let segment_seal: [(&str, &[u8]); 3] = [
        ("map: segment 0 len",        &9u64.to_le_bytes()),
        ("map: segment 0 tail",       &0x900u32.to_le_bytes()),
        ("map: segment 0 sums",       &0x9_0000u32.to_le_bytes()),
    ];
    let fields = [&golden[..], &segment_seal].concat();

    assert_fields(&body.encode(), &fields);

    let bytes: Vec<u8> = fields.iter().flat_map(|(_, bytes)| bytes.iter().copied()).collect();
    assert_eq!(Body::decode(&bytes, &schema).expect("golden bytes decode"), body);
}

/// Fixed-width sequence sealed at 6 bytes: not whole 4-byte records
#[test]
fn a_manifest_body_sealing_part_of_a_fixed_record_is_refused() {
    const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(4));
    let schema =
        Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, Tables::new(&[FIXED], &[]));
    let body = Body {
        durable: DurableTip { tip: Some(block_at(0)) },
        sequences: vec![Seals {
            data: Sealed { len: 6, tail: 0, sums: 0 },
            records: Records::Fixed(NonZeroU32::new(4).expect("4")),
        }],
        maps: vec![],
    };

    let refused = Body::decode(&body.encode(), &schema).expect_err("6 bytes of 4-byte records");

    assert_eq!(
        refused.to_string(),
        "malformed manifest body: a sequence's length is not whole records"
    );
}

/// 3 commits, settled before each: commits 1 + 2's segments merged at commit 3 (inputs unlinked)
#[test]
fn verify_of_a_merged_store_scrubs_every_listed_file_clean() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let engine = DiskEngine::with_fanout(RealFs::shared(), 2);
    let mut store = engine.open(&path, &SCHEMA, NonZeroUsize::MAX).expect("open");
    let mut model = Model::default();
    for writes in [
        Writes { records: 3, owners: vec![1], ids: 1, removed: vec![] },
        Writes { records: 1, owners: vec![2], ids: 1, removed: vec![] },
        Writes::default(),
    ] {
        store.settle();
        store.apply(model.advance(&writes));
        store.commit().expect("commit");
    }

    let verified = engine.verify(&path, &SCHEMA).expect("verify");

    assert_eq!(verified.heights, 3, "durable heights");
    let names: Vec<(&str, bool)> =
        verified.units.iter().map(|unit| (unit.name.as_str(), unit.is_clean())).collect();
    #[rustfmt::skip]
    let expected = [
        ("blocks.dat", true), ("blocks.idx", true), ("heights.dat", true), ("pool/nodes.dat", true),
        ("scanned/0000000002.seg", true), ("probed/0000000002.seg", true),
    ];
    assert_eq!(names, expected, "(file, clean): one merged segment per map");
}

#[test]
fn verify_names_the_page_a_flipped_byte_lands_in() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let engine = DiskEngine::with_fanout(RealFs::shared(), 2);
    let mut store = engine.open(&path, &SCHEMA, NonZeroUsize::MAX).expect("open");
    store.apply(Model::default().advance(&Writes {
        records: 3,
        owners: vec![1],
        ids: 1,
        removed: vec![],
    }));
    store.commit().expect("commit");
    let heights = path.join("heights.dat");
    let mut bytes = std::fs::read(&heights).expect("read");
    bytes[3] ^= 1;
    std::fs::write(&heights, &bytes).expect("write");

    let verified = engine.verify(&path, &SCHEMA).expect("verify");

    let bad: Vec<(&str, &[u64])> = (verified.units.iter())
        .filter(|unit| !unit.is_clean())
        .map(|unit| (unit.name.as_str(), unit.bad_pages.as_slice()))
        .collect();
    assert_eq!(bad, [("heights.dat", &[0][..])], "(file, bad pages) of every unclean file");
}

/// Merge lands between verify's manifest read and its scrub: the inputs it lists are gone
///
/// - scrubbed again against a newer manifest = clean
/// - no newer manifest = the inputs lost
#[test]
fn verify_rescrubs_files_a_merge_retired_against_the_newer_manifest() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let fs = RealFs::shared();
    let engine = DiskEngine::with_fanout(fs.clone(), 2);
    let mut store = engine.open(&path, &SCHEMA, NonZeroUsize::MAX).expect("open");
    let mut model = Model::default();
    store.apply(model.advance(&Writes { records: 3, owners: vec![1], ids: 1, removed: vec![] }));
    store.commit().expect("commit");
    store.apply(model.advance(&Writes { records: 1, owners: vec![2], ids: 1, removed: vec![] }));
    store.commit().expect("commit");
    let read = || index_dir::read_durable(fs.as_ref(), &path, identity(&SCHEMA)).expect("read");
    let before_merge = read();
    store.settle();
    store.apply(model.advance(&Writes::default()));
    store.commit().expect("the merge lands, its inputs unlinked");
    let after_merge = read();

    let mut reads = 0;
    let rescrubbed = verify_durable(fs.as_ref(), &path, &SCHEMA, || {
        reads += 1;
        Ok(if reads == 1 { before_merge.clone() } else { after_merge.clone() })
    })
    .expect("verify");
    let stale =
        verify_durable(fs.as_ref(), &path, &SCHEMA, || Ok(before_merge.clone())).expect("verify");

    assert_eq!(reads, 2, "manifest read again once its files went missing");
    assert_eq!(
        rescrubbed,
        engine.verify(&path, &SCHEMA).expect("verify"),
        "rescrubbed = fresh verify"
    );
    let lost: Vec<&str> =
        stale.units.iter().filter(|unit| unit.lost).map(|unit| unit.name.as_str()).collect();
    #[rustfmt::skip]
    let inputs = ["scanned/0000000000.seg", "scanned/0000000001.seg", "probed/0000000000.seg", "probed/0000000001.seg"];
    assert_eq!(lost, inputs, "listed only by the manifest before the merge: lost");
}

#[test]
fn verify_of_a_directory_never_created_finds_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    let engine = DiskEngine::with_fanout(RealFs::shared(), 2);

    let verified = engine.verify(&root.path().join("absent"), &SCHEMA).expect("verify");

    assert_eq!(verified, Verification { heights: 0, units: Vec::new() });
}

#[test]
fn verify_refuses_another_networks_directory() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let engine = DiskEngine::with_fanout(RealFs::shared(), 2);
    let mut store = engine.open(&path, &SCHEMA, NonZeroUsize::MAX).expect("open");
    store.apply(store.changes(block_at(0)));
    store.commit().expect("commit");

    let mainnet = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Main, TABLES);
    let refused =
        engine.verify(&path, &mainnet).expect_err("a regtest directory, verified as mainnet");

    assert_eq!(
        refused.to_string(),
        "directory was built for network tag 2, configured network is Main"
    );
}
