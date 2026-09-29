//! `LsmStore` end to end: `BTreeMap` model, every crash state, every failed I/O call, planted bugs

use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::Arc,
};

use proptest::{prelude::*, strategy::Union};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zcash_protocol::consensus::NetworkType;

use super::{
    file_name, writer::SegmentWriter, Key, LsmIndex, LsmStore, Record, SegmentError, SegmentLog,
    SegmentSet, Snapshot,
};
use crate::{
    fs::{Fs, SimFs},
    manifest::IndexKind,
    pages::{sums_path, PageError},
};

const NET: NetworkType = NetworkType::Regtest;

/// `scanned` = range-read rows (`owner ‖ seq`), `probed` = filtered point lookups (hash-like id)
struct TestIndex<const FANOUT: usize>;

impl<const FANOUT: usize> LsmIndex for TestIndex<FANOUT> {
    const KIND: IndexKind = IndexKind::TransparentAddress;
    const FORMAT: u16 = 1;
    const FANOUT: usize = FANOUT;
    const SETS: &'static [&'static str] = &["scanned", "probed"];
    type Logs = (SegmentLog<Row>, SegmentLog<IdRow>);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OwnerAt {
    owner: [u8; 4],
    seq: u32,
}

impl Key for OwnerAt {
    const LEN: usize = 8;

