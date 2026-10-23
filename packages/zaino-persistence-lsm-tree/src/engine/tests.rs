//! Files-only concerns: every crash state, every failed I/O call, open's trimming + refusals,
//! golden manifest bytes, offline verify, the LSM's own guards

use std::{num::NonZeroU32, ops::Bound, path::Path};

use zaino_persistence::{
    conformance::{
        block, block_ref, probed_key, scanned_key, Model, BLOCKS, PROBED, SCANNED, SPECS,
    },
    Backend, BackendReader, CommitError, FlushError, MaintenanceReader, ReadError, WriteOp,
};

use super::*;
use crate::{
    fs::{RealFs, SimFs},
    lsm::{Breach, Cursor, SegmentError},
    testing::{commit, eventually, open, ROOT},
    LsmTreeBackend,
};

/// Fanout 2 (merges running beside the writes), removals of durable and buffered rows; per crash
/// state: recovered = an acked or the attempted commit, every namespace = the model at that
/// commit (a removed row never back), verify clean, commits continue
#[test]
fn every_crash_state_reopens_to_exactly_an_acknowledged_or_the_attempted_commit() {
    let fs = SimFs::recording();
    // the last batch's 80 probed ids (filtered whole): a filter past the tests' 64-byte spill
    // threshold
    let batches: [(u8, &[u8], u16, &[u16]); 4] = [
        (2, &[1, 2, 1], 2, &[]),
        (1, &[1, 3], 1, &[0]),
        (0, &[], 1, &[0, 1]),
        (3, &[4], 80, &[]),
    ];
    let mut models = vec![Model::default()];
    {
        let backend = open(&fs, 2);
        for (acked, (records, owners, ids, removals)) in (1u64..).zip(batches) {
            let mut model = models.last().expect("seeded").clone();
            commit(&backend, model.advance(records, owners, ids, removals)).expect("commit");
            backend.flush().expect("flush");
            models.push(model);
            fs.set_tag(acked);
        }
    }

    let states = fs.crash_states();
    assert!(
        states.len() > 50,
        "enumerated {} crash states",
        states.len()
    );
    let ops = fs.recorded_ops();
    assert!(
        ops.iter().any(|op| op.contains(".scratch")),
        "the workload's segments spill: crash points while scratch files exist"
    );
    assert!(
        states
            .iter()
            .any(|state| state.label.contains("blocks.dat: ")
                && state.label.contains("first sector garbled")),
        "an append's garbled first sector: a committed tail damaged, restored from TAILS"
    );
    for state in states {
        let label = &state.label;
        let backend = LsmTreeBackend::open(
            state.fs.clone(),
            Path::new(ROOT),
            &crate::testing::config(2, std::num::NonZeroUsize::MAX),
        )
        .unwrap_or_else(|error| panic!("{label}: {error}"));
        let reader = backend.reader().expect("reader");
        let recovered = reader
            .tip()
            .expect("tip")
            .map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let acked = usize::try_from(state.tag).expect("small");
        assert!(
            recovered == acked || recovered == acked + 1,
            "{label}: recovered {recovered}"
        );
        models[recovered].assert_view(&reader, label);
        let verified = backend.verify().expect("verify");
        assert!(verified.is_clean(), "{label}: {verified:?}");

        let mut model = models[recovered].clone();
        commit(&backend, model.advance(1, &[9], 1, &[])).expect("commit after recovery");
        backend.flush().expect("flush after recovery");
        model.assert_view(
            &backend.reader().expect("reader"),
            &format!("{label}: commits continue at the recovered end"),
        );
    }
}

