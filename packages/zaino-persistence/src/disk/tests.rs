//! `DiskEngine` under the conformance suite (`crate::conformance`), then files-only concerns:
//! every crash state, every failed I/O call, open's trimming + refusals, golden manifest bytes,
//! offline verify, the LSM's own guards

use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::Arc,
};

use proptest::prelude::*;
use zcash_protocol::consensus::NetworkType;

use super::*;
use crate::{
    conformance::{
        self, block, block_ref, panic_message, scanned_key, Model, Subject, BLOCKS, HEIGHTS,
        SCANNED, SCHEMA, TABLES,
    },
    fs::{RealFs, SimFs},
    manifest::IndexKind,
    port::{MapRead, MapTable, SequenceRead, SequenceTable, Tables, Width},
};

const ROOT: &str = "/idx";

fn open(engine: &DiskEngine) -> DiskStore {
    engine.open(Path::new(ROOT), &SCHEMA).expect("open")
}

/// `DiskEngine` on `SimFs`: crash = `SimFs::power_loss`, background work = merges, internal
/// invariants = each map's tiers + files, the buffer layer's
struct SimDisk {
    fs: Arc<SimFs>,
    fanout: usize,
    path: PathBuf,
}

impl SimDisk {
    fn new(fanout: usize) -> Self {
        Self { fs: SimFs::new(), fanout, path: PathBuf::from(ROOT) }
    }
}

impl Subject for SimDisk {
    type Engine = DiskEngine;

