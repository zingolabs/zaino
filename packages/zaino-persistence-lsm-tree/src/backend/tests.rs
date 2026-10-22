//! The adapter under the port's conformance suite, then what only it has: the write threshold,
//! poisoning, the guard, open's refusals, bounds over sequence keys

use std::{
    num::NonZeroUsize,
    ops::{
        Bound::{self, Excluded, Included, Unbounded},
        RangeBounds,
    },
    path::Path,
};

use proptest::prelude::*;
use zaino_persistence::{
    conformance::{self, Model, BLOCKS, PROBED, SCANNED},
    sequence_key, Width,
};

use super::*;
use crate::{
    fs::SimFs,
    testing::{commit, config, eventually, open, SimDisk, ROOT},
};

proptest! {
    // 64 cases ≈ seconds; heavy run after any change: PROPTEST_CASES=1000
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
fn the_backend_keeps_the_port_contract() {
    conformance::contract(SimDisk::new(2));
}

/// A commit taking the buffer past `write_buffer` writes it: durable without a flush
#[test]
fn a_full_buffer_is_written_by_the_commit_that_fills_it() {
    let fs = SimFs::new();
    let tiny = config(2, NonZeroUsize::MIN);
    let backend = LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &tiny).expect("open");
    let mut model = Model::default();
    commit(&backend, model.advance(2, &[1], 1, &[])).expect("commit");
    drop(backend);
    let reopened = LsmTreeBackend::open(fs, Path::new(ROOT), &tiny).expect("reopen");
    model.assert_view(
        &reopened.reader().expect("reader"),
        "written without a flush",
    );
}

/// A failed write: its error returned, later commits and flushes `Poisoned`, readers keep the
/// last published state
#[test]
fn a_failed_write_poisons_writes_and_readers_keep_reading() {
    let fs = SimFs::new();
    let backend = open(&fs, 2);
    let mut model = Model::default();
    commit(&backend, model.advance(2, &[1], 1, &[])).expect("commit");
    fs.fail_from(fs.mutations());
    let failed = backend.flush().expect_err("the injected failure");
    assert!(
        matches!(failed, FlushError::Write(CommitError::WriteFailed { .. })),
        "{failed:?}"
    );
    let refused = commit(&backend, model.clone().advance(1, &[], 0, &[]));
    assert!(matches!(refused, Err(CommitError::Poisoned)), "{refused:?}");
    let refused = backend.flush();
    assert!(
        matches!(refused, Err(FlushError::Write(CommitError::Poisoned))),
        "{refused:?}"
    );
    model.assert_view(
        &backend.reader().expect("reader"),
        "the last published state",
    );
}

/// A merge reading a corrupt page can't succeed on retry: the write that finds it is
/// `Corrupted`, and writes stay refused
#[test]
fn a_merge_over_a_corrupt_segment_fails_the_write_as_corrupted_and_poisons() {
    let fs = SimFs::new();
    let mut model = Model::default();
    let backend = open(&fs, 2);
    // several pages of rows: page 0 holds rows only (the filter, checked at open, is past it)
    commit(&backend, model.advance(0, &[1; 1_000], 0, &[])).expect("commit");
    backend.flush().expect("flush");
    drop(backend);

    let dir = Path::new(ROOT).join("scanned");
    let segments: Vec<String> = fs
        .list(&dir)
        .expect("list")
        .into_iter()
        .filter(|name| !name.ends_with(".crc"))
        .collect();
    let [segment] = segments.as_slice() else {
        panic!("one segment: {segments:?}");
    };
    // a key byte of row 0: lengths intact, so the open succeeds; the page fails on first touch
    fs.corrupt(&dir.join(segment), |bytes| bytes[9] ^= 1);

    // a second segment at fanout 2 = a merge reading the corrupt page
    let backend = open(&fs, 2);
    let failed = eventually("the merge's corruption surfaces", || {
        commit(&backend, model.advance(0, &[2], 0, &[])).expect("buffered");
        backend.flush().err()
    });
    assert!(
        matches!(failed, FlushError::Write(CommitError::Corrupted(_))),
        "{failed:?}"
    );
    let refused = commit(&backend, model.advance(1, &[], 0, &[]));
    assert!(matches!(refused, Err(CommitError::Poisoned)), "{refused:?}");
}

