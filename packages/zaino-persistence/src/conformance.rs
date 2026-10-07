//! What every [`PersistenceEngine`] must answer, through the port alone (feature `testing`)
//!
//! - an engine's crate implements [`Subject`] once, then runs [`history`] under proptest and
//!   [`contract`] as a plain test; `PROPTEST_CASES=1000` = its heavy run
//! - both drive the engine through [`Tiered`] (apply, stage, finalize, reorg) as every index does
//! - storage-specific moves (power loss, background work, internal invariants) = [`Subject`]
//!   hooks with no-op defaults
//! - [`Model`] doubles as the expected state for an engine's own crash and fault tests

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
};

use bytes::Bytes;
use proptest::{prelude::*, strategy::Union};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zcash_protocol::consensus::NetworkType;

use crate::{
    manifest::IndexKind,
    port::{
        Changes, MapId, MapRead, PersistenceEngine, Schema, SequenceId, SequenceRead, Store, Width,
    },
    tiered::Tiered,
};

pub const BLOCKS: SequenceId = SequenceId(0);
pub const HEIGHTS: SequenceId = SequenceId(1);
pub const NODES: SequenceId = SequenceId(2);
pub const SCANNED: MapId = MapId(0);
pub const PROBED: MapId = MapId(1);

/// Source bytes per staged commit (a few small blocks; any `large` one alone)
const BATCH: NonZeroUsize = NonZeroUsize::new(24).expect("non-zero");

/// Every shape an index declares: a variable sequence, a fixed one, one in a sub-directory, a
/// scoped map (`account ‖ seq`, read per account), a point-lookup map (hash-like ids)
pub fn schema() -> Schema {
    Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest)
        .with_sequence(BLOCKS, "blocks", Width::Variable)
        .with_sequence(HEIGHTS, "heights", Width::fixed(8))
        .with_sequence(NODES, "pool/nodes", Width::fixed(4))
        .with_map(SCANNED, "scanned", Width::fixed(12), Width::fixed(8), 8)
        .with_map(PROBED, "probed", Width::fixed(16), Width::fixed(4), 0)
}

/// One engine under test, over storage the suite can reopen (and, if it can, crash)
pub trait Subject {
    type Engine: PersistenceEngine<Store: Store<View: SequenceRead + MapRead>>;

    /// An engine over the storage as it stands now (a reopen = a fresh `open` through it)
    fn engine(&self) -> Self::Engine;

    fn path(&self) -> &Path;

    /// Storage replaced by what a crash right now would leave; `false` = no such image (the
    /// step reopens instead)
    fn power_loss(&mut self) -> bool {
        false
    }

    /// Background work finished (merges); no-op where there is none
    fn settle(&self, _store: &StoreOf<Self>) {}

    /// Engine-internal invariants after every step (`just_opened` = a fresh open)
    fn check(&self, _store: &StoreOf<Self>, _just_opened: bool, _label: &str) {}
}

pub type StoreOf<S> = <<S as Subject>::Engine as PersistenceEngine>::Store;

fn open<S: Subject>(subject: &S) -> StoreOf<S> {
    subject.engine().open(subject.path(), &schema()).expect("open")
}

/// Record `n` of `blocks`: 0 to 22 bytes (empty records included)
pub fn block(n: u32) -> Vec<u8> {
    vec![n as u8; n as usize % 23]
}

/// Uniform 8 bytes per account (engines may shard on leading key bytes)
fn account(n: u8) -> [u8; 8] {
    u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes()
}

pub fn scanned_key(owner: u8, seq: u32) -> Vec<u8> {
    [&account(owner)[..], &seq.to_be_bytes()].concat()
}

/// Uniform first 8 bytes, `n` in the last 4 (distinct per `n`)
pub fn probed_key(n: u32) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[..8].copy_from_slice(&u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id[12..].copy_from_slice(&n.to_be_bytes());
    id
}

