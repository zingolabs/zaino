//! End-to-end **index-construction test for zaino**: zaino builds its chain
//! index from scratch over a pinned, pre-synced testnet snapshot, and the run
//! continuously asserts that what it has built so far is correct — with the
//! zebrad serving the same snapshot as the independent authority.
//!
//! **Zaino is the subject, not the driver.** There is no wallet here. The
//! `sync` harness observes zaino's own ingest tick by tick, so the thing under
//! assertion is the *index itself*: how far it has been written, whether it was
//! written in order, and whether the answers it serves over the range it has
//! covered are the answers zebra gives for that same range.
//!
//! ## Why this is not `clientless/tests/testnet_parity.rs`
//!
//! `state_parity` waits for zaino's index to finish and then compares its
//! answers against zebra's. That is the *end-state* claim, and it is the right
//! one to make there. It is also, by construction, blind to everything this
//! profile exists for: `wait_for_served_index` throws the entire construction
//! interval away, and every bug that lives in that interval with it — an index
//! written out of order, a frontier that goes backwards, an answer served for a
//! range that has not been indexed yet.
//!
//! This profile owns that interval. Anything only checkable once the index is
//! complete belongs in `state_parity` instead, not here.
//!
//! ## Why the deep snapshot
//!
//! [`IRONWOOD`] is the 4,140,000-block artifact: the only fixture that crosses
//! the real Ironwood boundary, carries every prior activation, and puts the
//! finalised/non-finalised seam and the commitment trees under genuine scale.
//! Indexing it is hours of real work over real history — which is the point.
//! Its zebrad is configured with no peers (`initial_testnet_peers = []`), so the
//! chain is frozen at the pinned tip for the whole run: the target never moves,
//! no reorg can occur, and every backwards motion in zaino's frontier is
//! therefore a bug rather than a legal rollback.
//!
//! Launched detached via `ztest sync start zaino_index_construction`.