/// A merge failing for a cause a retry clears: that write's `MergeFailed` with nothing applied,
/// commits still accepted, and once the disk recovers the buffer is written
#[test]
fn a_failed_merge_fails_one_write_and_never_poisons() {
    let fs = SimFs::new();
    let mut model = Model::default();
    let backend = open(&fs, 8);
    for owner in [1, 2] {
        commit(&backend, model.advance(0, &[owner], 0, &[])).expect("commit");
        backend.flush().expect("flush");
    }
    drop(backend);

    // fanout 2: open launches the merge of the two segments; its creates fail
    fs.fail_creates(true);
    let backend = open(&fs, 2);
    let failed = eventually("the merge's failure surfaces", || {
        commit(&backend, model.advance(1, &[], 0, &[])).expect("buffered");
        backend.flush().err()
    });
    assert!(
        matches!(failed, FlushError::Write(CommitError::MergeFailed(_))),
        "{failed:?}"
    );
    model.assert_view(&backend.reader().expect("reader"), "still buffered");
    commit(&backend, model.advance(1, &[], 0, &[])).expect("writes stay open");

    fs.fail_creates(false);
    backend
        .flush()
        .expect("the buffer, written once the disk recovers");
    drop(backend);
    model.assert_view(&open(&fs, 2).reader().expect("reader"), "durable");
}

/// A panic in a scan's filter: that chunk's error, the walk finished, the backend unharmed
#[test]
fn a_panicking_filter_is_a_read_error() {
    let fs = SimFs::new();
    let backend = open(&fs, 2);
    let mut model = Model::default();
    commit(&backend, model.advance(3, &[1], 1, &[])).expect("commit");
    let range = ScanRange {
        lower: Unbounded,
        upper: Unbounded,
        direction: ScanDirection::Forward,
        chunk_bytes: NonZeroUsize::MAX,
    };
    let reader = backend.reader().expect("reader");
    let mut chunks = reader.scan_range(BLOCKS, range, |_: &[u8], _: &[u8]| -> Option<()> {
        panic!("a caller's bug")
    });
    let error = chunks.next().expect("a chunk").expect_err("caught");
    assert!(matches!(error, ReadError::ReadFailed { .. }), "{error:?}");
    assert!(error.to_string().contains("scan_range"), "{error}");
    assert!(chunks.next().is_none(), "finished after the error");
    commit(&backend, model.advance(1, &[], 0, &[])).expect("commits continue");
    model.assert_view(&backend.reader().expect("reader"), "unharmed");
}