    fn engine(&self) -> DiskEngine {
        DiskEngine::with_fanout(self.fs.clone(), self.fanout)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn power_loss(&mut self) -> bool {
        self.fs = self.fs.power_loss();
        true
    }

    fn settle(&self, store: &DiskStore) {
        store.settle();
    }

    fn check(&self, store: &DiskStore, just_opened: bool, label: &str) {
        assert_tiers(store, &self.fs, self.fanout, just_opened, label);
        store.buffer.check(label);
    }
}

/// - each map's tiers within `fanout` inputs + `STALL_WINDOWS` (2) idle windows
/// - `just_opened`: every listed segment + sums on disk, beyond them only running merges' outputs
fn assert_tiers(store: &DiskStore, fs: &SimFs, fanout: usize, just_opened: bool, label: &str) {
    for (table, log) in SCHEMA.maps().iter().zip(&store.maps) {
        let name = table.name;
        let mut per_tier = BTreeMap::<u32, usize>::new();
        for segment in log.segments() {
            *per_tier.entry(segment.records.ilog(fanout as u64)).or_default() += 1;
        }
        let bounded = per_tier.values().all(|&segments| segments < 3 * fanout);
        assert!(bounded, "{label}: {name} segments per tier {per_tier:?}");
        if just_opened {
            let on_disk = fs.list(&Path::new(ROOT).join(name)).expect("list");
            for segment in log.segments() {
                let file = file_name(segment.id);
                let present =
                    [file.clone(), format!("{file}.crc")].iter().all(|f| on_disk.contains(f));
                assert!(present, "{label}: {name} lost listed {file}: {on_disk:?}");
            }
            let extra = on_disk.len() - 2 * log.segments().len();
            assert!(extra <= 2 * log.merging(), "{label}: {name} unlisted {on_disk:?}");
        }
    }
}

proptest! {
    // 64 cases ≈ seconds; heavy run after any change: root CLAUDE.md
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Conformance history at fanouts merging every few commits and rarely
    #[test]
    fn random_histories_answer_like_a_btreemap_through_merges_reopens_and_power_loss(
        fanout in prop_oneof![Just(2usize), Just(3), Just(8)],
        steps in conformance::steps(),
    ) {
        conformance::history(SimDisk::new(fanout), &steps);
    }
}

#[test]
fn the_disk_engine_keeps_the_port_contract() {
    conformance::contract(SimDisk::new(2));
}

/// Fanout 2, merges launched + landed mid-history; per crash state: recovered = an acked or the
/// attempted commit, every table = the model at that commit, only listed files, commits continue
#[test]
fn every_crash_state_reopens_to_exactly_an_acknowledged_or_the_attempted_commit() {
    let fs = SimFs::recording();
    let batches: [(u8, &[u8], u16); 4] =
        [(2, &[1, 2, 1], 2), (1, &[1, 3], 1), (0, &[], 1), (3, &[4], 0)];
    let mut models = vec![Model::default()];
    {
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        for (acked, (records, owners, ids)) in (1u64..).zip(batches) {
            store.settle();
            let mut model = models.last().expect("seeded").clone();
            store.apply(model.advance(records, owners, ids));
            store.commit().expect("commit");
            models.push(model);
            fs.set_tag(acked);
        }
        let merged = store.maps.iter().map(|log| log.segments().len()).collect::<Vec<_>>();
        assert_eq!(merged, [2, 2], "merges landed: scanned 2+2+1 rows, probed 1+1+1");
    }

    let states = fs.crash_states();
    assert!(states.len() > 50, "enumerated {} crash states", states.len());
    for state in states {
        let label = &state.label;
        let mut store = DiskEngine::with_fanout(state.fs.clone(), 2)
            .open(Path::new(ROOT), &SCHEMA)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        let recovered = store.view().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let acked = usize::try_from(state.tag).expect("small");
        assert!(recovered == acked || recovered == acked + 1, "{label}: recovered {recovered}");
        models[recovered].assert_view(&store.view(), label);
        assert_tiers(&store, &state.fs, 2, true, label);

        let mut model = models[recovered].clone();
        store.apply(model.advance(1, &[9], 1));
        store.commit().expect("commit after recovery");
        model
            .assert_view(&store.view(), &format!("{label}: commits continue at the recovered end"));
    }
}

/// Op 0, 1, 2, … failed until the workload succeeds (Pebble `errorfs`); each = the injected `Err`
/// (never a panic, never swallowed), later commits refused, restart = acked or attempted commit
#[test]
fn every_failed_io_call_surfaces_poisons_the_store_and_recovers_to_a_committed_state() {
    let batches: [(u8, &[u8], u16); 3] = [(2, &[1, 2], 1), (1, &[1], 1), (1, &[3], 2)];
    let mut models = vec![Model::default()];
    for (records, owners, ids) in batches {
        let mut model = models.last().expect("seeded").clone();
        model.advance(records, owners, ids);
        models.push(model);
    }

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 10_000, "workload never succeeded");
        let fs = SimFs::new();
        fs.fail_from(fail_at);
        let engine = DiskEngine::with_fanout(fs.clone(), 2);
        let mut acked = 0;
        let error = match engine.open(Path::new(ROOT), &SCHEMA) {
            Err(error) => error,
            Ok(mut store) => {
                let mut failed = None;
                for at in 0..batches.len() {
                    store.settle();
                    let (records, owners, ids) = batches[at];
                    store.apply(models[at].clone().advance(records, owners, ids));
                    match store.commit() {
                        Ok(()) => acked = at + 1,
                        Err(error) => {
                            failed = Some(error);
                            break;
                        }
                    }
                }
                let error = match failed {
                    Some(error) => error,
                    None => {
                        store.settle();
                        if fs.mutations() <= fail_at {
                            break;
                        }
                        // op `fail_at` hit a merge the last commit launched: surfaces next commit
                        store.apply(models[acked].clone().advance(0, &[], 0));
                        let next = store.commit();
                        next.expect_err("a failed background merge surfaces at the next commit")
                    }
                };
                let retried = catch_unwind(AssertUnwindSafe(|| store.commit()));
                let message = panic_message(retried.expect_err("commit after a failed one"));
                assert!(message.contains("commit after a failed one"), "op {fail_at}: {message}");
                error
            }
        };
        failures += 1;
        assert!(error.to_string().contains("injected EIO"), "op {fail_at}: {error}");