use ztest::prelude::*;
use ztest::sync::{
    hours, mins, secs, Op, Severity, Snapshot, Subject, SyncCtx, SyncOutcome, SyncRunner, Verdict,
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
/// Generous because a single commit near the tip of real history is a large
/// batch of dense blocks, and because the first minutes are spent opening a
/// ~10 GB state directory rather than indexing anything.
const STALL_WINDOW: std::time::Duration = mins(15);

/// How many blocks of frontier progress between prefix-correctness sweeps.
///
/// The sweep is the most expensive probe in the profile — it is real RPC
/// traffic against both zaino and zebra — so it is paced by chain progress
/// rather than by the clock: on a 4.14M-block build this is a few hundred
/// sweeps spread evenly across the whole of history, rather than a burst
/// wherever the indexer happened to be slow.
const PREFIX_SWEEP_BLOCKS: u32 = 10_000;

#[ztest::needs(IRONWOOD)]
#[ztest::sync_test(
    name = "zaino_index_construction",
    description = "zaino builds its chain index over the pinned Ironwood testnet snapshot; zebrad is the authority",
    subject = indexer,
    timeout = "48h",
    qos = sync,
    tags = ["testnet", "zaino", "index", "ironwood"],
)]
async fn zaino_index_construction(mut run: SyncRunner) -> SyncOutcome {
    // Topology: one zebrad serving the snapshot, and one zaino building a state
    // index over its own CoW clone of the same artifact. `?` is unavailable —
    // the body returns `SyncOutcome`, not `Result` — so a setup failure converts
    // to an errored outcome via `From` and returns.
    let (zeb, zai) = match run
        .topology(|t| {
            let zeb = t.add_validator(Validator::zebrad("6.2.3").restore(IRONWOOD));
            // Zaino is the SUT: built from this repo's Dockerfile with metrics
            // *and* profiling, because this profile's progress source is its own
            // exporter — `no_tls_with_prometheus` is load-bearing here, not
            // decoration. It also carries the no-TLS the cluster needs; the
            // default JSON-RPC public-bind feature is restated because
            // overriding the feature list drops it.
            let zai = t.add_indexer(
                dev!(
                    Indexer::Zainod,
                    "../../Dockerfile",
                    context = "../..",
                    features = [
                        "no_tls_with_prometheus",
                        "allow_unencrypted_public_json_rpc_bind",
                        "profile"
                    ]
                )
                .restore(IRONWOOD)
                // The whole subject of the test: `Fetch` forwards to the
                // validator and builds no index, so there would be nothing to
                // observe.
                .tuning(ZainoTuning::State),
            );
            (zeb, zai)
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

    run.sync(Subject::zaino(&zai));
    run.tick(TICK).timeout(RUN_CAP);
    // Deliberately no `until_height`: a declared stop height is what makes two
    // runs' throughput comparable, and it is the wrong trade here. The reason
    // this fixture was chosen is the history above the Ironwood activation, and
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
        let zeb = zeb.clone();
        run.always(Severity::Fatal)
            .named("index_within_pinned_tip")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let zeb = zeb.clone();
                Box::pin(async move { index_within_pinned_tip(s, &zeb, chain).await })
            });
    }
    {
        let zeb = zeb.clone();
        run.always(Severity::Fatal)
            .named("index_prefix_matches_validator")
            .every_blocks(PREFIX_SWEEP_BLOCKS)
            .check_rpc(move |s, cx| {
                let zeb = zeb.clone();
                Box::pin(async move { index_prefix_matches_validator(s, cx, &zeb, chain).await })
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
        .named("crossed_the_ironwood_activation")
        .check(move |s: &Snapshot| crossed_activation(s, chain));

    // ── terminal: end-state agreement, against the pin rather than a component ──
    run.at_completion(Severity::Fatal)
        .named("reached_the_pinned_tip")
        .check(move |s: &Snapshot| reached_the_pinned_tip(s, chain));
    run.at_completion(Severity::Fatal)
        .named("serves_from_its_own_index")
        .check_rpc(move |s, cx| Box::pin(serves_from_its_own_index(s, cx)));

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
fn indexed_work_monotonic(s: &Snapshot) -> Verdict {
    for op in [Op::SaplingOutput, Op::OrchardAction] {
        let (prev, now) = (s.prev_work().require(op), s.work().require(op));
        ztest::sync_ensure!(
            now >= prev,
            "indexed {op:?} count fell {prev} -> {now}; an append-only index only accumulates"
        );
    }
    Verdict::Satisfied
}

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
    chain: ChainInfo,
) -> Verdict {
    let pinned = chain.tip_height();
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

/// **The correctness spine: the index is right at every intermediate state, not
/// only at the end.**
///
/// Everything above this probe is about the *shape* of the build — the frontier
/// only rises, the counters only accumulate, the chain underneath is frozen.
/// None of them look at a single thing the index actually stored. This one
/// does: over the range zaino has indexed *so far*, its answers must be zebra's
/// answers.
///
/// Called every [`PREFIX_SWEEP_BLOCKS`] of frontier progress, with the snapshot
/// at that moment, the indexer on `cx`, and the validator captured. See
/// [`ChainInfo::boundary_heights`] for the pinned heights worth querying, and
/// `clientless/tests/testnet_parity.rs` for the end-state battery this is the
/// mid-build counterpart to.
///
// TODO(prefix-sweep): implement. The design questions this has to answer are
// laid out in the conversation that prepared this file; the shape below is the
// signature and the wiring, not the policy.
async fn index_prefix_matches_validator(
    s: &Snapshot,
    cx: &SyncCtx,
    validator: &ZebraValidator,
    chain: ChainInfo,
) -> Verdict {
    let _ = (s, cx, validator, chain);
    Verdict::ProbeError(
        "index_prefix_matches_validator is unimplemented: the profile must not report a \
         green run while its correctness probe checks nothing"
            .into(),
    )
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

/// The build crossed the activation this fixture exists for.
///
/// A run that ended below 4,134,000 never indexed a single Ironwood-bearing
/// block, so every pool-sensitive claim it made was about history that predates
/// the pool. That is a weak pass, and coverage is how the harness says so.
fn crossed_activation(s: &Snapshot, chain: ChainInfo) -> Verdict {
    if s.height() >= chain.activation() {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── terminal ─────────────────────────────────────────────────────────────

/// The finished index covers exactly the chain the artifact pins.
///
/// Against the manifest, deliberately, and not against `latest_block_height`:
/// asking zaino where the chain ends and then congratulating it for having
/// reached there is the failure mode this whole profile was rebuilt to avoid.
fn reached_the_pinned_tip(s: &Snapshot, chain: ChainInfo) -> Verdict {
    let pinned = chain.tip_height();
    ztest::sync_ensure!(
        s.height() == pinned,
        "index frontier finished at {} but the snapshot pins its tip at {pinned}",
        s.height()
    );
    Verdict::Satisfied
}

/// At completion, zaino answers from its own index rather than forwarding.
///
/// `getaddressdeltas` is the discriminator: zaino synthesizes it from its index
/// and zebra serves no such method, so while the state backend is still
/// proxying, the call is forwarded and comes back JSON-RPC `-32601`. Any other
/// answer is the index serving. (Same signal as `wait_for_served_index` in
/// `clientless/tests/testnet_parity.rs`, used here as an assertion rather than
/// as a gate.)
async fn serves_from_its_own_index(s: &Snapshot, cx: &SyncCtx) -> Verdict {
    let Some(ix) = cx.indexer() else {
        return Verdict::ProbeError("serves_from_its_own_index: no indexer bound".into());
    };
    let rpc = match ix.json_rpc().await {
        Ok(rpc) => rpc,
        Err(e) => return Verdict::ProbeError(format!("zaino json_rpc: {e}")),
    };
    // The probe is whether the *method* is answered, so the selector is
    // deliberately trivial: an empty result proves the index served just as well
    // as a populated one, and picking a real address would make this depend on
    // discovery it does not need.
    let selector = serde_json::json!([{ "addresses": [], "start": 0, "end": 1 }]);
    match rpc.call_value("getaddressdeltas", selector).await {
        Ok(_) => Verdict::Satisfied,
        Err(e) if e.to_string().contains("-32601") => violated(
            s.height(),
            format!(
                "at frontier {} zaino still forwards `getaddressdeltas` to the validator, so \
                 its index is not serving: the run completed without the index ever coming up",
                s.height()
            ),
        ),
        Err(e) => Verdict::ProbeError(format!("getaddressdeltas: {e}")),
    }
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
