//! Zaino builds its chain index from empty over a pinned mainnet snapshot; the zebrad serving
//! that same snapshot = independent authority for where the chain ends.
//!
//! - Zaino = subject, not driver (no wallet; harness watches its ingest tick by tick)
//! - Asserts build *shape*: frontier monotonic, per-pool counters accumulate, frontier <= pin,
//!   ground kept, index serving the pin at the end — all properties an end-state check discards
//! - Index *contents* vs zebra NOT compared (periodic prefix sweep removed — hundreds of live RPC
//!   rounds taxed the throughput being measured) → "holds the right blocks" unasserted here
//! - "Reached the tip" = two claims, zaino never finalising the tip: non-finalised state asked for
//!   the tip, finalised state for the bound ([`NON_FINALISED_DEPTH`])
//!
//! Fixture = `IRONWOOD_MAINNET`, mainnet 3,434,143 (NU6.3 activation 3,428,143 + 6,000):
//!
//! - Mainnet not testnet (indexer sensitive to tx density, testnet has near none)
//! - Deepest artifact either network ships, only mainnet one above the sandblast onset
//! - Onset derived, not cited (ECC's retrospective says only "June 2022") = first 500-block
//!   window above 3x baseline @ 1,704,323, permanent from 1,706,605
//! - Orchard rung stops ~11,000 below it (holds the pre-spam regime); this spans both → the two
//!   throughputs are not comparable, neither a regression signal for the other
//! - 257.85 GiB extracted, ~2x the Orchard rung's height and ~8x its bytes
//!
//! Two volumes, sized two ways:
//!
//! - Seed PVC = extracted chain archive alone (zebra state DB, pulled once, CoW-cloned per pod),
//!   sized by `seed_size_for` off the artifact's manifest
//! - Zaino's index never seeded → built from empty, declares its own PVC (`.disk(..)` below);
//!   sync tier refuses unbounded node ephemeral storage
//!
//! - Probes read pools + heights off the manifest → repointing at a deeper rung = one line
//! - zebrad peerless (`initial_mainnet_peers = []`) → chain frozen, so backwards frontier motion
//!   = bug, never a legal rollback
//!
//! Launched detached: `ztest sync start zaino_index_construction`

use ztest::prelude::*;
use ztest::snapshots::IRONWOOD_MAINNET;
use ztest::sync::{
    hours, mins, secs, Op, OpSet, Severity, Snapshot, SyncCtx, SyncOutcome, SyncRunner, Verdict,
    Violation,
};

/// Engine snapshot cadence. Full-history build runs for hours (a 5 s tick spends the run
/// scraping); 15 s still resolves the frontier finer than any probe below reads it
const TICK: std::time::Duration = secs(15);

/// Run cap, set on the runner — `sync_test(timeout = ..)` records the inventory's declared cap
/// but never reaches `SyncEngine`, so a profile omitting this has no in-process deadline
const RUN_CAP: std::time::Duration = hours(48);

/// Frontier may sit this long before the run is called stalled.
///
/// - One commit = a large batch of dense mainnet blocks
/// - First minutes open a 22.5 GB state dir rather than index anything
const STALL_WINDOW: std::time::Duration = mins(15);

/// Blocks held out of the finalised index = zebra's `MAX_BLOCK_REORG_HEIGHT` (1,000) + 1.
///
/// - Writer aims at `tip - MAX_NONFINALISED_DEPTH`; rest sits in non-finalised state (reorgable)
/// - Mirrored, not imported (live-test workspace links no zaino production code)
/// - Consensus bound, not a knob → a build changing it changes what "synced" means
const NON_FINALISED_DEPTH: u32 = 1_001;

/// Terminal probes' wait for zaino to serve from its own index.
///
/// - Completion fires when the finalised writer commits its last batch, not when serving starts
/// - Two unbounded post-sync steps first: rebuild txout-set accumulator, then anchor + fill
///   the non-finalised state
/// - Unmeasured — expiry means wedged, not slow
const INDEX_UP_WINDOW: std::time::Duration = mins(30);

/// Poll cadence inside [`INDEX_UP_WINDOW`]. Slack on purpose: `gettxoutsetinfo` folds the whole
/// non-finalised state per call, and the answer is a step change
const INDEX_UP_POLL: std::time::Duration = secs(10);

