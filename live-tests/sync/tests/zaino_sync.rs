//! End-to-end **index-construction test for zaino**: zaino builds its chain
//! index from scratch over a pinned, pre-synced *mainnet* snapshot, and the run
//! asserts the whole way up that the build is well-formed — with the zebrad
//! serving the same snapshot as the independent authority for where the chain
//! ends.
//!
//! **Zaino is the subject, not the driver.** There is no wallet here. The
//! `sync` harness observes zaino's own ingest tick by tick, so the thing under
//! assertion is the *index itself*: how far it has been written, whether it was
//! written in order, and whether it ever claims more chain than exists.
//!
//! ## What this profile does and does not claim
//!
//! It claims the *shape* of the build and its throughput: the frontier only
//! rises, the per-pool counters only accumulate, the frontier never passes the
//! pinned tip, the build keeps making ground, and it ends with its own index
//! serving the pinned tip out of the non-finalised state, the finalised half
//! trailing by no more than one reorg bound. Those are the properties that only
//! exist during construction, which is the interval an end-state comparison
//! throws away.
//!
//! **"Reached the tip" is a two-part claim here, deliberately.** Zaino never
//! finalises the tip: the sync worker writes the finalised index up to
//! `tip - MAX_NONFINALISED_DEPTH` and holds the last 1,001 blocks in the
//! non-finalised state, where a reorg can still rewrite them. A single equality
//! against the pin is therefore unsatisfiable by construction, whichever half it
//! is asked of — so the terminal probes ask the non-finalised state for the tip
//! and the finalised state for the bound.
//!
//! It does **not** compare index *contents* against zebra. That check lived
//! here as a periodic prefix sweep and was removed: several hundred rounds of
//! live RPC against both pods taxes the very throughput this profile measures,
//! and content correctness is an end-state claim that belongs in a parity test
//! over the same fixture. There is no such test on this fixture today, so treat
//! "the index holds the right blocks" as unasserted here.
//!
//! ## Why mainnet, and why this rung
//!
//! Mainnet, because zaino's indexer is sensitive to *transaction density* and
//! testnet has almost none. The per-op counters this profile reads are
//! `sapling_outputs` and `orchard_actions`; a throughput number drawn from a
//! sparse testnet chain describes a corpus nobody runs against.
//!
//! The Ironwood rung — mainnet 3,434,143, NU6.3 activation (3,428,143) plus
//! 6,000 blocks — because it is the deepest artifact either network ships, and
//! the only mainnet one *above* the sandblast onset.
//!
//! **That inverts the reason the Orchard rung was chosen, deliberately.** In
//! June 2022 the sandblasting spam attack begins, and within two days mainnet's
//! median block goes from 2.5 kB to over 100 kB and never comes back down. The
//! onset is derived rather than cited — ECC's retrospective says only "June
//! 2022" — and measures out as the first 500-block window above 3x baseline at
//! **1,704,323** (2022-06-15 18:18 UTC), permanent from **1,706,605**. The
//! Orchard rung stopped ~11,000 blocks below it to hold the pre-spam regime
//! fixed; this one spans both, because the regime an operator runs against today
//! is the post-sandblast one. Throughput measured here and on the Orchard rung
//! are not comparable numbers, and a run of one is not a regression signal for
//! the other.
//!
//! **What it costs.** 3,434,143 blocks and 257.85 GiB extracted — roughly twice
//! the Orchard rung's height and eight times its bytes. The seed PVC is sized
//! from the artifact's own manifest; zaino's index is not, and is declared below.
//!
//! **Two volumes, sized two different ways.** The seed PVC holds the *extracted
//! chain archive* and nothing else — zebra's state DB, pulled once per cluster and
//! CoW-cloned per pod — and `seed_size_for` requests it from the artifact's own
//! manifest (257.85 GiB here, plus headroom). Zaino's index is not seeded and
//! never was: it is built from empty, and under the sync tier it must declare its
//! own PVC rather than take unbounded node ephemeral storage. That declaration is
//! the `.disk(..)` on the indexer below.
//!
//! Every probe here reads the pools and heights off the artifact's manifest
//! rather than hardcoding them, so moving this profile to a deeper mainnet rung
//! is a one-line change to the handle and nothing else.
//!
//! Its zebrad is configured with no peers (`initial_mainnet_peers = []`), so the
//! chain is frozen at the pinned tip for the whole run: the target never moves,
//! no reorg can occur, and every backwards motion in zaino's frontier is
//! therefore a bug rather than a legal rollback.
//!
//! Launched detached via `ztest sync start zaino_index_construction`.