/// Commit `n` (from 1) covers heights 0 to `n - 1`, tipped by `[n; 32]`
pub fn commit_point(n: usize) -> BlockRef {
    salted_point(n, 0)
}

/// [`commit_point`] on branch `salt` (0 = the first branch)
fn salted_point(n: usize, salt: u8) -> BlockRef {
    let height = Height::try_from(n as u32 - 1).expect("small height");
    let mut hash = [n as u8; 32];
    hash[0] ^= salt;
    BlockRef { hash: BlockHash::from(hash), height }
}

/// `bytes` on branch `salt` (a dropped record ≠ its replacement)
fn salted(mut bytes: Vec<u8>, salt: u8) -> Vec<u8> {
    if let Some(first) = bytes.first_mut() {
        *first ^= salt;
    }
    bytes
}

/// Panic payload text (`panic!` with arguments → `String`, a literal → `&str`)
pub fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => {
            payload.downcast::<&str>().map(|message| message.to_string()).unwrap_or_default()
        }
    }
}

/// Every table's contents, as acknowledged (a commit returns once durable)
///
/// - `salt` = branch of the next commit's contents (0 outside [`history`])
#[derive(Debug, Clone, Default)]
pub struct Model {
    commits: usize,
    tip: Option<BlockRef>,
    salt: u8,
    blocks: Vec<Vec<u8>>,
    heights: Vec<Vec<u8>>,
    nodes: Vec<Vec<u8>>,
    scanned: BTreeMap<Vec<u8>, Vec<u8>>,
    probed: BTreeMap<Vec<u8>, Vec<u8>>,
    seq: u32,
    ids: u32,
}

impl Model {
    /// The next commit (one height): `records` blocks + twice as many nodes, one height record,
    /// one `scanned` row per listed owner (fresh seqs), `ids` fresh `probed` ids; applied here,
    /// returned as `Changes`
    pub fn commit(&mut self, records: u8, owners: &[u8], ids: u16) -> Changes {
        self.commits += 1;
        let tip = salted_point(self.commits, self.salt);
        self.tip = Some(tip);
        let mut changes = Changes::new(tip, &schema());
        for _ in 0..records {
            let record = salted(block(self.blocks.len() as u32), self.salt);
            changes.append(BLOCKS, &record);
            self.blocks.push(record);
            for _ in 0..2 {
                let node = salted((self.nodes.len() as u32).to_le_bytes().to_vec(), self.salt);
                changes.append(NODES, &node);
                self.nodes.push(node);
            }
        }
        let height = salted((self.commits as u64).to_be_bytes().to_vec(), self.salt);
        changes.append(HEIGHTS, &height);
        self.heights.push(height);
        for &owner in owners {
            self.seq += 1;
            let key = scanned_key(owner, self.seq);
            let value = salted(u64::from(self.seq).to_be_bytes().to_vec(), self.salt);
            changes.insert(SCANNED, &key, &value);
            self.scanned.insert(key, value);
        }
        for n in self.ids..self.ids + u32::from(ids) {
            let value = salted(n.to_be_bytes().to_vec(), self.salt);
            changes.insert(PROBED, &probed_key(n), &value);
            self.probed.insert(probed_key(n), value);
        }
        self.ids += u32::from(ids);
        changes
    }

    /// `start` inclusive to `end` exclusive, as `(key, value)`
    fn scan(&self, start: &[u8], end: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let rows =
            self.scanned.iter().filter(|(key, _)| start <= key.as_slice() && key.as_slice() < end);
        rows.map(|(key, value)| (key.clone(), value.clone())).collect()
    }

    /// `owner` + `raw` seq reduced into 0 to `seq + 1` (lands on, between and past issued rows)
    fn bound(&self, (owner, raw): (u8, u32)) -> Vec<u8> {
        scanned_key(owner, raw % (self.seq + 2))
    }