/// A write that lands a map's merge without rows of its own for that map (so it syncs no map
/// directory itself): every crash state around it reopens to an acknowledged or the attempted
/// commit, the merged segment's name durable before any manifest lists it
#[test]
fn every_crash_state_of_a_write_landing_a_merge_without_rows_reopens_whole() {
    let fs = SimFs::recording();
    let mut models = vec![Model::default()];
    let config = crate::testing::config(2, std::num::NonZeroUsize::MAX);
    {
        let backend = LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &config).expect("open");
        // two scanned segments: their merge launches
        for owner in [1u8, 2] {
            let mut model = models.last().expect("seeded").clone();
            commit(&backend, model.advance(0, &[owner], 0, &[])).expect("commit");
            backend.flush().expect("flush");
            models.push(model);
            fs.set_tag((models.len() - 1) as u64);
        }
        // writes with no scanned rows until one lands the merge
        let scanned = |backend: &LsmTreeBackend| {
            let units = backend.verify().expect("verify").units;
            units
                .iter()
                .filter(|unit| unit.name.starts_with("scanned/"))
                .count()
        };
        eventually("a write without scanned rows lands the merge", || {
            let mut model = models.last().expect("seeded").clone();
            commit(&backend, model.advance(1, &[], 0, &[])).expect("commit");
            backend.flush().expect("flush");
            models.push(model);
            fs.set_tag((models.len() - 1) as u64);
            (scanned(&backend) == 1).then_some(())
        });
    }
    let states = fs.crash_states();
    for state in states {
        let label = &state.label;
        let backend = LsmTreeBackend::open(state.fs.clone(), Path::new(ROOT), &config)
            .unwrap_or_else(|error| panic!("{label}: {}", chain(&error)));
        let reader = backend.reader().expect("reader");
        let recovered = reader
            .tip()
            .expect("tip")
            .map_or(0, |tip| u32::from(tip.height) as usize + 1);
        let acked = usize::try_from(state.tag).expect("small");
        assert!(
            recovered == acked || recovered == acked + 1,
            "{label}: recovered {recovered}"
        );
        models[recovered].assert_view(&reader, label);
    }
}

/// A first open that fails (or is killed) after creating a nested sequence's files but before
/// syncing their directory: the next open makes their names durable, so a commit acknowledged
/// then survives a power loss (it was lost, and the open after refused, before every open synced
/// every sequence directory)
#[test]
fn a_nested_sequence_a_failed_open_created_survives_a_power_loss_after_its_first_commit() {
    let config = crate::testing::config(2, std::num::NonZeroUsize::MAX);
    let failed = (0..400u64).find_map(|op| {
        let fs = SimFs::new();
        fs.fail_from(op);
        match LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &config) {
            Ok(_) => None,
            Err(error) => chain(&error).contains("sync_dir /idx/pool").then_some(fs),
        }
    });
    let failed = failed.expect("an open fails at the nested directory's sync");
    let fs = failed.restarted();
    let mut model = Model::default();
    {
        let backend = LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &config).expect("reopen");
        commit(&backend, model.advance(2, &[1], 1, &[])).expect("commit");
        backend.flush().expect("acknowledged");
    }
    let crashed = fs.power_loss();
    let backend = LsmTreeBackend::open(crashed, Path::new(ROOT), &config)
        .unwrap_or_else(|error| panic!("the acknowledged commit survives: {}", chain(&error)));
    model.assert_view(&backend.reader().expect("reader"), "after the power loss");
}

/// Op 0, 1, 2, … failed until the workload succeeds (Pebble `errorfs`); each = the injected `Err`
/// (never a panic, never swallowed), later commits refused, restart = acked or attempted commit
///
/// - no merges (fanout past every tier): every I/O call runs on the writer, in order
#[test]
fn every_failed_io_call_surfaces_poisons_the_backend_and_recovers_to_a_committed_state() {
    let batches: [(u8, &[u8], u16, &[u16]); 3] =
        [(2, &[1, 2], 1, &[]), (1, &[1], 1, &[0]), (1, &[3], 2, &[1])];
    let mut models = vec![Model::default()];
    for (records, owners, ids, removals) in batches {
        let mut model = models.last().expect("seeded").clone();
        model.advance(records, owners, ids, removals);
        models.push(model);
    }
    let config = crate::testing::config(1 << 20, std::num::NonZeroUsize::MAX);

    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 10_000, "workload never succeeded");
        let fs = SimFs::new();
        fs.fail_from(fail_at);
        let mut acked = 0;
        let error = match LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &config) {
            Err(error) => error.to_string() + &chain(&error),
            Ok(backend) => {
                let mut failed = None;
                for at in 0..batches.len() {
                    let (records, owners, ids, removals) = batches[at];
                    let next = models[at].clone().advance(records, owners, ids, removals);
                    let written = commit(&backend, next)
                        .map_err(FlushError::Write)
                        .and_then(|()| backend.flush());
                    match written {
                        Ok(()) => acked = at + 1,
                        Err(error) => {
                            failed = Some(chain(&error));
                            break;
                        }
                    }
                }
                let Some(error) = failed else {
                    assert!(
                        fs.mutations() <= fail_at,
                        "op {fail_at}: injected, never surfaced"
                    );
                    break;
                };
                let retried = commit(&backend, models[acked].clone().advance(0, &[], 0, &[]));
                assert!(
                    matches!(retried, Err(CommitError::Poisoned)),
                    "op {fail_at}: {retried:?}"
                );
                error
            }
        };
        failures += 1;
        assert!(error.contains("injected EIO"), "op {fail_at}: {error}");

        let fs = fs.restarted();
        let backend = LsmTreeBackend::open(fs, Path::new(ROOT), &config)
            .unwrap_or_else(|error| panic!("op {fail_at}: reopen: {error}"));
        let reader = backend.reader().expect("reader");
        let recovered = reader
            .tip()
            .expect("tip")
            .map_or(0, |tip| u32::from(tip.height) as usize + 1);
        assert!(
            recovered == acked || recovered == acked + 1,
            "op {fail_at}: recovered {recovered}"
        );
        models[recovered].assert_view(&reader, &format!("op {fail_at}"));
        commit(&backend, models[recovered].clone().advance(1, &[9], 1, &[]))
            .expect("commit after restart");
        backend.flush().expect("flush after restart");
    }
    assert!(failures > 40, "only {failures} failure points exercised");
}