use ztest::prelude::*;
use ztest::snapshots::IRONWOOD_MAINNET;
use ztest::sync::{
    hours, mins, secs, Op, OpSet, Severity, Snapshot, SyncCtx, SyncOutcome, SyncRunner, Verdict,
    Violation,
};

/// How often the engine captures a snapshot. A full-history index build is
/// measured in hours, so a 5 s base tick would spend the run scraping; 15 s
/// still resolves the frontier far finer than any probe cadence below reads it.
const TICK: std::time::Duration = secs(15);

/// The run cap.
///
/// Set on the runner explicitly: `#[ztest::sync_test(timeout = ..)]` records the
/// declared cap in the inventory (where `ztest sync list` and QoS admission read
/// it) but does not reach `SyncEngine`, so a profile that does not set it here
/// has no in-process deadline at all.
const RUN_CAP: std::time::Duration = hours(48);

/// How long the index may go without its frontier advancing before the run is
/// called stalled.
///
/// Generous because a single commit is a large batch of dense mainnet blocks,
/// and because the first minutes are spent opening a 22.5 GB state directory
/// rather than indexing anything.
const STALL_WINDOW: std::time::Duration = mins(15);

/// The blocks zaino deliberately leaves *out* of its finalised index.
///
/// The sync worker never asks the finalised writer for the tip: it writes to
/// `finalized_height_floor(tip)` = `tip - MAX_NONFINALISED_DEPTH`, and the
/// remaining span lives in the non-finalised state, where a reorg can still
/// rewrite it. `MAX_NONFINALISED_DEPTH` is zebra's `MAX_BLOCK_REORG_HEIGHT`
/// (1,000) plus one.
///
/// Mirrored rather than imported: the live-test workspace links no zaino
/// production code (see the workspace note in the repo's `CLAUDE.md`), so the
/// number is restated here with its derivation. It is a consensus-level bound,
/// not a tuning knob — a build that changes it changes what "synced" means, and
/// this profile should fail until the constant is revisited alongside it.
const NON_FINALISED_DEPTH: u32 = 1_001;

/// How long the terminal probes wait for zaino to finish coming up.
///
/// Completion is declared the moment the finalised writer commits its last
/// batch, but zaino is not serving from its own index at that instant: the
/// worker still has to rebuild the txout-set accumulator over the whole
/// finalised range and then anchor and fill the non-finalised state. Both are
/// unbounded-by-design post-sync work, so the terminal probes wait rather than
/// read once and fail the run for asking too early. Not a measured figure — a
/// bound generous enough that expiry means wedged, not slow.
const INDEX_UP_WINDOW: std::time::Duration = mins(30);

/// Poll cadence inside [`INDEX_UP_WINDOW`]. `gettxoutsetinfo` folds the entire
/// non-finalised state on every call, so this is deliberately slack: the answer
/// is a step change, not a curve worth resolving.
const INDEX_UP_POLL: std::time::Duration = secs(10);