    /// Every sequence record (one at a time and as ranges), full and per-owner scans, sampled
    /// values + a miss, values over every id issued and as many never issued (each asked twice:
    /// answers in caller order)
    pub fn assert_view(&self, view: &(impl SequenceRead + MapRead), label: &str) {
        assert_eq!(view.tip(), self.tip, "{label}: tip");
        for (table, model) in
            [(BLOCKS, &self.blocks), (HEIGHTS, &self.heights), (NODES, &self.nodes)]
        {
            let len = model.len() as u64;
            assert_eq!(view.len(table), len, "{label}: {table:?} len");
            let one_by_one: Vec<_> = (0..len).map(|at| view.record(table, at)).collect();
            let expected: Vec<_> =
                model.iter().map(|record| Some(Bytes::from(record.clone()))).collect();
            assert_eq!(one_by_one, expected, "{label}: {table:?} records");
            assert_eq!(view.record(table, len), None, "{label}: {table:?} past the end");
            assert_eq!(view.records(table, 0..len), *model, "{label}: {table:?} whole range");
            let middle = len / 3..len - len / 3;
            let expected = &model[middle.start as usize..middle.end as usize];
            assert_eq!(view.records(table, middle), expected, "{label}: {table:?} middle range");
        }

        let every = self.scan(&[0; 12], &[0xff; 12]);
        let scanned = view.range(SCANNED, &[0; 12], &[0xff; 12], usize::MAX).expect("unbounded");
        assert_eq!(owned(scanned), every, "{label}: full scan");
        for owner in [0u8, 3, 9] {
            let (from, to) = (scanned_key(owner, 0), scanned_key(owner, u32::MAX));
            let answer = view.range(SCANNED, &from, &to, usize::MAX).expect("unbounded");
            assert_eq!(owned(answer), self.scan(&from, &to), "{label}: owner {owner}");
        }
        for (key, value) in self.scanned.iter().step_by(7) {
            assert_eq!(view.value(SCANNED, key).as_deref(), Some(&value[..]), "{label}: {key:?}");
        }
        assert_eq!(view.value(SCANNED, &scanned_key(1, self.seq + 1)), None, "{label}: miss");

        let asked: Vec<Vec<u8>> =
            (0..2 * self.ids + 2).flat_map(|n| [probed_key(n), probed_key(n)]).collect();
        let answers: Vec<Option<Bytes>> =
            asked.iter().map(|id| self.probed.get(id).cloned().map(Bytes::from)).collect();
        let keys: Vec<&[u8]> = asked.iter().map(Vec::as_slice).collect();
        assert_eq!(view.values(PROBED, &keys), answers, "{label}: values");
        let singles: Vec<_> = keys.iter().map(|id| view.value(PROBED, id)).collect();
        assert_eq!(singles, answers, "{label}: value");
    }
}

fn owned(rows: Vec<(Bytes, Bytes)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    rows.into_iter().map(|(key, value)| (key.to_vec(), value.to_vec())).collect()
}

/// Oracle for [`Tiered`]: `durable` = finalized commits; `held` = the contents after each held
/// block, oldest first (a view = the newest over durable)
///
/// - `staged` = source bytes when the held blocks are final; `salt` = the branch blocks go on
#[derive(Debug, Clone, Default)]
struct Tiers {
    durable: Model,
    held: Vec<Model>,
    staged: Option<usize>,
    salt: u8,
}

impl Tiers {
    fn tip(&self) -> &Model {
        self.held.last().unwrap_or(&self.durable)
    }

    /// Next block on the current branch, held
    fn hold(&mut self, (records, owners, ids): (u8, &[u8], u16)) -> Changes {
        let mut next = self.tip().clone();
        next.salt = self.salt;
        let changes = next.commit(records, owners, ids);
        self.held.push(next);
        changes
    }

    /// Height of the `count`th held block (from 1)
    fn height(&self, count: usize) -> Height {
        self.held[count - 1].tip.expect("a held block has a tip").height
    }

    fn finalize(&mut self, count: usize) {
        self.durable = self.held[count - 1].clone();
        self.held.drain(..count);
        if self.held.is_empty() {
            self.staged = None;
        }
    }