/// Every message in `error`'s cause chain
fn chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        out.push_str(": ");
        out.push_str(&next.to_string());
        cause = next.source();
    }
    out
}

/// Read 0, 1, 2, … failed while reopening a committed directory until the open succeeds
///
/// - each failure = the injected `Err`, never a panic; a successful open finds every commit
#[test]
fn every_failed_read_at_open_surfaces_and_a_clean_open_finds_every_commit() {
    let fs = SimFs::new();
    let mut model = Model::default();
    {
        let backend = open(&fs, 2);
        for n in 0..3u8 {
            commit(&backend, model.advance(2, &[n], 1, &[])).expect("commit");
            backend.flush().expect("flush");
        }
    }

    let config = crate::testing::config(2, std::num::NonZeroUsize::MAX);
    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 1_000, "open never succeeded");
        let fs = fs.restarted();
        fs.fail_reads_from(fail_at);
        match LsmTreeBackend::open(fs, Path::new(ROOT), &config) {
            Ok(backend) => {
                model.assert_view(
                    &backend.reader().expect("reader"),
                    &format!("read {fail_at}"),
                );
                break;
            }
            Err(error) => {
                let error = chain(&error);
                assert!(
                    error.contains("injected read EIO"),
                    "read {fail_at}: {error}"
                );
                assert!(!error.contains("panicked"), "read {fail_at}: {error}");
                failures += 1;
            }
        }
    }
    assert!(
        failures > 0,
        "open reads through positional reads (the manifest at least)"
    );
}