#[ztest::needs(IRONWOOD_MAINNET)]
#[ztest::sync_test(
    name = "zaino_index_construction",
    description = "zaino builds its chain index over the pinned NU6.3 mainnet snapshot; zebrad is the authority",
    subject = indexer,
    timeout = "48h",
    qos = sync,
    footprint = "15c/29Gi",
    tags = ["mainnet", "zaino", "index", "ironwood", "nu6.3"],
)]
async fn zaino_index_construction(mut run: SyncRunner) -> SyncOutcome {
    // Topology: one zebrad serving the snapshot, and one zaino building a state
    // index over its own CoW clone of the same artifact. `?` is unavailable —
    // the body returns `SyncOutcome`, not `Result` — so a setup failure converts
    // to an errored outcome via `From` and returns.
    let (zebra, zaino_state) = match run
        .topology(|t| {
            // Explicit per-pod sizing rather than the profile's even split, because
            // the two pods are nothing like the same shape here. Zebra serves a
            // frozen snapshot over RPC — it reads, it does not sync, and it does
            // not grow. Zaino holds an open LMDB write transaction whose dirty
            // pages are the run's real memory cost, so the headroom belongs to
            // it. An even split starves the side that needs it and wastes it
            // on the side that does not.
            //
            // These two must sum inside the declared `footprint` above; the deploy
            // budget checks exactly that before creating a pod.
            // No `.disk(..)`: zebra boots from the archive clone and this chain is
            // frozen, so the clone never grows past the seed floor the manifest sized.
            // A to-tip profile would need one; this one reads what it was given.
            let zebra = t.add_validator(
                Validator::zebrad("6.2.3")
                    .snapshot(IRONWOOD_MAINNET)
                    .resources(Cpu::cores(5), Mem::gib(4)),
            );
            // Zaino is the SUT: built from this repo's Dockerfile with metrics,
            // because this profile's progress source is its own exporter —
            // `no_tls_with_prometheus` is load-bearing here, not decoration. It
            // also carries the no-TLS the cluster needs; the default JSON-RPC
            // public-bind feature is restated because overriding the feature
            // list drops it. Profiling needs no feature: ztest's eBPF collector
            // samples the pod from outside it.
            let zaino_state = t.add_indexer(
                dev!(
                    Indexer::Zainod,
                    "../../Dockerfile",
                    context = "../..",
                    features = [
                        "no_tls_with_prometheus",
                        "allow_unencrypted_public_json_rpc_bind"
                    ]
                )
                // The same chain the validator runs, as a private CoW clone this
                // pod *reads*. Zaino's own index is pod-local scratch and starts
                // empty — building it is what this profile watches.
                .snapshot(IRONWOOD_MAINNET)
                // The whole subject of the test: `Fetch` forwards to the
                // validator and builds no index, so there would be nothing to
                // observe.
                .tuning(ZainoTuning::State)
                // Zaino's index is pod-local scratch, and under the sync tier that must be
                // a reserved PVC rather than node ephemeral storage — a 48-hour build
                // evicted under DiskPressure at hour 39 costs the whole run. Unmeasured:
                // this is headroom over 3.4M mainnet blocks, and the first number to
                // revisit once a run reports `zaino_db_used_bytes` at the pin.
                .disk(Disk::gib(325))
                // The SUT gets the bulk of the declared 29 GiB reserve: 24 GiB,
                // leaving the validator its 4 and a GiB of slack. Not a measured
                // requirement — it is headroom while the write transaction's
                // dirty-page growth is still unbounded, and the number to revisit
                // first once a run gets far enough to measure the real ceiling.
                .resources(Cpu::cores(9), Mem::gib(24)),
            );
            (zebra, zaino_state)
        })
        .await
    {
        Ok(handles) => handles,
        Err(e) => return e.into(),
    };

    // The pinned facts about this chain: read from the artifact's own manifest
    // at compile time and cross-checked against the running validator during
    // `topology`. That provenance is what makes them usable as an oracle — a
    // height zaino reports and a height zebra reports can be wrong together,
    // and the manifest was written by the producer before either pod existed.
    let chain = run.chain();

    run.sync(zaino_state);
    run.tick(TICK).timeout(RUN_CAP);
    // The same op list `indexed_work_monotonic` reads with `Work::require`,
    // declared up front so the engine checks it against one live reading before
    // the run starts. Without this the mismatch that matters here — zaino not
    // publishing a counter ztest asks for, which is a cross-repo agreement on a
    // Prometheus series name that nothing compiles — surfaces as a `require`
    // panic on some later tick, naming the op but never the series to go find.
    run.requires_work(OpSet::of(&INDEXED_POOLS));
    // Deliberately no `until_height`: a declared stop height is what makes two
    // runs' throughput comparable, and it is the wrong trade here. The reason
    // this fixture was chosen is the 6,000 blocks above the NU6.3 activation, and
    // any stop height low enough to bound the run is also low enough to end it
    // before that boundary — buying comparability by never reaching the thing
    // under test. The chain is frozen, so the span is already fixed by the pin.

    // ── safety: what the index must never do while it is being built ──
    run.always(Severity::Fatal)
        .named("index_append_only")
        .every(secs(30))
        .check(index_append_only);
    run.always(Severity::Recorded)
        .named("indexed_work_monotonic")
        .each_tick()
        .check(indexed_work_monotonic);
    {
        // Captured clones rather than `cx`: `SyncCtx` carries only the indexer,
        // so the validator — the one oracle in this topology that is not the
        // subject — reaches a probe by being moved into it.
        let zebra = zebra.clone();
        run.always(Severity::Fatal)
            .named("index_within_pinned_tip")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let zebra = zebra.clone();
                Box::pin(async move { index_within_pinned_tip(s, &zebra, chain).await })
            });
    }
    // ── liveness: the build must keep making ground ──
    run.eventually(Severity::Fatal)
        .named("index_advances")
        .window(STALL_WINDOW)
        .check(index_advances);

    // ── coverage: the checks above are only worth their green if the run
    //    actually watched an index being built ──
    run.sometimes()
        .named("observed_a_partial_index")
        .check(observed_a_partial_index);
    run.sometimes()
        .named("crossed_the_straddled_activation")
        .check(crossed_activation);

    // ── terminal: end-state agreement, against the pin rather than a component ──
    run.at_completion(Severity::Fatal)
        .named("index_serves_the_pinned_tip")
        .check_rpc(move |s, cx| Box::pin(index_serves_the_pinned_tip(s, cx, chain)));
    run.at_completion(Severity::Fatal)
        .named("finalised_seam_within_reorg_bound")
        .check(move |s: &Snapshot| finalised_seam_within_reorg_bound(s, chain));

    run.run().await
}