#[ztest::needs(IRONWOOD_MAINNET)]
#[ztest::sync_test(
    name = "zaino_index_construction",
    description = "index built from empty over a pinned NU6.3 mainnet snapshot; zebrad = authority",
    subject = indexer,
    timeout = "48h",
    qos = sync,
    footprint = "6c/24Gi",
    tags = ["mainnet", "zaino", "index", "ironwood", "nu6.3"],
)]
async fn zaino_index_construction(mut run: SyncRunner) -> SyncOutcome {
    // Topology: one zebrad serving the snapshot, and one zaino building a state
    // index over its own CoW clone of the same artifact. `?` is unavailable —
    // the body returns `SyncOutcome`, not `Result` — so a setup failure converts
    // to an errored outcome via `From` and returns.
    let (zebra, zaino_state) = match run
        .topology(|t| {
            // Lopsided on purpose: zebra reads a frozen snapshot and never grows, zaino holds an
            // open LMDB write txn whose dirty pages are the run's real memory cost.
            // - Pods must sum inside the declared `footprint` (DeployBudget checks pre-create)
            // - No `.disk(..)` on zebra: frozen chain → the clone never passes the seed floor
            let zebra = t.add_validator(
                Validator::zebrad("6.2.3")
                    .snapshot(IRONWOOD_MAINNET)
                    .resources(Cpu::cores(2), Mem::gib(4)),
            );
            // SUT, built from this repo's Dockerfile.
            // - `no_tls_with_prometheus` load-bearing (its exporter = this profile's progress
            //   source) + carries the no-TLS the cluster needs
            // - Public-bind restated: overriding the feature list drops the default
            // - Profiling needs no feature (eBPF collector samples from outside the pod)
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
                // Validator's chain as a private CoW clone this pod reads; zaino's own index
                // starts empty (building it = what this profile watches)
                .snapshot(IRONWOOD_MAINNET)
                // Subject of the test — `Fetch` forwards and builds no index, nothing to observe
                .tuning(ZainoTuning::State)
                // Reserved PVC, not node ephemeral (sync tier; eviction at hour 39 = the run).
                // Unmeasured headroom over 3.4M blocks — revisit off `zaino_db_used_bytes`
                .disk(Disk::gib(325))
                // 20 of the declared 24 GiB (validator takes the rest). Unmeasured headroom
                // while write-txn dirty-page growth is unbounded; first number to revisit
                .resources(Cpu::cores(4), Mem::gib(20)),
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

    // ── safety: what the index must never do mid-build ──
    run.always(Severity::Fatal)
        .named("index_append_only")
        .every(secs(30))
        .check(index_append_only);
    run.always(Severity::Recorded)
        .named("indexed_work_monotonic")
        .each_tick()
        .check(indexed_work_monotonic);
    {
        // Captured clone, not `cx`: `SyncCtx` carries only the indexer, so the validator (the
        // one oracle here that is not the subject) reaches a probe by being moved in
        let zebra = zebra.clone();
        run.always(Severity::Fatal)
            .named("index_within_pinned_tip")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let zebra = zebra.clone();
                Box::pin(async move { index_within_pinned_tip(s, &zebra, chain).await })
            });
    }
    // ── liveness: build keeps making ground ──
    run.eventually(Severity::Fatal)
        .named("index_advances")
        .window(STALL_WINDOW)
        .check(index_advances);

    // ── coverage: above green only counts if the run watched an index being built ──
    run.sometimes()
        .named("observed_a_partial_index")
        .check(observed_a_partial_index);
    run.sometimes()
        .named("crossed_the_straddled_activation")
        .check(crossed_activation);

    // ── terminal: end state vs the pin, not vs a component ──
    run.at_completion(Severity::Fatal)
        .named("index_serves_the_pinned_tip")
        .check_rpc(move |s, cx| Box::pin(index_serves_the_pinned_tip(s, cx, chain)));
    run.at_completion(Severity::Fatal)
        .named("finalised_seam_within_reorg_bound")
        .check(move |s: &Snapshot| finalised_seam_within_reorg_bound(s, chain));

    run.run().await
}

// ── safety invariants ────────────────────────────────────────────────────

/// Finalised frontier never moves backwards (zaino's glossary: "append-only: never
/// incrementally rolled back").
///
/// - No reorg-depth tolerance, unlike the same invariant against a live chain: peerless zebrad
///   → the chain cannot reorg → any backwards motion = index bug, not a rollback it was asked for
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

/// Per-pool work absorbed only accumulates (zaino's own cumulative counters, one bump per block).
///
/// - Decrease = double-count corrected, or a range re-entered — neither an append-only build does
/// - `require`, not `get`: two absent values compare equal and make this probe unfailable
/// - `run.requires_work` already proved [`INDEXED_POOLS`] published → `require` cannot surprise
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