        let fs = fs.restarted();
        let mut store = DiskEngine::with_fanout(fs.clone(), 2)
            .open(Path::new(ROOT), &SCHEMA)
            .unwrap_or_else(|error| panic!("op {fail_at}: reopen: {error}"));
        let recovered = store.view().tip().map_or(0, |tip| u32::from(tip.height) as usize + 1);
        assert!(
            recovered == acked || recovered == acked + 1,
            "op {fail_at}: recovered {recovered}"
        );
        models[recovered].assert_view(&store.view(), &format!("op {fail_at}"));
        store.apply(models[recovered].clone().advance(1, &[9], 1));
        store.commit().expect("commit after restart");
    }
    assert!(failures > 40, "only {failures} failure points exercised");
}

/// Read 0, 1, 2, … failed while reopening a committed store until the open succeeds
///
/// - each failure = the injected `Err`, never a panic; successful open finds every commit
#[test]
fn every_failed_read_at_open_surfaces_and_a_clean_open_finds_every_commit() {
    let fs = SimFs::new();
    let mut model = Model::default();
    {
        let mut store = open(&DiskEngine::with_fanout(fs.clone(), 2));
        for n in 0..3u8 {
            store.apply(model.advance(2, &[n], 1));
            store.commit().expect("commit");
        }
    }

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 1_000, "open never succeeded");
        let fs = fs.restarted();
        fs.fail_reads_from(fail_at);
        let engine = DiskEngine::with_fanout(fs.clone(), 2);
        let opened = catch_unwind(AssertUnwindSafe(|| engine.open(Path::new(ROOT), &SCHEMA)))
            .unwrap_or_else(|payload| {
                panic!("read {fail_at}: panicked: {}", panic_message(payload))
            });
        match opened {
            Ok(store) => {
                model.assert_view(&store.view(), &format!("read {fail_at}"));
                break;
            }
            Err(error) => {
                assert!(error.to_string().contains("injected read EIO"), "read {fail_at}: {error}");
                failures += 1;
            }
        }
    }
    assert!(failures > 0, "open reads through positional reads (the manifest at least)");
}

/// Each guard fires on the bug it guards, naming it (RocksDB: check never seen firing = not known
/// to work)
#[test]
fn invariant_checks_fire_on_the_bugs_they_guard() {
    let fires = |expected: &str, bug: &dyn Fn()| {
        let message = panic_message(catch_unwind(AssertUnwindSafe(bug)).expect_err(expected));
        assert!(message.contains(expected), "expected {expected:?}, got {message:?}");
    };
    let store = || open(&DiskEngine::with_fanout(SimFs::new(), 2));
    let changes = |n| Changes::new(block_ref(n), SCHEMA);
    let committed = |store: &mut DiskStore, changes| {
        store.apply(changes);
        store.commit().expect("commit");
    };
    let stray_sequence = SequenceTable::new(3, "stray", Width::Variable);
    let stray_map = MapTable::new(1, "probed", Width::fixed(16), Width::fixed(8), 0);

    fires("heights: a 7-byte item, width 8", &|| changes(1).sequence(HEIGHTS).append(&[0; 7]));
    fires("scanned: a 11-byte item, width 12", &|| {
        changes(1).map(SCANNED).insert(&[0; 11], &[0; 8])
    });
    fires("compact_block: sequence stray not in its schema", &|| {
        changes(1).sequence(stray_sequence).append(&[])
    });
    fires("compact_block: map probed not in its schema", &|| {
        changes(1).map(stray_map).insert(&[0; 16], &[0; 8])
    });
    fires("compact_block: map probed not in its schema", &|| {
        store().view().map(stray_map).value(&[0; 16]);
    });
    static SKIPPED: [SequenceTable; 1] = [SequenceTable::new(1, "skipped", Width::Variable)];
    fires("sequence id != its position", &|| {
        Tables::new(&SKIPPED, &[]);
    });
    fires("changes built for another schema", &|| {
        let other = Schema::new(IndexKind::BlockHash, 1, NetworkType::Regtest, TABLES);
        store().apply(Changes::new(block_ref(1), other));
    });
    fires("apply at height 0, not above the last applied Some(Height(0))", &|| {
        let mut store = store();
        committed(&mut store, changes(1));
        store.apply(changes(1));
    });
    fires("scanned: a map key held twice", &|| {
        let mut twice = changes(1);
        twice.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
        twice.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
        store().apply(twice);
    });
    let one_row = |n| {
        let mut changes = changes(n);
        changes.map(SCANNED).insert(&scanned_key(1, 0), &[0; 8]);
        changes
    };
    fires("a key listed in two committed segments", &|| {
        let mut store = store();
        committed(&mut store, one_row(1));
        committed(&mut store, one_row(2));
        store.view().map(SCANNED).range(&[0; 12], &[0xff; 12], usize::MAX);
    });
    // two 1-row segments at fanout 2 = merge; its duplicate panics on its thread, resumed here
    fires("strictly ascending", &|| {
        let mut store = store();
        committed(&mut store, one_row(1));
        committed(&mut store, one_row(2));
        store.settle();
        committed(&mut store, changes(3));
    });
}