// ── safety invariants ────────────────────────────────────────────────────

/// The finalised index is append-only: its frontier never moves backwards.
///
/// zaino's own glossary states this outright — the finalised state is
/// "append-only: never incrementally rolled back" — and on this topology there
/// is no escape hatch for it. The snapshot's zebrad runs with no peers, so the
/// chain cannot reorg; *any* backwards motion in the frontier is a bug in the
/// index, not a rollback it was asked to perform. That is why this carries no
/// reorg-depth tolerance, unlike the same invariant written against a live
/// chain.
fn index_append_only(s: &Snapshot) -> Verdict {
    ztest::sync_ensure!(
        s.height() >= s.prev_height(),
        "finalised index frontier went backwards {} -> {} on a frozen chain, where no \
         reorg can have asked it to",
        s.prev_height(),
        s.height()
    );
    Verdict::Satisfied
}

/// The per-pool work the index has absorbed only ever accumulates.
///
/// These are zaino's own cumulative counters, incremented as each block is
/// written. A decrease means the indexer either double-counted and corrected, or
/// re-entered a range it had already absorbed — neither of which an append-only
/// build does. `require` rather than `get`: an op nobody measured must panic
/// here, because comparing two absent values would make this probe unfailable.
///
/// The op list is [`INDEXED_POOLS`]; `run.requires_work` has already proved the
/// subject publishes every one of them, so `require` cannot surprise this probe.
fn indexed_work_monotonic(s: &Snapshot) -> Verdict {
    for op in INDEXED_POOLS {
        let (prev, now) = (s.prev_work().require(op), s.work().require(op));
        ztest::sync_ensure!(
            now >= prev,
            "indexed {op:?} count fell {prev} -> {now}; an append-only index only accumulates"
        );
    }
    Verdict::Satisfied
}

/// The op classes the chain this profile rides actually contains.
///
/// Written out rather than derived. The chain is named at the declaration now —
/// `IRONWOOD_MAINNET` spans every activation mainnet has had, so Sapling outputs
/// and Orchard actions are both dense across it — which makes this a property of
/// *this* profile,
/// not something to rediscover from a manifest at runtime.
///
/// **What makes writing it out safe.** Naming a pool the chain never reaches used
/// to have two failure modes and no good one: the counter is absent and every
/// tick panics on `require`, or it is published as a flat zero and `0 >= 0`
/// passes forever while proving nothing. `run.requires_work` closes the first
/// before the run starts — it reads the subject once and fails with the
/// Prometheus series it could not find — and the second cannot happen, because
/// an op zaino does not publish is *unmeasured*, never zero.
///
/// Repointing this profile at a different rung means editing this list. That is
/// the intended cost: a fixture and the pools it carries are one decision.
const INDEXED_POOLS: [Op; 2] = [Op::SaplingOutput, Op::OrchardAction];

/// Mainnet NU6.3, the upgrade `IRONWOOD_MAINNET` straddles — 6,000 blocks below its pin.
///
/// Stated here for the same reason as [`INDEXED_POOLS`]: it is a consensus constant
/// about the chain this profile rides, and ztest carries no table of those. A wrong
/// value weakens [`crossed_activation`] rather than breaking it — the coverage probe
/// would latch early — so it is checked by review, next to the prose that derives it.
const STRADDLED_ACTIVATION: u32 = 3_428_143;