/// Refused at open, each for its own reason: an unsafe or reserved name (any case), names equal
/// ignoring case, a nested map, bad tuning, a duplicate, other namespaces than the directory holds
#[test]
fn open_refuses_unsafe_names_bad_tuning_and_other_namespaces() {
    let width = |n| std::num::NonZeroU32::new(n).expect("non-zero");
    let sequence = |name| NamespaceSpec::sequence(Namespace::new(name), Width::Variable);
    let map = |name| NamespaceSpec::map(Namespace::new(name), width(8), width(8), false);
    let scoped = Tuning {
        scope: 4,
        cache_writes: false,
    };
    const UNSAFE: &str = "not of `/`-separated [A-Za-z0-9_-] parts, or a reserved one";
    const CASE: &str = "equal to another ignoring case";
    const NESTED: &str = "nested with another namespace's path";
    type Case = (
        &'static str,
        Vec<NamespaceSpec>,
        Vec<(Namespace, Tuning)>,
        &'static str,
    );
    let cases: Vec<Case> = vec![
        ("parent escape", vec![sequence("../x")], vec![], UNSAFE),
        ("absolute", vec![sequence("/x")], vec![], UNSAFE),
        ("empty part", vec![sequence("a//b")], vec![], UNSAFE),
        ("dot", vec![sequence("a.b")], vec![], UNSAFE),
        ("MANIFEST", vec![sequence("MANIFEST")], vec![], UNSAFE),
        ("manifest", vec![sequence("x/manifest")], vec![], UNSAFE),
        ("LOCK", vec![sequence("LOCK")], vec![], UNSAFE),
        (
            "MANIFEST.creating",
            vec![sequence("MANIFEST.creating")],
            vec![],
            UNSAFE,
        ),
        (
            "case-equal",
            vec![sequence("blocks"), map("Blocks")],
            vec![],
            CASE,
        ),
        (
            "nested in a map",
            vec![map("a"), sequence("a/b")],
            vec![],
            NESTED,
        ),
        (
            "nested ignoring case",
            vec![map("A"), sequence("a/b")],
            vec![],
            NESTED,
        ),
        ("map in a map", vec![map("a"), map("a/b")], vec![], NESTED),
        (
            "undeclared tuning",
            vec![sequence("a")],
            vec![(Namespace::new("b"), scoped)],
            "tuned, not declared",
        ),
        (
            "tuned twice",
            vec![map("a")],
            vec![(Namespace::new("a"), scoped); 2],
            "tuned twice",
        ),
        (
            "sequence scope",
            vec![sequence("a")],
            vec![(Namespace::new("a"), scoped)],
            "a scope on a sequence",
        ),
    ];
    fn open_with(
        fs: Arc<SimFs>,
        namespaces: &[NamespaceSpec],
        tuning: &[(Namespace, Tuning)],
    ) -> Result<LsmTreeBackend, OpenError> {
        let declared = LsmTreeConfig {
            namespaces,
            tuning,
            ..config(2, NonZeroUsize::MAX)
        };
        LsmTreeBackend::open(fs, Path::new(ROOT), &declared)
    }
    for (case, namespaces, tuning, reason) in cases {
        let error = open_with(SimFs::new(), &namespaces, &tuning)
            .err()
            .unwrap_or_else(|| panic!("{case}: opened"));
        let cause = std::error::Error::source(&error).map(ToString::to_string);
        assert!(
            matches!(error, OpenError::Unavailable { .. })
                && cause.as_ref().is_some_and(|cause| cause.contains(reason)),
            "{case}: {error:?}"
        );
    }
    let fine = [
        sequence("pool/a"),
        sequence("pool/b"),
        map("x"),
        sequence("x_y"),
    ];
    open_with(SimFs::new(), &fine, &[]).expect("siblings are fine");

    let twice = [sequence("a"), sequence("a")];
    let opened = open_with(SimFs::new(), &twice, &[]);
    assert!(matches!(opened, Err(OpenError::DuplicateNamespace(_))));

    let fs = SimFs::new();
    drop(open(&fs, 2));
    let error = open_with(fs, &conformance::SPECS[..4], &[])
        .err()
        .expect("other namespaces refused");
    let cause = std::error::Error::source(&error).map(ToString::to_string);
    assert!(
        cause.is_some_and(|cause| cause.contains("holds other namespaces")),
        "{error:?}"
    );
}

/// Bounds of any length over 8-byte big-endian position keys = the positions a byte compare
/// admits
#[test]
fn sequence_bounds_admit_exactly_the_positions_bytes_compare_into() {
    let len = 300u64;
    let probes: Vec<Vec<u8>> = vec![
        vec![],
        vec![0],
        vec![0, 0, 0, 0, 0, 0, 1],
        sequence_key(0),
        sequence_key(7),
        sequence_key(255),
        [sequence_key(7), vec![0]].concat(),
        [sequence_key(7), vec![9, 9]].concat(),
        sequence_key(299),
        sequence_key(500),
        vec![0xff; 9],
    ];
    let bounds =
        |key: &Vec<u8>| -> [Bound<Vec<u8>>; 2] { [Included(key.clone()), Excluded(key.clone())] };
    let mut all: Vec<Bound<Vec<u8>>> = vec![Unbounded];
    all.extend(probes.iter().flat_map(bounds));
    for lower in &all {
        for upper in &all {
            let expected: Vec<u64> = (0..len)
                .filter(|at| {
                    let key = sequence_key(*at);
                    RangeBounds::<Vec<u8>>::contains(&(lower.as_ref(), upper.as_ref()), &key)
                })
                .collect();
            let lower = lower.as_ref().map(Vec::as_slice);
            let upper = upper.as_ref().map(Vec::as_slice);
            let found: Vec<u64> = positions(lower, upper, len).collect();
            assert_eq!(found, expected, "{lower:?}..{upper:?}");
        }
    }
}