/// - open drops bytes past the manifest
/// - refused: lost file, torn tail page, data in a directory that never committed
#[test]
fn open_trims_to_the_manifest_and_refuses_lost_torn_or_unmanifested_data() {
    let populated = || {
        let fs = SimFs::new();
        let mut store = open(&DiskEngine::new(fs.clone()));
        store.apply(Model::default().advance(4, &[1, 2], 3));
        store.commit().expect("commit");
        fs
    };
    let path = |name: &str| Path::new(ROOT).join(name);
    let blocks_len = |fs: &SimFs| fs.contents(&path("blocks.dat")).expect("blocks").len();

    let fs = populated();
    let committed = (0..4).map(|n| block(n).len()).sum::<usize>();
    fs.corrupt(&path("blocks.dat"), |bytes| bytes.extend_from_slice(&[0xa5; 64]));
    let store = open(&DiskEngine::new(fs.clone()));
    assert_eq!(blocks_len(&fs), committed, "uncommitted tail truncated");
    let records = store.view().sequence(BLOCKS).records(0..4);
    assert_eq!(records, (0..4).map(block).collect::<Vec<_>>());
    drop(store);

    let refused = |edit: &dyn Fn(&SimFs)| {
        let fs = populated();
        edit(&fs);
        DiskEngine::new(fs).open(Path::new(ROOT), &SCHEMA).expect_err("refused").to_string()
    };
    let short = refused(&|fs| fs.corrupt(&path("pool/nodes.dat"), |bytes| bytes.truncate(10)));
    assert_eq!(short, "/idx/pool/nodes.dat is 10 bytes, the committed state needs 32");
    let torn = refused(&|fs| fs.corrupt(&path("blocks.idx"), |bytes| bytes[3] ^= 1));
    assert_eq!(torn, "/idx/blocks.idx: tail page fails its checksum");

    let bare = SimFs::new();
    bare.create_dir_all(Path::new(ROOT)).expect("dir");
    bare.open(&path("heights.dat")).expect("file").write_all_at(&[1], 0).expect("write");
    let opened = DiskEngine::new(bare).open(Path::new(ROOT), &SCHEMA);
    assert!(matches!(opened, Err(StoreError::Manifest(ManifestError::Unmanifested { .. }))));
}