/// The same key written by two commits straight to the engine (the backend's rules refuse it:
/// only a writer bug gets here), in a map with deletes and one without: a range read and the
/// merge both name the breach and the segments holding the key
#[test]
fn a_key_held_twice_is_named_by_reads_and_merges() {
    let specs: Arc<[NamespaceSpec]> = SPECS.as_slice().into();
    let repeated = |error: &StoreError| {
        matches!(
            error,
            StoreError::Segment(SegmentError::Contract {
                segments,
                breach: Breach::Repeated { held: 2 },
            }) if segments.len() == 2
        )
    };
    for (namespace, key, value) in [
        (SCANNED, scanned_key(1, 0), vec![0; 8]),
        (PROBED, probed_key(0), vec![0; 4]),
    ] {
        let fs: Arc<dyn Fs> = SimFs::new();
        let lsm = LsmConfig {
            fanout: 2,
            spill_bytes: crate::testing::SPILL,
            ..LsmConfig::default()
        };
        let identity = Identity::new("forged", 1).expect("identity");
        let tunings = vec![Tuning::default(); specs.len()];
        let mut engine = Engine::open(
            &fs,
            Path::new(ROOT),
            identity,
            Arc::clone(&specs),
            &tunings,
            lsm,
            Some(crate::testing::THREADS),
        )
        .expect("open");
        let op = WriteOp::Put {
            namespace,
            key,
            value,
        };
        let write = |engine: &mut Engine, n: usize, ops: &[WriteOp]| {
            let buffer = Buffer::empty(&specs)
                .with(&specs, block_ref(n), ops)
                .expect("buffer");
            engine.write(&buffer)
        };
        for n in 1..=2 {
            write(&mut engine, n, std::slice::from_ref(&op)).expect("write");
        }

        let index = position(&specs, namespace).expect("declared");
        let Some(Durable::Map(snapshot)) = engine.view().held(index) else {
            panic!("{namespace} is a map");
        };
        let walked = Cursor::new(snapshot, Bound::Unbounded, Bound::Unbounded, false)
            .map(drop)
            .map_err(StoreError::from);
        assert!(
            walked.as_ref().is_err_and(repeated),
            "{namespace} walk: {:?}",
            walked.err()
        );

        // two 1-row segments at fanout 2 = a merge; its `Contract` surfaces at a later write,
        // as that write's `MergeFailed`
        let mut n = 2;
        let merged = eventually("the merge's breach surfaces", || {
            n += 1;
            let written = write(&mut engine, n, &[]);
            written.is_err().then_some(written)
        });
        let breach = match &merged {
            Err(StoreError::Segment(SegmentError::MergeFailed { source, .. })) => Some(source),
            _ => None,
        };
        assert!(
            breach.is_some_and(|source| matches!(
                &**source,
                SegmentError::Contract {
                    segments,
                    breach: Breach::Repeated { held: 2 },
                } if segments.len() == 2
            )),
            "{namespace} merge: {:?}",
            merged.err()
        );
    }
}

/// - open drops bytes past the manifest; restores a torn tail page from `TAILS`
/// - refused: lost file, a torn tail page `TAILS` can't restore, data in a directory that never
///   committed
#[test]
fn open_trims_to_the_manifest_and_refuses_lost_torn_or_unmanifested_data() {
    let populated = || {
        let fs = SimFs::new();
        let backend = open(&fs, 2);
        commit(&backend, Model::default().advance(4, &[1, 2], 3, &[])).expect("commit");
        backend.flush().expect("flush");
        fs
    };
    let path = |name: &str| Path::new(ROOT).join(name);
    let blocks_len = |fs: &SimFs| fs.contents(&path("blocks.dat")).expect("blocks").len();
    let config = crate::testing::config(2, std::num::NonZeroUsize::MAX);

    let fs = populated();
    let committed = (0..4).map(|n| block(n).len()).sum::<usize>();
    fs.corrupt(&path("blocks.dat"), |bytes| {
        bytes.extend_from_slice(&[0xa5; 64])
    });
    let backend = open(&fs, 2);
    assert_eq!(blocks_len(&fs), committed, "uncommitted tail truncated");
    let blocks = backend
        .reader()
        .expect("reader")
        .scan(BLOCKS)
        .expect("scan");
    let records: Vec<Vec<u8>> = blocks.into_iter().map(|(_, record)| record).collect();
    assert_eq!(records, (0..4).map(block).collect::<Vec<_>>());
    drop(backend);

    let refused = |edit: &dyn Fn(&SimFs)| {
        let fs = populated();
        edit(&fs);
        let error = LsmTreeBackend::open(fs, Path::new(ROOT), &config)
            .err()
            .expect("refused");
        chain(&error)
    };
    // cut inside its tail page, TAILS would restore it: lost only with TAILS gone too
    let short = refused(&|fs| {
        fs.corrupt(&path("pool/nodes.dat"), |bytes| bytes.truncate(10));
        fs.corrupt(&path("TAILS"), |bytes| bytes.fill(0));
    });
    assert!(
        short.ends_with("/idx/pool/nodes.dat is 10 bytes, the committed state needs 32"),
        "{short}"
    );
    // a torn tail page: restored from TAILS; refused only when TAILS can't restore it
    let fs = populated();
    fs.corrupt(&path("blocks.idx"), |bytes| bytes[3] ^= 1);
    let backend = open(&fs, 2);
    let blocks = backend
        .reader()
        .expect("reader")
        .scan(BLOCKS)
        .expect("scan");
    assert_eq!(blocks.len(), 4, "restored: every record readable");
    drop(backend);
    let torn = refused(&|fs| {
        fs.corrupt(&path("blocks.idx"), |bytes| bytes[3] ^= 1);
        fs.corrupt(&path("TAILS"), |bytes| bytes.fill(0));
    });
    assert!(
        torn.ends_with("/idx/blocks.idx: tail page fails its checksum"),
        "{torn}"
    );

    let bare = SimFs::new();
    bare.create_dir_all(Path::new(ROOT)).expect("dir");
    bare.open(&path("heights.dat"))
        .expect("file")
        .write_all_at(&[1], 0)
        .expect("write");
    let opened = LsmTreeBackend::open(bare, Path::new(ROOT), &config);
    let error = chain(&opened.err().expect("unmanifested data refused"));
    assert!(
        error.contains("holds data but the directory has no MANIFEST"),
        "{error}"
    );
}