/// Op classes this profile's chain carries — written out, not derived from a manifest.
///
/// - `IRONWOOD_MAINNET` spans every mainnet activation → both dense across it
/// - Safe to write out: a pool the chain never reaches is *unmeasured*, never a flat zero, and
///   `run.requires_work` fails pre-run naming the missing series
/// - Repointing the rung means editing this list (fixture + its pools = one decision)
const INDEXED_POOLS: [Op; 2] = [Op::SaplingOutput, Op::OrchardAction];

/// Mainnet NU6.3, straddled by `IRONWOOD_MAINNET` 6,000 blocks below its pin.
///
/// - Stated here per [`INDEXED_POOLS`] (consensus constant; ztest carries no table of those)
/// - Wrong value weakens [`crossed_activation`] rather than breaking it (latches early) →
///   checked by review, beside the derivation
const STRADDLED_ACTIVATION: u32 = 3_428_143;

/// Frontier never claims more chain than exists.
///
/// - Two independent statements of where the chain ends: the validator's live height, and the
///   manifest's pin
/// - Manifest = the stronger (written by the producer before either pod existed) → cannot be
///   dragged along by whatever zaino and zebra agree to be wrong about
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

/// Frontier advanced inside [`STALL_WINDOW`]
fn index_advances(s: &Snapshot) -> Verdict {
    if s.progressed_within(STALL_WINDOW) {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── coverage ─────────────────────────────────────────────────────────────

/// At least one tick caught the index mid-build. Anti-vacuity latch = why this profile is
/// trustable at all.
///
/// - State backend proxies its validator until its own index serves → a subject reading any
///   proxied height opens at the tip and passes on tick one having observed nothing
/// - Never latching = exactly that, with every safety probe above run against a frozen frontier
fn observed_a_partial_index(s: &Snapshot) -> Verdict {
    match s.target() {
        Some(target) if s.height() < target => Verdict::Satisfied,
        _ => Verdict::Pending,
    }
}

/// Build crossed [`STRADDLED_ACTIVATION`].
///
/// - Ending below it = every claim about history predating the new rules (weak pass, and
///   coverage is how the harness says so)
/// - Weaker claim than the Orchard rung's: NU5 funded a pool inside its tail, NU6.3's carries
///   only whatever Ironwood adoption existed → this proves blocks written under the new rules
///   were indexed, not that they hold Ironwood actions ([`INDEXED_POOLS`] omits it for the same)
fn crossed_activation(s: &Snapshot) -> Verdict {
    if s.height() >= STRADDLED_ACTIVATION {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── terminal ─────────────────────────────────────────────────────────────

/// Index comes up serving the pinned tip. One probe, not two — `gettxoutsetinfo` states both
/// claims and neither is observable without the other.
///
/// - Finalised state still catching up → zaino answers zcashd's empty stats-failed object, so an
///   answer *with* a height proves the index is up
/// - That height = `non_finalized_snapshot.best_tip`, folded on the finalised accumulator; no
///   proxy path through it → unlike every height zaino forwards, not the validator's in disguise
/// - Non-finalised state = the only place the last [`NON_FINALISED_DEPTH`] blocks exist (other
///   half of the end state: [`finalised_seam_within_reorg_bound`])
/// - Waits, never reads once: completion fires two post-sync steps before serving, and a refused
///   connection while the accumulator rebuilds is expected, not a verdict
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
        // Why this attempt missed → expiry names the state it expired in, not just the height
        let last = match rpc
            .call_value("gettxoutsetinfo", serde_json::json!([]))
            .await
        {
            // Empty object = still syncing, `height` present = serving. Keyed on the field, not
            // a shape name (both arms serialize untagged)
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

/// Finalised index stops short of the tip by no more than one reorg bound. Other half of the
/// end state, and why this is not an equality against the pin.
///
/// - Writer aims at `tip - MAX_NONFINALISED_DEPTH`, never the tip (everything above that floor
///   is still reorgable) → `frontier == pinned` asserts a height the design forbids
/// - Bound is what is worth asserting: deeper = writer lagging its own target, which on a frozen
///   chain means it stopped
fn finalised_seam_within_reorg_bound(s: &Snapshot, chain: ChainSnapshot) -> Verdict {
    let pinned = chain.tip_height;
    let frontier = s.height();
    // Already fatal per tick (`index_within_pinned_tip`); repeated because the subtraction
    // below would otherwise wrap into a passing lag

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

/// Named terminal/streamed violation
fn violated(height: u32, detail: String) -> Verdict {
    Verdict::Violated(Violation {
        probe: String::new(),
        height: Some(height),
        detail,
    })
}
