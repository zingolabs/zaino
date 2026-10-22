//! `DiskEngine` on `SimFs` against [`Model`]: random histories of applies, commits, reorgs above
//! durable, merges, reopens, power loss and pinned views

use std::{
    cell::{Cell, RefCell},
    fmt,
    panic::{self, AssertUnwindSafe},
    sync::Once,
};

use proptest::{prelude::*, strategy::Union};

use super::model::{assert_tiers, open, scanned_key, sim_fs, Key, Model, Writes, SCHEMA};
use super::*;
use crate::overlay::Overlay;

/// Block above durable, as the NFS holds it: its changes, contents through it, its layer
struct Node {
    changes: BlockChanges,
    model: Model,
    layer: Overlay,
}

/// Oracle for [`history`]
///
/// - `nodes` = blocks above `durable`, oldest first, the first `applied` buffered in the store
/// - `branch` = branch of the next node
#[derive(Default)]
struct Oracle {
    durable: Model,
    nodes: Vec<Node>,
    applied: usize,
    branch: u8,
}

impl Oracle {
    /// Contents through the last applied node (what `applied()` reads)
    fn buffered(&self) -> &Model {
        self.applied.checked_sub(1).map_or(&self.durable, |at| &self.nodes[at].model)
    }

    /// Contents through the newest node (a pinned view's)
    fn newest(&self) -> &Model {
        self.nodes.last().map_or(&self.durable, |node| &node.model)
    }

    /// Newest node's layer over `durable` (`durable` = the store's durable view)
    fn newest_view(&self, durable: DiskView) -> OverlayView<DiskView> {
        let layer = self.nodes.last().map_or_else(|| Overlay::empty(&SCHEMA), |n| n.layer.clone());
        OverlayView::new(durable, layer)
    }

    fn unapplied(&self) -> usize {
        self.nodes.len() - self.applied
    }

    /// Node on the current branch above the newest: parent's layer `.with` its changes
    fn grow(&mut self, writes: &Writes) {
        let (mut model, layer) = match self.nodes.last() {
            Some(node) => (node.model.clone(), node.layer.clone()),
            None => (self.durable.clone(), Overlay::empty(&SCHEMA)),
        };
        model.branch = self.branch;
        let changes = model.advance(writes);
        let layer = layer.with(&changes);
        self.nodes.push(Node { changes, model, layer });
    }

    /// Every node rebased onto `durable` (commit or reopen: what it now holds dropped)
    fn rebase(&mut self, durable: &DiskView) {
        for node in &mut self.nodes {
            node.layer = node.layer.rebase(durable);
        }
    }

    /// - store's durable view = durable, applied = through the last applied node
    /// - node layers over the view = their contents (oldest + newest read in full)
    /// - buffered bytes >= applied item bytes, 0 iff nothing applied
    fn assert(&self, store: &DiskStore, label: &str) {
        self.durable.assert_view(&store.durable(), &format!("{label}: durable"));
        self.buffered().assert_view(&store.applied(), &format!("{label}: applied"));
        let buffered = store.buffered_bytes();
        let items = self.buffered().bytes - self.durable.bytes;
        assert!(
            buffered >= items,
            "{label}: buffered bytes {buffered}, want >= applied item bytes {items}"
        );
        assert_eq!(
            buffered == 0,
            self.applied == 0,
            "{label}: buffered bytes {buffered} with {} nodes applied (0 iff none)",
            self.applied
        );
        for node in &self.nodes {
            node.layer.check(label);
        }
        for node in [self.nodes.first(), self.nodes.last()].into_iter().flatten() {
            let view = OverlayView::new(store.durable(), node.layer.clone());
            node.model.assert_view(&view, &format!("{label}: node {:?}", node.model.tip));
        }
    }
}

/// Views taken at step `at` + the models they must keep answering (applied = a borrow: never
/// pinned)
struct Pinned {
    at: usize,
    durable: Model,
    newest: Model,
    view: DiskView,
    node: OverlayView<DiskView>,
}