/// Manifest body pinned byte for byte: committed tip, each sequence's seals in schema order (two
/// for a variable one), each map's segment list
#[test]
fn a_manifest_body_is_its_golden_bytes() {
    const FIXED: SequenceTable = SequenceTable::new(0, "fixed", Width::fixed(4));
    const VARIABLE: SequenceTable = SequenceTable::new(1, "variable", Width::Variable);
    const MAP: MapTable = MapTable::new(0, "map", Width::fixed(8), Width::fixed(1), 0);
    const TABLES: Tables = Tables::new(&[FIXED, VARIABLE], &[MAP]);
    let schema = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest, TABLES);
    let sealed =
        |n: u8| Sealed { len: u64::from(n), tail: u32::from(n) << 8, sums: u32::from(n) << 16 };
    let body = Body {
        committed: Committed { tip: Some(block_ref(3)) },
        sequences: vec![
            Seals { data: sealed(4), ends: Sealed::EMPTY },
            Seals { data: sealed(5), ends: sealed(8) },
        ],
        maps: vec![vec![SegmentMeta { id: 7, records: 2, sealed: sealed(9) }]],
    };
    let seal = |n: u8| {
        [
            u64::from(n).to_le_bytes().as_slice(),
            &(u32::from(n) << 8).to_le_bytes(),
            &(u32::from(n) << 16).to_le_bytes(),
        ]
        .concat()
    };
    let golden = [
        &3u64.to_le_bytes()[..],
        &[3; 32],
        &seal(4),
        &seal(5),
        &seal(8),
        &1u32.to_le_bytes(),
        &7u32.to_le_bytes(),
        &2u64.to_le_bytes(),
        &seal(9),
    ]
    .concat();
    assert_eq!(body.encode(&schema), golden);
    assert_eq!(Body::decode(&golden, &schema).expect("decodes"), body);

    let partial = Body::decode(&[&golden[..40], &seal(6)].concat(), &schema);
    assert!(matches!(partial, Err(ManifestError::Body(_))), "6 bytes is not whole 4-byte records");
}

/// `verify` over a real directory: clean, flipped byte names its page, missing file lost, file a
/// merge retired between manifest read and scrub scrubbed again
#[test]
fn verify_names_bad_pages_and_lost_files_and_rescrubs_after_a_merge() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let fs = RealFs::shared();
    let engine = DiskEngine::with_fanout(fs.clone(), 2);
    let mut store = engine.open(&path, &SCHEMA).expect("open");
    let mut model = Model::default();
    store.apply(model.advance(3, &[1], 1));
    store.commit().expect("commit");
    store.apply(model.advance(1, &[2], 1));
    store.commit().expect("commit");
    let read = || manifest::read(fs.as_ref(), &path, identity(&SCHEMA)).expect("read");
    let before = read();
    store.settle();
    store.apply(model.advance(0, &[], 0));
    store.commit().expect("the merge lands, its inputs unlinked");
    let after = read();

    let clean = engine.verify(&path, &SCHEMA).expect("verify");
    assert!(clean.is_clean(), "{clean:?}");
    assert_eq!(clean.heights, 3);
    let names: Vec<&str> = clean.units.iter().map(|unit| unit.name.as_str()).collect();
    assert_eq!(names[..4], ["blocks.dat", "blocks.idx", "heights.dat", "pool/nodes.dat"]);
    assert_eq!(names.len(), 4 + 2, "one merged segment per map");

    let mut reads = [before.clone(), after.clone()].into_iter().chain(std::iter::repeat(after));
    let rescrubbed =
        verify_committed(fs.as_ref(), &path, &SCHEMA, || Ok(reads.next().expect("endless")))
            .expect("verify");
    assert_eq!(rescrubbed, clean, "retired inputs: scrubbed again against the newer manifest");
    let stale =
        verify_committed(fs.as_ref(), &path, &SCHEMA, || Ok(before.clone())).expect("verify");
    assert!(stale.units.iter().any(|unit| unit.lost), "a listed file missing is lost: {stale:?}");

    let heights = path.join("heights.dat");
    let mut bytes = std::fs::read(&heights).expect("read");
    bytes[3] ^= 1;
    std::fs::write(&heights, &bytes).expect("write");
    let corrupt = engine.verify(&path, &SCHEMA).expect("verify");
    let unit = corrupt.units.iter().find(|unit| unit.name == "heights.dat").expect("listed");
    assert_eq!((corrupt.is_clean(), unit.bad_pages.clone()), (false, vec![0]));

    let fresh = engine.verify(&root.path().join("absent"), &SCHEMA).expect("verify");
    assert_eq!(fresh, Verification { heights: 0, units: Vec::new() });
}