    /// Reorg, restart and power loss alike: uncommitted gone, later blocks on a new branch
    fn drop_held(&mut self) {
        self.held.clear();
        self.staged = None;
        self.salt = self.salt.wrapping_add(1);
    }

    /// Both tiers + the tips a writer publishes by = the model's
    fn assert<S: Store<View: SequenceRead + MapRead>>(&self, tiered: &Tiered<S>, label: &str) {
        let view = tiered.view();
        self.tip().assert_view(&view, label);
        self.durable.assert_view(view.durable(), &format!("{label}: durable"));
        let staged = self.staged.and(self.tip().tip);
        let applied = if staged.is_some() { self.durable.tip } else { self.tip().tip };
        let tips = (tiered.durable_tip(), tiered.applied(), tiered.staged());
        assert_eq!(tips, (self.durable.tip, applied, staged), "{label}: durable, applied, staged");
    }
}

/// - `Apply`: a tip block (`records` blocks + nodes, one height, `owners` scanned rows, `ids`
///   probed ids); staged blocks finalized first (a writer's move)
/// - `Stage`: a final block (applied blocks finalized first: a producer's window empties
///   before a final block arrives); a whole batch finalized
/// - `Finalize`: through held block `at` (reduced at run time; staged = all of them)
/// - `Reorg`: applied blocks dropped (never with staged: no producer sends it)
/// - `Settle`: background work finishes; `Reopen`: exit mid-work; `PowerLoss`: crash now
/// - `Pin`: the view + model kept, re-checked every later step
/// - `Scan`: raw bounds reduced at run time, every limit around the answer's size
#[derive(Debug, Clone)]
pub enum Step {
    Apply { records: u8, owners: Vec<u8>, ids: u16 },
    Stage { records: u8, owners: Vec<u8>, ids: u16 },
    Finalize { at: u8 },
    Reorg,
    Settle,
    Reopen,
    PowerLoss,
    Pin,
    Scan { from: (u8, u32), to: (u8, u32), limit: u16 },
}

/// Swarm testing (TigerBeetle `tree_fuzz`): whole step kinds off per case (a uniform mix dilutes
/// rare interleavings: all reopens, no settles, only small batches, bulk alone, …)
pub fn steps() -> impl Strategy<Value = Vec<Step>> {
    let on = prop::array::uniform8(prop::bool::ANY);
    on.prop_flat_map(|[settle, reopen, power_loss, pin, scan, large, apply, reorg]| {
        // small blocks; with `large`, sometimes past a 4 KiB block (205 scanned / 204 probed)
        let small = prop::collection::vec(0u8..4, 0..8);
        let (owners, ids) = match large {
            true => (
                prop_oneof![4 => small, 1 => prop::collection::vec(0u8..4, 250..600)].boxed(),
                prop_oneof![4 => 0u16..8, 1 => 200u16..600].boxed(),
            ),
            false => (small.boxed(), (0u16..8).boxed()),
        };
        let content = (0u8..6, owners, ids);
        let stage = content
            .clone()
            .prop_map(|(records, owners, ids)| Step::Stage { records, owners, ids })
            .boxed();
        let tip = content.prop_map(|(records, owners, ids)| Step::Apply { records, owners, ids });
        let bound = (0u8..6, any::<u32>());
        let query = (bound.clone(), bound, any::<u16>()).prop_map(|(from, to, limit)| Step::Scan {
            from,
            to,
            limit,
        });
        let finalize = any::<u8>().prop_map(|at| Step::Finalize { at }).boxed();
        let mut kinds = vec![(4, stage), (2, finalize)];
        for (enabled, weight, kind) in [
            (apply, 4, tip.boxed()),
            (reorg, 1, Just(Step::Reorg).boxed()),
            (settle, 1, Just(Step::Settle).boxed()),
            (reopen, 1, Just(Step::Reopen).boxed()),
            (power_loss, 1, Just(Step::PowerLoss).boxed()),
            (pin, 1, Just(Step::Pin).boxed()),
            (scan, 2, query.boxed()),
        ] {
            if enabled {
                kinds.push((weight, kind));
            }
        }
        prop::collection::vec(Union::new_weighted(kinds), 1..40)
    })
}