/// A scan across several durable segments (no merges: each flush its own) holding values and
/// durable tombstones, and the buffer's inserts and removals, in both directions, one entry per
/// chunk: every removal masks its value, buffered rows interleave, nothing twice
#[test]
fn map_scans_merge_durable_and_buffered_rows_chunk_by_chunk() {
    let fs = SimFs::new();
    let backend = open(&fs, 1 << 20);
    let mut model = Model::default();
    for removals in [&[][..], &[0, 3], &[1]] {
        commit(&backend, model.advance(1, &[1, 2, 3], 3, removals)).expect("commit");
        backend.flush().expect("flush");
    }
    let segments = |dir: &str| {
        let prefix = format!("{dir}/");
        let verified = backend.verify().expect("verify");
        let names = verified.units.iter().map(|unit| unit.name.as_str());
        names
            .filter(|name| name.starts_with(&prefix) && name.ends_with(".seg"))
            .count()
    };
    // a segment per flush, plus one of tombstones per flush that removed rows
    assert!(segments("scanned") >= 3, "several durable segments");
    assert!(segments("probed") >= 3, "several durable segments");
    commit(&backend, model.advance(1, &[1, 4], 2, &[0, 2, 5])).expect("buffered");
    model.assert_view(&backend.reader().expect("reader"), "durable + buffered");

    let reader = backend.reader().expect("reader");
    for namespace in [SCANNED, PROBED] {
        let whole = reader.scan(namespace).expect("scan");
        for direction in [ScanDirection::Forward, ScanDirection::Reverse] {
            let range = ScanRange {
                lower: Unbounded,
                upper: Unbounded,
                direction,
                chunk_bytes: NonZeroUsize::MIN,
            };
            let chunks: Vec<Vec<(RawKey, RawValue)>> = reader
                .scan_range(namespace, range, raw_entry)
                .collect::<Result<_, _>>()
                .expect("chunks");
            assert!(
                chunks.iter().all(|chunk| chunk.len() == 1),
                "one entry each"
            );
            let mut walked: Vec<_> = chunks.concat();
            if direction == ScanDirection::Reverse {
                walked.reverse();
            }
            assert_eq!(walked, whole, "{namespace} {direction:?}");
        }
    }
}

/// A reader pinned before a merge lands (its inputs removed from disk, the list swapped) keeps
/// answering from its snapshot, a scan started on it included
#[test]
fn a_pinned_reader_outlives_a_landed_merge() {
    let fs = SimFs::new();
    let backend = open(&fs, 2);
    let mut model = Model::default();
    for _ in 0..2 {
        commit(&backend, model.advance(1, &[1, 2], 2, &[])).expect("commit");
        backend.flush().expect("flush");
    }
    let pinned_model = model.clone();
    let pinned = backend.reader().expect("reader");
    let range = ScanRange {
        lower: Unbounded,
        upper: Unbounded,
        direction: ScanDirection::Forward,
        chunk_bytes: NonZeroUsize::MIN,
    };
    let mut chunks = pinned.scan_range(SCANNED, range, raw_entry);
    let first = chunks.next().expect("rows").expect("chunk");

    let listed = || {
        let verified = backend.verify().expect("verify");
        let names = verified.units.iter().map(|unit| unit.name.as_str());
        names.filter(|name| name.starts_with("scanned/")).count()
    };
    let before = listed();
    eventually("a scanned merge landed", || {
        commit(&backend, model.advance(0, &[], 0, &[])).expect("commit");
        backend.flush().expect("flush");
        (listed() < before).then_some(())
    });

    pinned_model.assert_view(&pinned, "pinned before the merge landed");
    let rest: Vec<_> = chunks.collect::<Result<_, _>>().expect("chunks");
    let walked = [vec![first], rest].concat().concat();
    assert_eq!(
        walked,
        pinned.scan(SCANNED).expect("scan"),
        "the scan's snapshot"
    );
    model.assert_view(&backend.reader().expect("reader"), "a new reader: merged");
}