/// The frontier never claims more chain than exists.
///
/// Checked against **two** independent statements of where the chain ends: the
/// validator's live height, and the height the artifact's manifest pins. The
/// manifest is the stronger of the two and the reason both are here — it was
/// written by the producer before either pod started, so it cannot be dragged
/// along by whatever zaino and zebra might agree to be wrong about.
async fn index_within_pinned_tip(
    s: &Snapshot,
    validator: &ZebraValidator,
    chain: ChainSnapshot,
) -> Verdict {
    let pinned = chain.tip_height;
    ztest::sync_ensure!(
        s.height() <= pinned,
        "index frontier {} is above the snapshot's pinned tip {pinned}; the chain cannot \
         have grown, so the frontier is describing blocks that do not exist",
        s.height()
    );
    let live = match validator.chain_height().await {
        Ok(h) => u32::from(h),
        Err(e) => return Verdict::ProbeError(format!("validator chain_height: {e}")),
    };
    ztest::sync_ensure!(
        live == pinned,
        "the validator reports height {live} but the snapshot pins {pinned}: this chain is \
         not frozen, and every invariant in this profile that assumes it is has been \
         measuring something else"
    );
    ztest::sync_ensure!(
        s.height() <= live,
        "index frontier {} is ahead of the validator it indexes ({live})",
        s.height()
    );
    Verdict::Satisfied
}

// ── liveness ─────────────────────────────────────────────────────────────