/// - `Grow`: node above the newest (`Writes::removed` may name rows durable, buffered or in a node)
/// - `Apply`: oldest unapplied nodes into the store, at most `up_to`
/// - `Commit`: every buffered node durable, then every node rebased onto the new view
/// - `Reorg`: unapplied nodes cut to at most `keep`, later nodes on a new branch
/// - `Settle`: merges finish
/// - `Reopen`: exit mid-merge, `PowerLoss`: crash now (both: buffer lost, nodes kept + re-applied)
/// - `Pin`: views + models kept, re-checked every later step
/// - `Scan`: `(owner, seq)` bounds (seq past the last issued = just past it), every limit around
///   the answer's size + `limit`
#[derive(Clone)]
enum Step {
    Grow(Writes),
    Apply { up_to: u8 },
    Commit,
    Reorg { keep: u8 },
    Settle,
    Reopen,
    PowerLoss,
    Pin,
    Scan { from: (u8, u32), to: (u8, u32), limit: u16 },
}

/// One line per step
impl fmt::Debug for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Grow(writes) => write!(f, "Grow({writes:?})"),
            Step::Apply { up_to } => write!(f, "Apply(up to {up_to})"),
            Step::Commit => write!(f, "Commit"),
            Step::Reorg { keep } => write!(f, "Reorg(keep {keep})"),
            Step::Settle => write!(f, "Settle"),
            Step::Reopen => write!(f, "Reopen"),
            Step::PowerLoss => write!(f, "PowerLoss"),
            Step::Pin => write!(f, "Pin"),
            Step::Scan { from, to, limit } => write!(
                f,
                "Scan(owner {} seq {} .. owner {} seq {}, limit {limit})",
                from.0, from.1, to.0, to.1
            ),
        }
    }
}