/// Source bytes a block stands for (one per item: few small blocks fill a batch)
fn weight(records: u8, owners: &[u8], ids: u16) -> usize {
    1 + usize::from(records) + owners.len() + usize::from(ids)
}

/// One history against the subject and the [`Tiers`] oracle, after every step:
///
/// - view = held over durable; durable view = finalized commits alone; pinned view = its model
/// - crash or reopen = exactly the finalized commits; [`Subject::check`] + [`Tiered::check`] hold
pub fn history<S: Subject>(mut subject: S, steps: &[Step]) {
    let mut tiered = Tiered::new(open(&subject), BATCH);
    let mut model = Tiers::default();
    let mut pinned = None;

    for (at, step) in steps.iter().enumerate() {
        let label = format!("step {at} {step:?}");
        let mut reopened = false;
        match step {
            Step::Apply { records, owners, ids } => {
                if let Some(staged) = tiered.staged() {
                    tiered.finalize(staged.height);
                    model.finalize(model.held.len());
                }
                tiered.apply(model.hold((*records, owners, *ids)));
            }
            Step::Stage { records, owners, ids } => {
                if model.staged.is_none() && !model.held.is_empty() {
                    tiered.finalize(model.height(model.held.len()));
                    model.finalize(model.held.len());
                }
                let weight = weight(*records, owners, *ids);
                let full = tiered.stage(model.hold((*records, owners, *ids)), weight);
                let staged = model.staged.unwrap_or(0) + weight;
                model.staged = Some(staged);
                assert_eq!(full, staged >= BATCH.get(), "{label}: a whole batch staged");
                if full {
                    tiered.finalize(model.height(model.held.len()));
                    model.finalize(model.held.len());
                }
            }
            Step::Finalize { at } if !model.held.is_empty() => {
                let count = match model.staged {
                    Some(_) => model.held.len(),
                    None => usize::from(*at) % model.held.len() + 1,
                };
                tiered.finalize(model.height(count));
                model.finalize(count);
            }
            Step::Finalize { .. } => {}
            Step::Reorg if model.staged.is_none() => {
                tiered.reorg();
                model.drop_held();
            }
            Step::Reorg => {}
            Step::Settle => subject.settle(tiered.store()),
            // a crash image taken while background work may still be writing: acked = durable
            Step::Reopen | Step::PowerLoss => {
                if matches!(step, Step::PowerLoss) {
                    subject.power_loss();
                }
                drop(tiered);
                tiered = Tiered::new(open(&subject), BATCH);
                model.drop_held();
                reopened = true;
            }
            Step::Pin => pinned = Some((at, model.clone(), tiered.view())),
            Step::Scan { from, to, limit } => {
                let tip = model.tip();
                let (from, to) = (tip.bound(*from), tip.bound(*to));
                let expected = tip.scan(&from, &to);
                // over the limit = `None`, never a truncated answer; both sides of the edge
                let len = expected.len();
                for limit in [len.saturating_sub(1), len, len + 1, usize::from(*limit) % (len + 2)]
                {
                    let answer = tiered.view().range(SCANNED, &from, &to, limit).map(owned);
                    let bounded = (len <= limit).then(|| expected.clone());
                    assert_eq!(answer, bounded, "{label}: {from:?}..{to:?} at most {limit}");
                }
            }
        }
        subject.check(tiered.store(), reopened, &label);
        tiered.check(&label);
        model.assert(&tiered, &label);
        if let Some((at, model, view)) = &pinned {
            let label = format!("{label}: view pinned at step {at}");
            model.tip().assert_view(view, &label);
            model.durable.assert_view(view.durable(), &label);
        }
    }
}