/// The index frontier advanced within the stall window.
fn index_advances(s: &Snapshot) -> Verdict {
    if s.progressed_within(STALL_WINDOW) {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── coverage ─────────────────────────────────────────────────────────────

/// At least one tick caught the index *mid-build*.
///
/// The anti-vacuity latch, and the reason this profile can be trusted at all.
/// Zaino's state backend proxies its validator until its own index is serving,
/// so a subject reading any proxied height would open at the tip, satisfy the
/// completion predicate on tick one, and pass having observed nothing. If this
/// probe never latches, that is exactly what happened — the run attached to an
/// index that was already built, and every safety probe above it ran against a
/// frontier that never moved.
fn observed_a_partial_index(s: &Snapshot) -> Verdict {
    match s.target() {
        Some(target) if s.height() < target => Verdict::Satisfied,
        _ => Verdict::Pending,
    }
}

/// The build crossed the activation this fixture straddles.
///
/// A run that ended below the activation never indexed a single block under the
/// new rules, so every claim it made was about history that predates them. That
/// is a weak pass, and coverage is how the harness says so.
///
/// On this fixture the claim is weaker than it was on the Orchard rung, and the
/// difference is worth stating. NU5 funded a pool inside its 6,000-block tail, so
/// crossing it proved Orchard actions were indexed. NU6.3's tail carries whatever
/// Ironwood adoption existed in those blocks, which nothing here measures — so
/// crossing [`STRADDLED_ACTIVATION`] proves the build indexed blocks written under
/// the new rules, and not that those blocks contain Ironwood actions.
/// [`INDEXED_POOLS`] omits `IronwoodAction` for the same reason.
fn crossed_activation(s: &Snapshot) -> Verdict {
    if s.height() >= STRADDLED_ACTIVATION {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── terminal ─────────────────────────────────────────────────────────────

/// The index comes up, and when it does it reaches the tip the artifact pins.
///
/// **Why this is one probe and not two.** "Zaino is serving from its own index"
/// and "that index covers the pinned tip" are two claims, but a single call
/// states both and neither is observable without the other. `gettxoutsetinfo`
/// is that call:
///
/// - While the finalised state is still catching up, zaino answers the empty
///   object zcashd returns when stats collection fails — the accumulator's
///   spent-index invariants do not hold yet, and it says so rather than
///   guessing. An answer *with* a height is therefore proof the index is up.
/// - The height it answers with is `non_finalized_snapshot.best_tip`: the
///   non-finalised tip, folded on top of the finalised accumulator. There is no
///   proxy path through it, so unlike every height zaino *forwards*, this one
///   cannot be the validator's answer wearing zaino's name.
///
/// So the tip claim is the non-finalised state's, which is the only place the
/// last [`NON_FINALISED_DEPTH`] blocks exist — see
/// [`finalised_seam_within_reorg_bound`] for the other half of the end state.
///
/// Waits rather than reads once: completion fires when the last finalised batch
/// commits, which is two pieces of post-sync work before the index serves. RPC
/// errors inside the window are retried and only surfaced if the window
/// expires — the pod is rebuilding an accumulator over the whole chain while
/// this polls, and a refused connection there is expected, not a verdict.
async fn index_serves_the_pinned_tip(s: &Snapshot, cx: &SyncCtx, chain: ChainSnapshot) -> Verdict {
    let Some(ix) = cx.indexer() else {
        return Verdict::ProbeError("index_serves_the_pinned_tip: no indexer bound".into());
    };
    let rpc = match ix.json_rpc().await {
        Ok(rpc) => rpc,
        Err(e) => return Verdict::ProbeError(format!("zaino json_rpc: {e}")),
    };
    let pinned = chain.tip_height;
    let deadline = std::time::Instant::now() + INDEX_UP_WINDOW;
    loop {
        // Why this attempt did not satisfy, so an expiry names the state it
        // expired in rather than only the height it wanted.
        let last = match rpc
            .call_value("gettxoutsetinfo", serde_json::json!([]))
            .await
        {
            // Empty object = still syncing; `height` present = the index is
            // serving. Distinguished by the field rather than by shape name
            // because the two arms serialize untagged.
            Ok(v) => match v.get("height").and_then(serde_json::Value::as_u64) {
                Some(h) if h == u64::from(pinned) => return Verdict::Satisfied,
                Some(h) => format!(
                    "the index is serving but its non-finalised tip is {h}, not the pinned \
                     {pinned}"
                ),
                None => "zaino still answers `gettxoutsetinfo` empty, so its finalised state is \
                         not caught up and the index is not serving"
                    .to_string(),
            },
            Err(e) => format!("`gettxoutsetinfo` did not answer: {e}"),
        };
        if std::time::Instant::now() >= deadline {
            return violated(
                s.height(),
                format!(
                    "{INDEX_UP_WINDOW:?} after the finalised writer committed its last batch at \
                     frontier {}, {last}",
                    s.height()
                ),
            );
        }
        tokio::time::sleep(INDEX_UP_POLL).await;
    }
}

/// The finalised index stops short of the tip, by no more than one reorg bound.
///
/// The other half of the end state, and the reason this is not an equality
/// against the pin. Zaino's finalised writer is *aimed* at
/// `tip - MAX_NONFINALISED_DEPTH` and never at the tip: everything above that
/// floor can still be reorged away, so it is held in the non-finalised state
/// instead. A probe demanding `frontier == pinned` therefore asserts against a
/// height the design forbids, and fails every run by exactly
/// [`NON_FINALISED_DEPTH`] — which is what it did before this was rewritten.
///
/// What is worth asserting is the bound: the span zaino left unfinalised is no
/// deeper than the reorg limit that justifies leaving it. Deeper means the
/// finalised index is lagging its own target, which on this frozen chain is a
/// writer that stopped rather than a chain that moved.
fn finalised_seam_within_reorg_bound(s: &Snapshot, chain: ChainSnapshot) -> Verdict {
    let pinned = chain.tip_height;
    let frontier = s.height();
    // Above the pin is already fatal per tick (`index_within_pinned_tip`); repeated
    // here because the subtraction below would otherwise wrap into a passing lag.
    ztest::sync_ensure!(
        frontier <= pinned,
        "finalised frontier {frontier} is above the snapshot's pinned tip {pinned}"
    );
    let lag = pinned - frontier;
    ztest::sync_ensure!(
        lag <= NON_FINALISED_DEPTH,
        "finalised frontier finished at {frontier}, {lag} blocks below the pinned tip {pinned}: \
         zaino finalises up to `tip - {NON_FINALISED_DEPTH}`, so anything deeper is the writer \
         falling short of its own target on a chain that cannot have moved"
    );
    Verdict::Satisfied
}

// ── helpers ──────────────────────────────────────────────────────────────

/// A named terminal/streamed violation.
fn violated(height: u32, detail: String) -> Verdict {
    Verdict::Violated(Violation {
        probe: String::new(),
        height: Some(height),
        detail,
    })
}