/// Swarm testing (TigerBeetle `tree_fuzz`): whole step kinds off per case (a uniform mix dilutes
/// rare interleavings: all reopens, no settles, only small blocks, no reorgs, …)
fn steps() -> impl Strategy<Value = Vec<Step>> {
    let on = prop::array::uniform8(prop::bool::ANY);
    on.prop_flat_map(|[settle, reopen, power_loss, pin, scan, large, reorg, removing]| {
        // - small blocks, with `large` sometimes past a 4 KiB block (205 scanned / 204 probed)
        // - `issued` ≈ seqs a case issues (removals + scan bounds drawn up to it)
        let small = prop::collection::vec(0u8..4, 0..8);
        let (owners, ids, issued) = match large {
            true => (
                prop_oneof![4 => small, 1 => prop::collection::vec(0u8..4, 250..600)].boxed(),
                prop_oneof![4 => 0u16..8, 1 => 200u16..600].boxed(),
                2_000u32,
            ),
            false => (small.boxed(), (0u16..8).boxed(), 128),
        };
        // `removing`: a few seqs per block, sometimes most of those issued (whole segments cancel
        // in merges)
        let seq = 1..=issued;
        let removed = match removing {
            true => prop_oneof![
                4 => prop::collection::vec(seq.clone(), 0..8),
                1 => prop::collection::vec(seq, 100..600),
            ]
            .boxed(),
            false => Just(Vec::new()).boxed(),
        };
        let grow = (0u8..6, owners, ids, removed)
            .prop_map(|(records, owners, ids, removed)| {
                Step::Grow(Writes { records, owners, ids, removed })
            })
            .boxed();
        let bound = (0u8..6, 0..=issued + 1);
        let query = (bound.clone(), bound, 0u16..64).prop_map(|(from, to, limit)| Step::Scan {
            from,
            to,
            limit,
        });
        let mut kinds = vec![
            (4, grow),
            (3, (0u8..8).prop_map(|up_to| Step::Apply { up_to }).boxed()),
            (2, Just(Step::Commit).boxed()),
        ];
        for (enabled, weight, kind) in [
            (reorg, 1, (0u8..4).prop_map(|keep| Step::Reorg { keep }).boxed()),
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

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
    static PANICKED: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Panic hook silent on this thread while held, keeping the last panic's location + message:
/// proptest reports the minimal case's panic once, not every shrink step's
struct QuietPanics;

impl QuietPanics {
    fn hold() -> Self {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let previous = panic::take_hook();
            panic::set_hook(Box::new(move |info| match QUIET.get() {
                true => PANICKED.set(info.to_string()),
                false => previous(info),
            }));
        });
        QUIET.set(true);
        Self
    }
}

impl Drop for QuietPanics {
    fn drop(&mut self) {
        QUIET.set(false);
    }
}

/// [`run`] with any panic re-raised as `panicked at <location>: <message>` (proptest's report
/// keeps only the message)
fn history(fanout: usize, flush_device: bool, steps: &[Step]) {
    let _quiet = QuietPanics::hold();
    let ran = panic::catch_unwind(AssertUnwindSafe(|| run(fanout, flush_device, steps)));
    if ran.is_err() {
        panic!("{}", PANICKED.take());
    }
}

/// History against `Oracle`, after every step:
///
/// - `durable()` = durable prefix, `applied()` = durable + buffered, buffered bytes >= applied
/// - each node's layer over the view = its contents, pinned views = their models
/// - crash or reopen = exactly the durable nodes
/// - [`assert_tiers`] + `Overlay::check` + `WriteBuffer::check` hold
fn run(fanout: usize, flush_device: bool, steps: &[Step]) {
    let mut fs = sim_fs(flush_device);
    let mut store = open(&DiskEngine::with_fanout(fs.clone(), fanout));
    let mut oracle = Oracle::default();
    let mut pinned: Option<Pinned> = None;

    for (at, step) in steps.iter().enumerate() {
        let mut label = format!("step {at} {step:?}");
        let mut reopened = false;
        match step {
            Step::Grow(writes) => oracle.grow(writes),
            Step::Apply { up_to } => {
                let count = usize::from(*up_to).min(oracle.unapplied());
                label = format!("{label}: applied {count} of {} unapplied", oracle.unapplied());
                for node in &oracle.nodes[oracle.applied..oracle.applied + count] {
                    store.apply(node.changes.clone());
                }
                oracle.applied += count;
            }
            Step::Commit => {
                store.commit().unwrap_or_else(|error| panic!("{label}: {error}"));
                oracle.durable = oracle.buffered().clone();
                oracle.nodes.drain(..oracle.applied);
                oracle.applied = 0;
                oracle.rebase(&store.durable());
            }
            Step::Reorg { keep } => {
                let kept = usize::from(*keep).min(oracle.unapplied());
                label = format!("{label}: kept {kept} of {} unapplied", oracle.unapplied());
                oracle.nodes.truncate(oracle.applied + kept);
                oracle.branch = oracle.branch.wrapping_add(1);
            }
            Step::Settle => store.settle(),
            // crash image taken while merges may still be writing: acked = durable
            Step::Reopen | Step::PowerLoss => {
                if matches!(step, Step::PowerLoss) {
                    fs = fs.power_loss();
                }
                drop(store);
                store = open(&DiskEngine::with_fanout(fs.clone(), fanout));
                oracle.applied = 0;
                oracle.rebase(&store.durable());
                reopened = true;
            }
            Step::Pin => {
                pinned = Some(Pinned {
                    at,
                    durable: oracle.durable.clone(),
                    newest: oracle.newest().clone(),
                    view: store.durable(),
                    node: oracle.newest_view(store.durable()),
                })
            }
            Step::Scan { from, to, limit } => {
                let newest = oracle.newest();
                let bound =
                    |(owner, seq): (u8, u32)| Key(scanned_key(owner, seq.min(newest.seq + 1)));
                let (from, to) = (bound(*from), bound(*to));
                let applied = format!("{label}: applied");
                oracle.buffered().assert_scans(&store.applied(), (&from, &to), *limit, &applied);
                let node = oracle.newest_view(store.durable());
                newest.assert_scans(&node, (&from, &to), *limit, &format!("{label}: newest node"));
            }
        }
        assert_tiers(&store, &fs, fanout, reopened, &label);
        store.buffer.check(&label);
        oracle.assert(&store, &label);
        if let Some(pin) = &pinned {
            let label = format!("{label}: pinned at step {}", pin.at);
            pin.durable.assert_view(&pin.view, &format!("{label}: durable"));
            pin.newest.assert_view(&pin.node, &format!("{label}: newest node"));
        }
    }
}

proptest! {
    // - 64 cases ≈ seconds (heavy run after any change: docs/testing.md "Heavy run")
    // - shrinking past the default 4 × cases (a 100-seq removal shrinks to the seqs that matter),
    //   for at most 20 s (inside the default profile's 60 s kill)
    #![proptest_config(ProptestConfig {
        cases: 64,
        max_shrink_iters: 4_096,
        max_shrink_time: 20_000,
        ..ProptestConfig::default()
    })]

    /// [`history`] at fanouts merging every few commits and rarely, per-file fsync and one-flush
    /// (`Barrier`) commits
    #[test]
    fn random_histories_answer_like_a_btreemap_through_merges_reopens_and_power_loss(
        fanout in prop_oneof![Just(2usize), Just(3), Just(8)],
        flush_device in any::<bool>(),
        steps in steps(),
    ) {
        history(fanout, flush_device, &steps);
    }
}