/// A key inserted and removed between two writes: re-inserting it is refused while buffered,
/// once written, after a reopen, and accepted only once a merge has cancelled the pair
#[test]
fn a_removal_is_forgotten_only_when_a_merge_cancels_its_pair() {
    let fs = SimFs::new();
    let key = conformance::scanned_key(1, 0);
    let put = |value: u8| {
        vec![WriteOp::Put {
            namespace: SCANNED,
            key: key.clone(),
            value: vec![value; 8],
        }]
    };
    let removed = vec![WriteOp::Delete {
        namespace: SCANNED,
        key: key.clone(),
    }];
    let refused = |backend: &LsmTreeBackend, at: usize, label: &str| {
        let reinsert = commit(backend, (conformance::block_ref(at), put(2)));
        assert!(
            matches!(reinsert, Err(CommitError::KeyRemoved { .. })),
            "{label}: {reinsert:?}"
        );
    };

    let backend = open(&fs, 2);
    commit(&backend, (conformance::block_ref(1), put(1))).expect("insert");
    commit(&backend, (conformance::block_ref(2), removed)).expect("remove");
    refused(&backend, 3, "buffered");
    backend.flush().expect("flush");
    refused(&backend, 3, "written");
    drop(backend);

    let backend = open(&fs, 2);
    refused(&backend, 3, "reopened");
    let reader = || backend.reader().expect("reader");
    assert_eq!(reader().get(SCANNED, &key).expect("get"), None);

    let mut at = 2;
    eventually("the merge cancelled the pair", || {
        at += 1;
        if commit(&backend, (conformance::block_ref(at), put(2))).is_ok() {
            return Some(());
        }
        commit(&backend, (conformance::block_ref(at), Vec::new())).expect("an empty commit");
        backend.flush().expect("flush: lands finished merges");
        None
    });
    assert_eq!(reader().get(SCANNED, &key).expect("get"), Some(vec![2; 8]));
    backend.flush().expect("flush");
    let verified = backend.verify().expect("verify");
    assert!(verified.is_clean(), "{verified:?}");
}

/// The scope changed between opens (none → 8 → 4 → none): segments written under the old one are
/// rewritten before the map serves, every row read as committed; every crash state of the
/// rewrites reopens to every row
#[test]
fn a_changed_scope_rebuilds_filters_and_loses_no_row() {
    let open_scoped = |fs: Arc<SimFs>, scope: u32| {
        let tuning = [(
            SCANNED,
            Tuning {
                scope,
                cache_writes: false,
            },
        )];
        let scoped = LsmTreeConfig {
            tuning: &tuning,
            ..config(1 << 20, NonZeroUsize::MAX)
        };
        LsmTreeBackend::open(fs, Path::new(ROOT), &scoped).expect("open")
    };
    let fs = SimFs::recording();
    let mut model = Model::default();
    {
        let backend = open_scoped(fs.clone(), 0);
        for _ in 0..3 {
            commit(&backend, model.advance(1, &[1, 2, 3], 2, &[0])).expect("commit");
            backend.flush().expect("flush");
        }
    }
    fs.set_tag(1);
    for scope in [8, 4, 0] {
        let backend = open_scoped(fs.clone(), scope);
        model.assert_view(
            &backend.reader().expect("reader"),
            &format!("scope {scope}"),
        );
        let verified = backend.verify().expect("verify");
        assert!(verified.is_clean(), "scope {scope}: {verified:?}");
    }
    let rescoping: Vec<_> = fs
        .crash_states()
        .into_iter()
        .filter(|state| state.tag == 1)
        .collect();
    assert!(
        !rescoping.is_empty(),
        "crash states recorded during the rewrites"
    );
    for state in rescoping {
        let backend = open_scoped(state.fs.clone(), 8);
        model.assert_view(&backend.reader().expect("reader"), &state.label);
    }
}