/// Panic expected from `act` on `tiered` (any wording: each names its own invariant)
fn refused<T>(what: &str, tiered: &mut T, act: impl FnOnce(&mut T)) {
    let acted = catch_unwind(AssertUnwindSafe(|| act(tiered)));
    assert!(acted.is_err(), "{what}: done instead of panicking");
}

/// Port's fixed promises, on a fresh subject: an empty open, identity refused across kind,
/// format and network, a reopen resuming at the tip, each misuse panicking (store and tiers),
/// and offline verify clean with every commit counted
pub fn contract<S: Subject>(subject: S) {
    let engine = subject.engine();
    let mut model = Model::default();
    let mut store = open(&subject);
    model.assert_view(&store.view(), "fresh");
    assert_eq!(store.schema(), &schema(), "the store keeps its schema");
    assert_eq!(store.path(), subject.path(), "the store keeps its path");
    store.commit(model.commit(2, &[1, 2], 2)).expect("commit");
    drop(store);

    for (what, schema) in [
        ("network", Schema { network: NetworkType::Main, ..schema() }),
        ("kind", Schema { kind: IndexKind::TreeState, ..schema() }),
        ("format", Schema { format: 2, ..schema() }),
    ] {
        let refused = engine.open(subject.path(), &schema).map(|_| ());
        assert!(refused.is_err(), "another {what} opened as this store");
    }

    let mut store = open(&subject);
    model.assert_view(&store.view(), "reopened");
    let verified = engine.verify(subject.path(), &schema()).expect("verify");
    assert!(verified.is_clean() && verified.heights == 1, "{verified:?}");

    let stale = Changes::new(commit_point(1), &schema());
    refused("a tip that does not advance", &mut store, |store| drop(store.commit(stale)));
    drop(store);
    let mut store = open(&subject);
    let other = Schema::new(IndexKind::CompactBlock, 1, NetworkType::Regtest);
    let foreign = Changes::new(commit_point(2), &other);
    refused("changes for another schema", &mut store, |store| drop(store.commit(foreign)));
    drop(store);

    // tiers' preconditions: each a panic before any state moves, tiers usable after
    let mut tiered = Tiered::new(open(&subject), BATCH);
    let mut tiers = Tiers { durable: model, ..Tiers::default() };
    let durable = tiers.durable.tip.map(|tip| tip.height).expect("one commit");
    let next = |tiers: &Tiers| tiers.clone().hold((1, &[3], 1));
    let gap = {
        let mut ahead = tiers.clone();
        ahead.hold((1, &[], 0));
        ahead.hold((1, &[], 0))
    };
    refused("a gap", &mut tiered, |tiered| tiered.apply(gap));
    let foreign = Changes::new(commit_point(3), &other);
    refused("tiers fed another schema", &mut tiered, |tiered| tiered.apply(foreign));
    refused("finalize at durable", &mut tiered, |tiered| tiered.finalize(durable));
    tiered.apply(tiers.hold((1, &[4], 1)));
    let changes = next(&tiers);
    refused("final above applied", &mut tiered, |tiered| _ = tiered.stage(changes, 1));
    let above = tiers.height(1).next();
    refused("finalize above held", &mut tiered, |tiered| tiered.finalize(above));
    tiered.reorg();
    tiers.drop_held();
    for _ in 0..2 {
        assert!(!tiered.stage(tiers.hold((1, &[5], 1)), 1), "two bytes: under the batch");
    }
    tiers.staged = Some(2);
    let changes = next(&tiers);
    refused("apply over staged", &mut tiered, |tiered| tiered.apply(changes));
    refused("reorg with staged", &mut tiered, |tiered| tiered.reorg());
    let first = tiers.height(1);
    refused("finalize splitting staged", &mut tiered, |tiered| tiered.finalize(first));
    tiers.assert(&tiered, "after misuse");
    tiered.finalize(tiers.height(2));
    tiers.finalize(2);
    tiers.assert(&tiered, "staged finalized after misuse");
    tiered.apply(tiers.hold((2, &[6], 2)));
    tiers.assert(&tiered, "applies continue after misuse");
}