/// Manifest body pinned byte for byte: namespaces, committed tip, each sequence's seals in
/// declaration order (two for a variable one), each map's segment list
#[test]
fn a_manifest_body_is_its_golden_bytes() {
    let width = |n| NonZeroU32::new(n).expect("non-zero");
    let specs = [
        NamespaceSpec::sequence(Namespace::new("fixed"), Width::Fixed(width(4))),
        NamespaceSpec::sequence(Namespace::new("variable"), Width::Variable),
        NamespaceSpec::map(Namespace::new("map"), width(8), width(1), true),
    ];
    let sealed = |n: u8| Sealed {
        len: u64::from(n),
        tail: u32::from(n) << 8,
        sums: u32::from(n) << 16,
    };
    let body = Body {
        committed: Committed {
            tip: Some(block_ref(3)),
        },
        stored: vec![
            Stored::Sequence(Seals {
                data: sealed(4),
                ends: Sealed::EMPTY,
            }),
            Stored::Sequence(Seals {
                data: sealed(5),
                ends: sealed(8),
            }),
            Stored::Map(vec![SegmentMeta {
                id: 7,
                records: 2,
                filtered: 6,
                sealed: sealed(9),
            }]),
        ],
    };
    let seal = |n: u8| {
        [
            u64::from(n).to_le_bytes().as_slice(),
            &(u32::from(n) << 8).to_le_bytes(),
            &(u32::from(n) << 16).to_le_bytes(),
        ]
        .concat()
    };
    let declared = [
        &3u32.to_le_bytes()[..],
        &[5],
        b"fixed",
        &[0],
        &4u32.to_le_bytes(),
        &[8],
        b"variable",
        &[0],
        &0u32.to_le_bytes(),
        &[3],
        b"map",
        &[1],
        &8u32.to_le_bytes(),
        &1u32.to_le_bytes(),
        &[1],
    ]
    .concat();
    let golden = [
        &declared[..],
        &3u64.to_le_bytes(),
        &[3; 32],
        &seal(4),
        &seal(5),
        &seal(8),
        &1u32.to_le_bytes(),
        &7u32.to_le_bytes(),
        &2u64.to_le_bytes(),
        &6u32.to_le_bytes(),
        &seal(9),
    ]
    .concat();
    assert_eq!(body.encode(&specs).expect("encodes"), golden);
    assert_eq!(Body::decode(&golden, &specs).expect("decodes"), body);

    let at = declared.len() + 40;
    let partial = Body::decode(&[&golden[..at], &seal(6)].concat(), &specs);
    assert!(
        matches!(partial, Err(StoreError::Manifest(ManifestError::Body(_)))),
        "6 bytes is not whole 4-byte records: {partial:?}"
    );
    let other = Body::decode(&golden, &specs[..2]);
    assert!(matches!(other, Err(StoreError::Specs(_))), "{other:?}");
}