    fn encode(&self) -> Vec<u8> {
        [&self.owner[..], &self.seq.to_be_bytes()].concat()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            owner: bytes.get(..4)?.try_into().ok()?,
            seq: u32::from_be_bytes(bytes.get(4..8)?.try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    at: OwnerAt,
    value: u64,
}

impl Record for Row {
    type Key = OwnerAt;
    const STRIDE: usize = 16;

    fn key(&self) -> OwnerAt {
        self.at
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.at.encode());
        out.extend_from_slice(&self.value.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            at: OwnerAt::decode(bytes)?,
            value: u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Id([u8; 16]);

impl Key for Id {
    const LEN: usize = 16;
    const PROBED: bool = true;

    fn encode(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self(bytes.get(..16)?.try_into().ok()?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdRow {
    id: Id,
    at: u32,
}

impl Record for IdRow {
    type Key = Id;
    const STRIDE: usize = 20;

    fn key(&self) -> Id {
        self.id
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.0);
        out.extend_from_slice(&self.at.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            id: Id::decode(bytes)?,
            at: u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?),
        })
    }
}

/// Uniform first 8 bytes (the filter shards on them), `n` in the last 4 (distinct per `n`)
fn id_row(n: u32) -> IdRow {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    IdRow { id: Id(id), at: n }
}

fn row(owner: u8, seq: u32) -> Row {
    Row { at: OwnerAt { owner: [owner; 4], seq }, value: u64::from(seq) * 1000 + u64::from(owner) }
}

/// Commit `n` (from 1) covers heights 0 to `n - 1`, both inclusive, tipped by `[n; 32]`
fn commit_point(n: usize) -> BlockRef {
    let height = Height::try_from(n as u32 - 1).expect("small height");
    BlockRef { hash: BlockHash::from([n as u8; 32]), height }
}

const EVERY_OWNER: (OwnerAt, OwnerAt) =
    (OwnerAt { owner: [0; 4], seq: 0 }, OwnerAt { owner: [0xff; 4], seq: u32::MAX });

/// Panic payload text (`panic!` with arguments → `String`, a literal → `&str`)
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => {
            payload.downcast::<&str>().map(|message| message.to_string()).unwrap_or_default()
        }
    }
}

/// - `Commit`: one `scanned` row per listed owner (fresh seqs), `ids` fresh `probed` ids
/// - `Settle`: merges finish (land next commit); `Reopen`: exit mid-merge; `PowerLoss`: crash now
/// - `Pin`: views + model kept, re-checked every later step; `Scan` / `Probe`: raw bounds / ids,
///   reduced modulo what was issued at run time
#[derive(Debug, Clone)]
enum Step {
    Commit { owners: Vec<u8>, ids: u16 },
    Settle,
    Reopen,
    PowerLoss,
    Pin,
    Scan { from: (u8, u32), to: (u8, u32), limit: u16 },
    Probe { ids: Vec<u32> },
}

/// Swarm testing (TigerBeetle `tree_fuzz`): whole step kinds off per case (a uniform mix dilutes
/// rare interleavings: all reopens, no settles, only small batches, …)
fn steps() -> impl Strategy<Value = Vec<Step>> {
    let on = prop::array::uniform6(prop::bool::ANY);
    on.prop_flat_map(|[settle, reopen, power_loss, pin, query, large]| {
        // small batches; with `large`, sometimes past a 4 KiB block (256 scanned / 204 probed)
        let small = prop::collection::vec(0u8..4, 0..8);
        let (owners, ids) = match large {
            true => {
                let big = prop::collection::vec(0u8..4, 250..600);
                (
                    prop_oneof![4 => small, 1 => big].boxed(),
                    prop_oneof![4 => 0u16..8, 1 => 200u16..600].boxed(),
                )
            }
            false => (small.boxed(), (0u16..8).boxed()),
        };
        let commit = (owners, ids).prop_map(|(owners, ids)| Step::Commit { owners, ids }).boxed();
        let bound = (0u8..6, any::<u32>());
        let scan = (bound.clone(), bound, 0u16..64).prop_map(|(from, to, limit)| Step::Scan {
            from,
            to,
            limit,
        });
        let probe = prop::collection::vec(any::<u32>(), 0..24).prop_map(|ids| Step::Probe { ids });
        let mut kinds = vec![(6, commit)];
        for (enabled, weight, kind) in [
            (settle, 1, Just(Step::Settle).boxed()),
            (reopen, 1, Just(Step::Reopen).boxed()),
            (power_loss, 1, Just(Step::PowerLoss).boxed()),
            (pin, 1, Just(Step::Pin).boxed()),
            (query, 2, scan.boxed()),
            (query, 2, probe.boxed()),
        ] {
            if enabled {
                kinds.push((weight, kind));
            }
        }
        prop::collection::vec(Union::new_weighted(kinds), 1..40)
    })
}

proptest! {
    // 64 cases ≈ 1.5 s idle, ~7 s on a loaded box; heavy run after any change: root CLAUDE.md
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// After every step: every query = the model (pinned views = the model when pinned), extent +
    /// tip = the last commit, tiers within their stall bound, a (re)open leaves only listed files
    #[test]
    fn random_histories_answer_like_a_btreemap_through_merges_reopens_and_power_loss(
        fanout in prop_oneof![Just(2usize), Just(3), Just(8)],
        steps in steps(),
    ) {
        match fanout {
            2 => random_history::<2>(&steps),
            3 => random_history::<3>(&steps),
            _ => random_history::<8>(&steps),
        }
    }
}

/// Every row the store acknowledged (all durable: a commit returns after its manifest fsync)
#[derive(Debug, Clone, Default)]
struct Model {
    scanned: BTreeMap<OwnerAt, Row>,
    probed: BTreeMap<Id, IdRow>,
    commits: usize,
    seq: u32,
    ids: u32,
}

impl Model {
    /// `owner` + `raw` seq reduced into `0` to `seq + 1`, both inclusive (lands on, between and past
    /// issued rows)
    fn bound(&self, (owner, raw): (u8, u32)) -> OwnerAt {
        OwnerAt { owner: [owner; 4], seq: raw % (self.seq + 2) }
    }

    /// `start` inclusive to `end` exclusive, empty when `start >= end` (`BTreeMap::range` panics
    /// there)
    fn scan(&self, start: OwnerAt, end: OwnerAt) -> Vec<Row> {
        match start < end {
            true => self.scanned.range(start..end).map(|(_, row)| *row).collect(),
            false => Vec::new(),
        }
    }

    /// `raw` ids reduced into every id issued + as many never issued
    fn ids(&self, raw: &[u32]) -> Vec<Id> {
        raw.iter().map(|&n| id_row(n % (2 * self.ids + 2)).id).collect()
    }

    /// Full scan, per-owner scans, sampled gets + a miss, get_many / get over every id ever issued
    /// and as many never issued (each asked twice: answers in caller order)
    fn assert_views(&self, scanned: &Snapshot<OwnerAt>, probed: &Snapshot<Id>, label: &str) {
        let every: Vec<Row> = self.scanned.values().copied().collect();
        assert_eq!(scanned.range::<Row>(&EVERY_OWNER.0, &EVERY_OWNER.1), every, "{label}: scan");
        for owner in [0u8, 3, 9] {
            let (from, to) = (
                OwnerAt { owner: [owner; 4], seq: 0 },
                OwnerAt { owner: [owner; 4], seq: u32::MAX },
            );
            assert_eq!(
                scanned.range::<Row>(&from, &to),
                self.scan(from, to),
                "{label}: owner {owner}"
            );
        }
        for (key, row) in self.scanned.iter().step_by(7) {
            assert_eq!(scanned.get::<Row>(key), Some(*row), "{label}: get {key:?}");
        }
        let unissued = OwnerAt { owner: [1; 4], seq: self.seq + 1 };
        assert_eq!(scanned.get::<Row>(&unissued), None, "{label}: get past the last seq");

        let asked: Vec<Id> = (0..2 * self.ids + 2).flat_map(|n| [id_row(n).id; 2]).collect();
        let answers: Vec<Option<IdRow>> =
            asked.iter().map(|id| self.probed.get(id).copied()).collect();
        assert_eq!(probed.get_many::<IdRow>(&asked), answers, "{label}: get_many");
        let singles: Vec<Option<IdRow>> = asked.iter().map(|id| probed.get::<IdRow>(id)).collect();
        assert_eq!(singles, answers, "{label}: get");
    }
}

/// Manifest = the model's last commit; each tier within `fanout` inputs + `STALL_WINDOWS` (2)
/// idle windows; with `files` (just opened): every listed segment + sums on disk, beyond them
/// only running merges' outputs (open launches merges)
fn assert_store<const FANOUT: usize>(
    store: &LsmStore<TestIndex<FANOUT>>,
    model: &Model,
    fs: &SimFs,
    files: bool,
    label: &str,
) {
    let expected = (model.commits > 0).then(|| commit_point(model.commits));
    assert_eq!(store.committed().tip, expected, "{label}");

    let logs = store.logs();
    let sets = [
        ("scanned", logs.0.segments(), logs.0.merging()),
        ("probed", logs.1.segments(), logs.1.merging()),
    ];
    for (set, listed, merging) in sets {
        let mut per_tier = BTreeMap::<u32, usize>::new();
        for segment in listed {
            *per_tier.entry(segment.records.ilog(FANOUT as u64)).or_default() += 1;
        }
        let bounded = per_tier.values().all(|&segments| segments < 3 * FANOUT);
        assert!(bounded, "{label}: {set} segments per tier {per_tier:?}");
        if files {
            let on_disk = fs.list(&Path::new("/idx").join(set)).expect("list");
            for segment in listed {
                let name = file_name(segment.id);
                let present =
                    [name.clone(), format!("{name}.crc")].iter().all(|n| on_disk.contains(n));
                assert!(present, "{label}: {set} lost listed {name}: {on_disk:?}");
            }
            let extra = on_disk.len() - 2 * listed.len();
            assert!(
                extra <= 2 * merging,
                "{label}: {set} unlisted files {on_disk:?}, {merging} merges"
            );
        }
    }
}

/// One history against one store: apply each step to both, then compare everything
fn random_history<const FANOUT: usize>(steps: &[Step]) {
    let root = Path::new("/idx");
    let mut fs = SimFs::new();
    let mut store = LsmStore::<TestIndex<FANOUT>>::open(fs.clone(), root, NET).expect("open");
    let mut model = Model::default();
    let mut pinned: Option<Pinned> = None;

    for (at, step) in steps.iter().enumerate() {
        let label = format!("fanout {FANOUT}, step {at} {step:?}");
        let (scanned, probed) = store.sets();
        let (scanned, probed) = (scanned.pin(), probed.pin());
        let mut reopened = false;
        match step {
            Step::Commit { owners, ids } => {
                let rows: Vec<Row> = owners
                    .iter()
                    .zip(model.seq + 1..)
                    .map(|(&owner, seq)| row(owner, seq))
                    .collect();
                let fresh: Vec<IdRow> =
                    (model.ids..model.ids + u32::from(*ids)).map(id_row).collect();
                let tip = commit_point(model.commits + 1);
                store.commit((rows.clone(), fresh.clone()), tip).expect("commit");
                model.commits += 1;
                model.seq += rows.len() as u32;
                model.ids += u32::from(*ids);
                model.scanned.extend(rows.into_iter().map(|row| (row.at, row)));
                model.probed.extend(fresh.into_iter().map(|row| (row.id, row)));
            }
            Step::Settle => {
                store.logs().0.settle();
                store.logs().1.settle();
            }
            Step::Reopen => {
                drop(store);
                store = LsmStore::open(fs.clone(), root, NET).expect("reopen");
                reopened = true;
            }
            // image taken while merges may still be writing; acknowledged = durable, so nothing
            // rolls back
            Step::PowerLoss => {
                let crashed = fs.power_loss();
                drop(store);
                fs = crashed;
                store = LsmStore::open(fs.clone(), root, NET).expect("open after power loss");
                reopened = true;
            }
            Step::Pin => pinned = Some(Pinned { at, model: model.clone(), scanned, probed }),
            Step::Scan { from, to, limit } => {
                let (from, to) = (model.bound(*from), model.bound(*to));
                let expected = model.scan(from, to);
                assert_eq!(scanned.range::<Row>(&from, &to), expected, "{label}: {from:?}..{to:?}");
                // over the budget = `None`, never a truncated answer; both sides of the edge + one
                // random limit
                let len = expected.len();
                let random = usize::from(*limit) % (len + 2);
                for limit in [len.saturating_sub(1), len, len + 1, random] {
                    let bounded = (len <= limit).then(|| expected.clone());
                    let answer = scanned.range_at_most::<Row>(&from, &to, limit);
                    assert_eq!(answer, bounded, "{label}: {from:?}..{to:?} at most {limit}");
                }
            }
            Step::Probe { ids } => {
                let asked = model.ids(ids);
                let answers: Vec<Option<IdRow>> =
                    asked.iter().map(|id| model.probed.get(id).copied()).collect();
                assert_eq!(probed.get_many::<IdRow>(&asked), answers, "{label}: get_many");
                let singles: Vec<Option<IdRow>> =
                    asked.iter().map(|id| probed.get::<IdRow>(id)).collect();
                assert_eq!(singles, answers, "{label}: get");
            }
        }

        assert_store(&store, &model, &fs, reopened, &label);
        let (scanned, probed) = store.sets();
        model.assert_views(&scanned.pin(), &probed.pin(), &label);
        if let Some(pinned) = &pinned {
            let pinned_label = format!("{label}: view pinned at step {}", pinned.at);
            pinned.model.assert_views(&pinned.scanned, &pinned.probed, &pinned_label);
        }
    }
}

/// Views taken at step `at`, and the model they must keep answering (commits, merges, reopens and
/// power loss since then change nothing a pinned view sees)
struct Pinned {
    at: usize,
    model: Model,
    scanned: Arc<Snapshot<OwnerAt>>,
    probed: Arc<Snapshot<Id>>,
}

/// Fanout 2: merges launch on commits 2-3, land on 3-4; per state: rows of recovered commits all
/// present, later ones all absent, only listed files, commits continue
#[test]
fn every_crash_state_reopens_to_exactly_an_acknowledged_or_the_attempted_commit() {
    let fs = SimFs::recording();
    let root = Path::new("/idx");
    let commits: [(Vec<Row>, Vec<IdRow>); 4] = [
        (vec![row(1, 0), row(2, 1), row(1, 2)], vec![id_row(0), id_row(1)]),
        (vec![row(1, 3), row(3, 4)], vec![id_row(2)]),
        (vec![], vec![id_row(3)]),
        (vec![row(4, 5)], vec![]),
    ];
    {
        let mut store = LsmStore::<TestIndex<2>>::open(fs.clone(), root, NET).expect("open");
        for (acked, rows) in (1u64..).zip(&commits) {
            store.logs().0.settle();
            store.logs().1.settle();
            store.commit(rows.clone(), commit_point(acked as usize)).expect("commit");
            fs.set_tag(acked);
        }
        let merged = (store.logs().0.segments().len(), store.logs().1.segments().len());
        assert_eq!(merged, (2, 2), "scanned 2+2 rows merged, probed 1+1 rows merged");
    }

    let states = fs.crash_states();
    assert!(states.len() > 50, "enumerated {} crash states", states.len());
    for state in states {
        let label = &state.label;
        let mut store = LsmStore::<TestIndex<2>>::open(state.fs.clone(), root, NET)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        let recovered = store.committed().count() as usize;
        let acked = usize::try_from(state.tag).expect("small");
        assert!(recovered == acked || recovered == acked + 1, "{label}: recovered {recovered}");
        assert_eq!(store.committed().tip, (recovered > 0).then(|| commit_point(recovered)));

        // recovered commits' rows all present, every later commit's rows all absent
        let (scanned, probed) = store.sets();
        let (scanned, probed) = (scanned.pin(), probed.pin());
        for (n, (rows, ids)) in commits.iter().enumerate() {
            let durable = n < recovered;
            for row in rows {
                assert_eq!(scanned.get::<Row>(&row.at).is_some(), durable, "{label}: {row:?}");
            }
            for id in ids {
                assert_eq!(probed.get::<IdRow>(&id.id).is_some(), durable, "{label}: {id:?}");
            }
        }
        let mut rows: Vec<Row> =
            commits[..recovered].iter().flat_map(|(rows, _)| rows.clone()).collect();
        rows.sort_by_key(|row| row.key());
        assert_eq!(
            scanned.range::<Row>(&EVERY_OWNER.0, &EVERY_OWNER.1),
            rows,
            "{label}: nothing else"
        );
        // open removed every unlisted file; beyond the listed ones only merges open launched
        let logs = store.logs();
        let sets = [
            ("scanned", logs.0.segments().len(), logs.0.merging()),
            ("probed", logs.1.segments().len(), logs.1.merging()),
        ];
        for (set, listed, merging) in sets {
            let files = state.fs.list(&root.join(set)).expect("list").len();
            let within = (2 * listed..=2 * (listed + merging)).contains(&files);
            assert!(within, "{label}: {set}: {files} files, {listed} listed, {merging} merging");
        }

        store
            .commit((vec![row(9, 99)], vec![id_row(99)]), commit_point(recovered + 1))
            .expect("commit after recovery");
        let next = store.sets().1.pin().get::<IdRow>(&id_row(99).id);
        assert_eq!(next, Some(id_row(99)), "{label}: commits continue at the recovered end");
    }
}

/// Op 0, 1, 2, … failed until the workload succeeds (Pebble `errorfs`); each = the injected `Err`
/// (never a panic, never swallowed), later commits refused, restart = acked or attempted commit
#[test]
fn every_failed_io_call_surfaces_poisons_the_store_and_recovers_to_a_committed_state() {
    let root = Path::new("/idx");
    let commits: [(Vec<Row>, Vec<IdRow>); 3] = [
        (vec![row(1, 0), row(2, 1)], vec![id_row(0)]),
        (vec![row(1, 2)], vec![id_row(1)]),
        (vec![row(3, 3)], vec![id_row(2), id_row(3)]),
    ];
    let mut failures = 0;
    for fail_at in 0.. {
        assert!(fail_at < 10_000, "workload never succeeded");
        let fs = SimFs::new();
        fs.fail_from(fail_at);
        let mut acked = 0;
        let error = match LsmStore::<TestIndex<2>>::open(fs.clone(), root, NET) {
            Err(error) => error,
            Ok(mut store) => {
                let mut failed = None;
                for (at, rows) in commits.iter().enumerate() {
                    store.logs().0.settle();
                    store.logs().1.settle();
                    match store.commit(rows.clone(), commit_point(at + 1)) {
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
                        store.logs().0.settle();
                        store.logs().1.settle();
                        if fs.mutations() <= fail_at {
                            break;
                        }
                        // op `fail_at` hit a merge the last commit launched: surfaces next commit
                        let next = store.commit((vec![], vec![]), commit_point(acked + 1));
                        next.expect_err("a failed background merge surfaces at the next commit")
                    }
                };
                let tip = commit_point(acked + 2);
                let retried =
                    catch_unwind(AssertUnwindSafe(|| store.commit(commits[0].clone(), tip)));
                let message =
                    panic_message(retried.expect_err("commit after a failed one refused"));
                assert!(message.contains("after a failed one"), "op {fail_at}: {message}");
                error
            }
        };
        failures += 1;
        assert!(error.to_string().contains("injected EIO"), "op {fail_at}: {error}");

        let fs = fs.restarted();
        let mut store = LsmStore::<TestIndex<2>>::open(fs.clone(), root, NET)
            .unwrap_or_else(|error| panic!("op {fail_at}: reopen: {error}"));
        let recovered = store.committed().count() as usize;
        assert!(
            recovered == acked || recovered == acked + 1,
            "op {fail_at}: recovered {recovered}"
        );
        let mut rows: Vec<Row> =
            commits[..recovered].iter().flat_map(|(rows, _)| rows.clone()).collect();
        rows.sort_by_key(|row| row.key());
        let (scanned, _) = store.sets();
        assert_eq!(
            scanned.pin().range::<Row>(&EVERY_OWNER.0, &EVERY_OWNER.1),
            rows,
            "op {fail_at}"
        );
        store
            .commit((vec![row(9, 99)], vec![]), commit_point(recovered + 1))
            .expect("commit after restart");
    }
    assert!(failures > 40, "only {failures} failure points exercised");
}

/// Duplicate key in a batch, a key in two segments (read + merge), a non-advancing tip (RocksDB:
/// a check never seen firing = not known to work)
#[test]
fn invariant_checks_fire_on_the_bugs_they_guard() {
    let fires = |expected: &str, bug: &dyn Fn()| {
        let message = panic_message(catch_unwind(AssertUnwindSafe(bug)).expect_err(expected));
        assert!(message.contains(expected), "expected {expected:?}, got {message:?}");
    };
    let open =
        || LsmStore::<TestIndex<2>>::open(SimFs::new(), Path::new("/idx"), NET).expect("open");

    fires("strictly ascending", &|| {
        let _ = open().commit((vec![row(1, 0), row(1, 0)], vec![]), commit_point(1));
    });

    fires("a key listed in two committed segments", &|| {
        let mut store = open();
        for n in 1..=2 {
            store.commit((vec![row(1, 0)], vec![]), commit_point(n)).expect("commit");
        }
        store.sets().0.pin().range::<Row>(&EVERY_OWNER.0, &EVERY_OWNER.1);
    });

    // two 1-row segments at fanout 2 = a merge; its duplicate panics on its thread, resumed here
    fires("strictly ascending", &|| {
        let mut store = open();
        for n in 1..=2 {
            store.commit((vec![row(1, 0)], vec![]), commit_point(n)).expect("commit");
        }
        store.logs().0.settle();
        let _ = store.commit((vec![], vec![]), commit_point(3));
    });

    fires("LSM commit to height 0, not above the committed Some(Height(0))", &|| {
        let mut store = open();
        store.commit((vec![row(1, 0)], vec![]), commit_point(1)).expect("commit");
        let _ = store.commit((vec![row(1, 1)], vec![]), commit_point(1));
    });
}

/// Open removes unlisted segments (and their checksums) and refuses a lost one; a flipped committed
/// byte passes open (lengths only) and dies on the first read or merge that touches its page
#[test]
fn open_checks_lengths_and_a_corrupt_page_dies_on_first_touch() {
    let fs = SimFs::new();
    let dir = Path::new("/segments");
    fs.create_dir_all(dir).expect("dir");
    let writer = SegmentWriter::open(fs.clone(), dir);
    let kept =
        writer.write(0, vec![row(1, 0), row(1, 1), row(2, 0)]).expect("write").expect("rows");
    let orphan = writer.write(1, vec![row(3, 0)]).expect("write").expect("rows");
    writer.sync_dir().expect("sync");

    SegmentSet::<OwnerAt>::open::<Row>(fs.clone(), dir, &[kept]).expect("open");
    let path = dir.join(file_name(kept.id));
    let mut kept_files = vec![file_name(kept.id), format!("{}.crc", file_name(kept.id))];
    kept_files.sort();
    assert_eq!(fs.list(dir).expect("list"), kept_files, "unlisted segment {} removed", orphan.id);

    let original = fs.contents(&path).expect("segment bytes");
    fs.corrupt(&path, |bytes| bytes.truncate(10));
    let short = SegmentSet::<OwnerAt>::open::<Row>(fs.clone(), dir, &[kept]).map(|_| ());
    assert!(matches!(short, Err(SegmentError::Page(PageError::Lost { have: 10, .. }))));

    fs.corrupt(&path, |bytes| {
        *bytes = original.clone();
        bytes[Row::STRIDE + 9] ^= 1;
    });
    let set = SegmentSet::<OwnerAt>::open::<Row>(fs.clone(), dir, &[kept]).expect("lengths intact");
    let pinned = set.pin();
    let died = |touch: &dyn Fn()| {
        let message =
            panic_message(catch_unwind(AssertUnwindSafe(touch)).expect_err("never serves"));
        let named =
            message.contains("page 0: checksum mismatch") && message.contains("zainod verify");
        assert!(named, "{message}");
    };
    died(&|| {
        pinned.get::<Row>(&row(1, 1).at);
    });

    // same tier as `kept` at fanout 2 → opening both launches their merge, which reads the page
    let second = writer.write(2, vec![row(4, 0), row(4, 1)]).expect("write").expect("rows");
    let set = SegmentSet::<OwnerAt>::open::<Row>(fs.clone(), dir, &[kept, second]).expect("open");
    let log = SegmentLog::<Row>::open(set, 2).expect("merge launched");
    log.settle();
    let log = std::sync::Mutex::new(log);
    died(&|| {
        let _ = log.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).batch(Vec::new());
    });

    fs.remove(&sums_path(&path)).expect("remove checksums");
    let unsummed = SegmentSet::<OwnerAt>::open::<Row>(fs.clone(), dir, &[kept]).map(|_| ());
    assert!(matches!(unsummed, Err(SegmentError::Page(PageError::Lost { .. }))));

    // probed set: last byte = a filter fingerprint, a page past the filter table (read at open);
    // first probe of its shard dies, even for a key the segment never held
    let ids = Path::new("/ids");
    fs.create_dir_all(ids).expect("dir");
    let id_writer = SegmentWriter::open(fs.clone(), ids);
    let probed =
        id_writer.write(0, (0..3_000).map(id_row).collect()).expect("write").expect("rows");
    let id_path = ids.join(file_name(probed.id));
    fs.corrupt(&id_path, |bytes| *bytes.last_mut().expect("non-empty") ^= 1);
    let set =
        SegmentSet::<Id>::open::<IdRow>(fs.clone(), ids, &[probed]).expect("table page intact");
    let pinned = set.pin();
    let message = panic_message(
        catch_unwind(AssertUnwindSafe(|| pinned.get::<IdRow>(&id_row(9_999).id)))
            .expect_err("a corrupt filter shard never answers"),
    );
    let page = (probed.sealed.len - 1) / 4096;
    assert!(message.contains(&format!("page {page}: checksum mismatch")), "{message}");
}