/// Removing merge-replaced files failing after a durable commit: commits still succeed, are
/// visible and survive a reopen; the files stay and are retried at each later write, removed once
/// removals work again; an open that can't remove them is refused
#[test]
fn a_failed_cleanup_never_fails_a_commit_and_is_retried() {
    let fs = SimFs::new();
    // a leftover: a segment once listed, no longer listed, still on disk (an unlisted file never
    // listed is a merge output still being written)
    let ever_listed = std::cell::RefCell::new(std::collections::BTreeSet::new());
    let leftovers = || -> Vec<String> {
        let listed: Vec<String> = verify_dir(
            fs.as_ref(),
            Path::new(ROOT),
            "conformance",
            1,
            &conformance::SPECS,
        )
        .expect("verify")
        .units
        .into_iter()
        .filter_map(|unit| unit.name.strip_prefix("scanned/").map(str::to_owned))
        .collect();
        ever_listed.borrow_mut().extend(listed.iter().cloned());
        fs.list(&Path::new(ROOT).join("scanned"))
            .expect("list")
            .into_iter()
            .filter(|name| ever_listed.borrow().contains(name) && !listed.contains(name))
            .collect()
    };
    let mut model = Model::default();
    let write_until_leftover = |backend: &LsmTreeBackend, model: &mut Model| -> Vec<String> {
        eventually("a merge landed, its inputs left over", || {
            commit(backend, model.advance(0, &[1], 0, &[])).expect("commits succeed");
            backend.flush().expect("flushes succeed");
            Some(leftovers()).filter(|left| !left.is_empty())
        })
    };

    let backend = open(&fs, 2);
    fs.fail_removes(true);
    write_until_leftover(&backend, &mut model);
    model.assert_view(
        &backend.reader().expect("reader"),
        "committed despite the leftovers",
    );
    fs.fail_removes(false);
    commit(&backend, model.advance(1, &[], 0, &[])).expect("commit");
    backend.flush().expect("flush");
    assert_eq!(
        leftovers(),
        Vec::<String>::new(),
        "retried at the next write"
    );

    fs.fail_removes(true);
    let left = write_until_leftover(&backend, &mut model);
    drop(backend);
    let refused = LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &config(2, NonZeroUsize::MAX));
    assert!(
        refused.is_err(),
        "an open that can't remove unlisted files is refused"
    );
    fs.fail_removes(false);
    let backend = open(&fs, 2);
    model.assert_view(&backend.reader().expect("reader"), "durable");
    let on_disk = fs.list(&Path::new(ROOT).join("scanned")).expect("list");
    assert!(
        left.iter().all(|name| !on_disk.contains(name)),
        "the open removed {left:?}: {on_disk:?}"
    );
}

/// Every log event: its level, message and `path` field
#[derive(Clone, Default)]
struct Events(Arc<std::sync::Mutex<Vec<(tracing::Level, String, String)>>>);

impl Events {
    fn logged(&self, level: tracing::Level, message: &str) -> usize {
        let events = self.0.lock().expect("events");
        events
            .iter()
            .filter(|(at, text, _)| *at == level && text == message)
            .count()
    }

    /// Events logging `message`, counted per `path`
    fn per_path(&self, message: &str) -> std::collections::HashMap<String, usize> {
        let mut counted = std::collections::HashMap::new();
        for (_, text, path) in self.0.lock().expect("events").iter() {
            if text == message {
                *counted.entry(path.clone()).or_insert(0) += 1;
            }
        }
        counted
    }
}