/// `verify` over a real directory: clean, flipped byte names its page, missing file lost, file a
/// merge retired between manifest read and scrub scrubbed again
#[test]
fn verify_names_bad_pages_and_lost_files_and_rescrubs_after_a_merge() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("idx");
    let fs = RealFs::shared();
    let config = crate::testing::config(2, std::num::NonZeroUsize::MAX);
    let backend = LsmTreeBackend::open(fs.clone(), &path, &config).expect("open");
    let identity = Identity::new("conformance", 1).expect("identity");
    let mut model = Model::default();
    commit(&backend, model.advance(3, &[1], 1, &[])).expect("commit");
    backend.flush().expect("flush");
    commit(&backend, model.advance(1, &[2], 1, &[])).expect("commit");
    backend.flush().expect("flush");
    let read = || manifest::read(fs.as_ref(), &path, &identity).expect("read");
    let before = read();
    // each map's two segments merge in the background; a later write lands the merge
    eventually("both merges landed, their inputs unlinked", || {
        commit(&backend, model.advance(0, &[], 0, &[])).expect("commit");
        backend.flush().expect("flush");
        (backend.verify().expect("verify").units.len() == 4 + 2).then_some(())
    });
    let after = read();

    let clean = backend.verify().expect("verify");
    assert!(clean.is_clean(), "{clean:?}");
    let names: Vec<&str> = clean.units.iter().map(|unit| unit.name.as_str()).collect();
    assert_eq!(
        names[..4],
        ["blocks.dat", "blocks.idx", "heights.dat", "pool/nodes.dat"]
    );
    assert_eq!(names.len(), 4 + 2, "one merged segment per map");

    // a scrub of the older manifest, its merged-away inputs gone by the time it reads them:
    // the manifest read once more no longer lists them, so they are skipped, not lost; one pass
    let mut reads = [before.clone(), after].into_iter();
    let mut manifest_reads = 0;
    let rescrubbed = verify_committed(fs.as_ref(), &path, &SPECS, || {
        manifest_reads += 1;
        Ok(reads.next().expect("two reads at most"))
    })
    .expect("verify");
    assert_eq!(manifest_reads, 2, "one pass, one recheck");
    let names: Vec<&str> = rescrubbed
        .units
        .iter()
        .map(|unit| unit.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["blocks.dat", "blocks.idx", "heights.dat", "pool/nodes.dat"],
        "the files committed when it started, retired segments skipped"
    );
    assert!(
        rescrubbed
            .units
            .iter()
            .all(|unit| !unit.problems.iter().any(|problem| problem.is_damage())),
        "{rescrubbed:?}"
    );
    let stale =
        verify_committed(fs.as_ref(), &path, &SPECS, || Ok(before.clone())).expect("verify");
    let lost = stale
        .units
        .iter()
        .any(|unit| unit.problems.contains(&zaino_persistence::Problem::Missing));
    assert!(lost, "a file still listed but missing is lost: {stale:?}");

    let heights = path.join("heights.dat");
    let mut bytes = std::fs::read(&heights).expect("read");
    bytes[3] ^= 1;
    std::fs::write(&heights, &bytes).expect("write");
    let corrupt = backend.verify().expect("verify");
    let unit = corrupt
        .units
        .iter()
        .find(|unit| unit.name == "heights.dat")
        .expect("listed");
    assert!(!corrupt.is_clean());
    let damage: Vec<_> = unit
        .problems
        .iter()
        .filter(|problem| problem.is_damage())
        .collect();
    assert_eq!(
        damage,
        [&zaino_persistence::Problem::ChecksumMismatch { page: 0 }],
        "the write-ahead reserve past the seal is orphaned, not damage"
    );
    let read = backend.reader().expect("reader").get(
        zaino_persistence::conformance::HEIGHTS,
        &zaino_persistence::sequence_key(0),
    );
    assert!(matches!(read, Err(ReadError::Corrupted { .. })), "{read:?}");

    let absent = verify(fs.as_ref(), &root.path().join("absent"), &identity, &SPECS);
    assert!(
        absent
            .as_ref()
            .is_err_and(|error| error.to_string().contains("no index here")),
        "a path holding no index is an error, never a clean report: {absent:?}"
    );
}

/// Every map's leftovers summed for the alert, with the error of a map that left something over
#[test]
fn leftovers_are_totalled_across_maps() {
    // a map whose last failure left nothing over: its error is not the leftovers'
    let maps = [
        (0, 0, Some("listing failed")),
        (2, 400, Some("EIO")),
        (0, 0, None),
        (1, 700, Some("EACCES")),
    ];
    let total = leftover_total(maps.into_iter());
    assert_eq!(total, Some((3, 1_100, "EIO")));
    assert_eq!(leftover_total([(0, 0, None)].into_iter()), None);
    assert_eq!(
        leftover_total([(0, 0, Some("listing failed"))].into_iter()),
        None,
        "nothing left over, nothing reported"
    );
}