impl tracing::Subscriber for Events {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Fields(String, String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.0 = format!("{value:?}"),
                    "path" => self.1 = format!("{value:?}"),
                    _ => {}
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let level = *event.metadata().level();
        self.0
            .lock()
            .expect("events")
            .push((level, fields.0, fields.1));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Gauges set since the last call, `(name, label value) → value` (a snapshot drains them: read
/// every value a check needs from one)
fn gauges(
    snapshotter: &metrics_util::debugging::Snapshotter,
) -> std::collections::HashMap<(String, String), f64> {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .filter_map(|(key, _, _, value)| {
            let key = key.key();
            let label = key.labels().next()?.value().to_owned();
            match value {
                metrics_util::debugging::DebugValue::Gauge(value) => {
                    Some(((key.name().to_owned(), label), value.0))
                }
                _ => None,
            }
        })
        .collect()
}

/// `name`'s value for `label` in `set`
fn gauge(set: &std::collections::HashMap<(String, String), f64>, name: &str, label: &str) -> f64 {
    *set.get(&(name.to_owned(), label.to_owned()))
        .unwrap_or_else(|| panic!("no {name} gauge for {label} in {set:?}"))
}

/// As an operator sees it: removals failing after a commit log a warning per retry, then the
/// leftover as a warning below `cleanup_alert_bytes` and an error at it, the leftover bytes
/// gauge rises and falls back to 0 once removals work; the snapshot gauges count the states
/// alive (the published one + one a reader holds) and their buffer heap
#[test]
fn cleanup_alerts_escalate_and_gauges_track_leftovers_and_snapshots() {
    const RETRY: &str = "Removing a merged-away file failed; retrying";
    const WARNED: &str = "Merged-away files not removed; retrying at the next write";
    const ALERTED: &str = "Merged-away files can't be removed: the disk may be failing";
    for (alert_bytes, logged, not_logged) in [
        (
            u64::MAX,
            (tracing::Level::WARN, WARNED),
            (tracing::Level::ERROR, ALERTED),
        ),
        (
            1,
            (tracing::Level::ERROR, ALERTED),
            (tracing::Level::WARN, WARNED),
        ),
    ] {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let events = Events::default();
        let dispatch = tracing::Dispatch::new(events.clone());
        tracing::dispatcher::with_default(&dispatch, || {
            metrics::with_local_recorder(&recorder, || {
                let fs = SimFs::new();
                let base = config(2, NonZeroUsize::MAX);
                let tuned = LsmTreeConfig {
                    lsm: LsmConfig {
                        cleanup_alert_bytes: alert_bytes,
                        ..base.lsm
                    },
                    ..base
                };
                let backend =
                    LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &tuned).expect("open");
                const LIVE: &str = "zaino.store.snapshots_live";
                const HEAP: &str = "zaino.store.snapshot_heap_bytes";
                const LEFTOVER: &str = "zaino.lsm.leftover_bytes";
                let set = gauges(&snapshotter);
                assert_eq!(gauge(&set, LIVE, "conformance"), 1.0, "the published state");

                let mut model = Model::default();
                let held = backend.reader().expect("reader");
                commit(&backend, model.advance(1, &[1], 1, &[])).expect("commit");
                let set = gauges(&snapshotter);
                assert_eq!(
                    gauge(&set, LIVE, "conformance"),
                    2.0,
                    "the published state + the one a reader holds"
                );
                assert!(
                    gauge(&set, HEAP, "conformance") > 0.0,
                    "the buffer's heap counted"
                );
                drop(held);
                let set = gauges(&snapshotter);
                assert_eq!(
                    gauge(&set, LIVE, "conformance"),
                    1.0,
                    "a dropped reader's state freed"
                );

                fs.fail_removes(true);
                eventually("a merge landed, its inputs left over", || {
                    commit(&backend, model.advance(0, &[1], 0, &[])).expect("commit");
                    backend.flush().expect("flush");
                    (gauge(&gauges(&snapshotter), LEFTOVER, "scanned") > 0.0).then_some(())
                });
                assert!(
                    events.logged(tracing::Level::WARN, RETRY) >= 3,
                    "a warning per retry"
                );
                assert!(events.logged(logged.0, logged.1) > 0, "{logged:?}");
                assert_eq!(
                    events.logged(not_logged.0, not_logged.1),
                    0,
                    "{not_logged:?}"
                );

                fs.fail_removes(false);
                commit(&backend, model.advance(1, &[], 0, &[])).expect("commit");
                backend.flush().expect("flush");
                assert_eq!(
                    gauge(&gauges(&snapshotter), LEFTOVER, "scanned"),
                    0.0,
                    "removed at the next write"
                );
            });
        });
    }
}

/// A file is retried with pauses only by the write that replaced it; later writes try it once,
/// without waiting
#[test]
fn only_the_write_that_replaced_a_file_retries_its_removal() {
    const RETRY: &str = "Removing a merged-away file failed; retrying";
    let events = Events::default();
    let dispatch = tracing::Dispatch::new(events.clone());
    tracing::dispatcher::with_default(&dispatch, || {
        let fs = SimFs::new();
        let backend = open(&fs, 2);
        let mut model = Model::default();
        fs.fail_removes(true);
        eventually("a merge landed, its inputs left over", || {
            commit(&backend, model.advance(0, &[1], 0, &[])).expect("commit");
            backend.flush().expect("flush");
            (events.logged(tracing::Level::WARN, RETRY) > 0).then_some(())
        });
        for _ in 0..3 {
            commit(&backend, model.advance(1, &[1], 1, &[])).expect("commit");
            backend.flush().expect("flush");
        }
        let retried = events.per_path(RETRY);
        let max = usize::try_from(LsmConfig::default().max_retries).expect("fits");
        assert!(
            retried.values().all(|count| *count == max),
            "each path retried by one write only: {retried:?}"
        );
    });
}

/// A merge's inputs deleted while a reader still maps them: counted as unlinked-mapped bytes
/// until that reader drops
#[test]
fn deleted_segments_a_reader_still_maps_are_counted_until_it_drops() {
    const UNLINKED: &str = "zaino.lsm.unlinked_mapped_bytes";
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    // raised and lowered by deltas, and a snapshot swaps each gauge to 0: the running sum is
    // its value
    let mut unlinked = 0.0;
    let mut read = || {
        unlinked += gauges(&snapshotter)
            .get(&(UNLINKED.to_owned(), "scanned".to_owned()))
            .copied()
            .unwrap_or(0.0);
        unlinked
    };
    metrics::with_local_recorder(&recorder, || {
        let fs = SimFs::new();
        let backend = open(&fs, 2);
        let mut model = Model::default();
        commit(&backend, model.advance(0, &[1], 0, &[])).expect("commit");
        backend.flush().expect("flush");
        let pinned = backend.reader().expect("reader");
        eventually("a merge landed under the pinned reader", || {
            commit(&backend, model.advance(0, &[2], 0, &[])).expect("commit");
            backend.flush().expect("flush");
            (read() > 0.0).then_some(())
        });
        drop(pinned);
        assert_eq!(read(), 0.0, "freed with the last reader");
    });
}

/// The manifest's 1 MiB slot outgrown by a map's segment list: `ManifestFull`, writes poisoned,
/// a reopen at the last commit that fit
#[test]
fn a_write_past_the_manifest_slot_is_manifest_full_and_nothing_moves() {
    let width = |n| std::num::NonZeroU32::new(n).expect("non-zero");
    let map = Namespace::new("map");
    // 277 manifest bytes per sequence (declaration + seal): 3,784 leave ~300 of the slot's bytes,
    // about 9 of the map's 32-byte segment entries
    let mut specs: Vec<NamespaceSpec> = (0..3_784)
        .map(|n| {
            let name: &'static str = Box::leak(format!("{n:0>255}").into_boxed_str());
            NamespaceSpec::sequence(Namespace::new(name), Width::Fixed(width(1)))
        })
        .collect();
    specs.push(NamespaceSpec::map(map, width(8), width(1), false));
    let fs = SimFs::new();
    let open = || {
        let declared = LsmTreeConfig {
            namespaces: &specs,
            tuning: &[],
            ..config(1 << 20, NonZeroUsize::MAX)
        };
        LsmTreeBackend::open(fs.clone(), Path::new(ROOT), &declared).expect("open")
    };
    let backend = open();
    let put = |n: u64| {
        vec![WriteOp::Put {
            namespace: map,
            key: n.to_be_bytes().to_vec(),
            value: vec![1],
        }]
    };
    let mut fitted = 0;
    let full = loop {
        let n = fitted + 1;
        assert!(n < 100, "the slot never filled");
        commit(&backend, (conformance::block_ref(n), put(n as u64))).expect("buffered");
        match backend.flush() {
            Ok(()) => fitted = n,
            Err(error) => break error,
        }
    };
    assert!(fitted > 0, "some segments fit before the slot filled");
    assert!(
        matches!(full, FlushError::Write(CommitError::ManifestFull)),
        "{full:?}"
    );
    let poisoned = commit(&backend, (conformance::block_ref(fitted + 2), Vec::new()));
    assert!(
        matches!(poisoned, Err(CommitError::Poisoned)),
        "{poisoned:?}"
    );
    drop(backend);

    let backend = open();
    let reader = backend.reader().expect("reader");
    assert_eq!(
        reader.tip().expect("tip"),
        Some(conformance::block_ref(fitted)),
        "the last commit that fit"
    );
    let written = reader.scan(map).expect("scan");
    assert_eq!(written.len(), fitted, "every row that fit, none after");
    let verified = backend.verify().expect("verify");
    assert!(verified.is_clean(), "{verified:?}");
}

/// `verify_dir` (offline) agrees with the backend's own `verify`; footprint covers every
/// namespace, cached unknowable on `SimFs`
#[test]
fn offline_verify_agrees_and_footprint_names_every_namespace() {
    let fs = SimFs::new();
    let backend = open(&fs, 2);
    commit(&backend, Model::default().advance(2, &[1], 1, &[])).expect("commit");
    backend.flush().expect("flush");
    let online = backend.verify().expect("verify");
    let offline = verify_dir(
        fs.as_ref(),
        Path::new(ROOT),
        "conformance",
        1,
        &conformance::SPECS,
    )
    .expect("verify_dir");
    assert!(online.is_clean() && !online.units.is_empty(), "{online:?}");
    assert_eq!(online, offline);
    let footprint = backend.footprint().expect("footprint");
    assert_eq!(footprint.len(), conformance::SPECS.len());
    assert!(footprint.iter().all(|namespace| namespace.cached.is_none()));
    assert!(footprint.iter().any(|namespace| namespace.bytes > 0));
}
