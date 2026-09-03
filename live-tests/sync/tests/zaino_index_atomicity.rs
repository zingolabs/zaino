//! Index-set conformance: zaino builds every index from genesis over a pinned pre-sandblast
//! mainnet snapshot; probes assert the Index Sync Model's invariants, not today's schedule.
//!
//! - Subject = zaino, no wallet
//! - vs `zaino_index_construction`: that asserts one frontier climbing to a pin, this the relations
//!   between all six (= where the schedule lives)
//! - Written against model §8 → survives the descriptor-driven refactor; budgets move, probes stay
//!
//! # Index set (`db_schema_v1.txt` v1.3)
//!
//! ```text
//!   φ  index                        σ                ⊕         D(I)
//!   -  ---------------------------  ---------------  --------  --------------------------------
//!   0  headers                      local            append    {}
//!   0  txids txid_location heights  local            append    {}
//!   0  transparent sapling          local            append    {}
//!   0  orchard ironwood             local            append    {}
//!   0  commitment_tree_data         local (fetched)  append    {}
//!   1  spent                        cross            append    {transparent, txid_location}
//!   2  tx_out_set_accumulator       cross ∧ self     MONOIDAL  {transparent, txid_location,
//!                                                               spent}
//! ```
//!
//! - depth(G) = 3; `FinalisedTxOutSetInfoAccumulator::combine` = ⊕ (XOR + checked adds, identity 0)
//! - `rebuild_tx_out_set_accumulator` map-reduces ⊕ across txid-prefix shards → algebra exploited
//!   spatially, never temporally (§6.3 cross-batch prefix = that same `combine`)
//! - `commitment_tree_data` σ_local only because roots are a source read
//!   (`treestate_fetch_seconds`); computing them → σ_self ∧ ⊕_mono, visible as that metric dropping
//!
//! # Mainnet, Orchard rung, run to the pin
//!
//! - Accumulator = only ⊕_mono index + deepest phase = critical path; c_I(b) driven by transparent
//!   UTXO churn alone (shielded pools never enter a txout set)
//! - `max_spent_entries` = 8 GiB / 256 B = 33.5M spent outpoints → testnet never shards, ⊕ called
//!   once against the identity, φ=2 measures a phase that did no work
//! - Pin 1,693,104 = deepest mainnet artifact under [`SANDBLAST_ONSET`], by 11,219; sandblast
//!   dust makes churn pathological → `perf --base` never compares this against the Ironwood rung
//! - 20.3 GiB pull vs Ironwood's 244.8 GiB = the profile you iterate on
//! - No `until_height`: φ=2 runs once, after the block loop, at the end of `write_blocks_to_height`
//!   → stopping short leaves every φ=2 claim unevaluated on a green run. §6.4 wants φ=2 trailing
//!   φ=0 by one batch; quantifying that gap = why this profile exists
//! - zebrad peerless → chain frozen → any backwards frontier motion = bug, not rollback
//!
//! # Unasserted
//!
//! - Index contents vs zebra (end-state claim, no parity test on this fixture)
//! - Crash recovery = Inv 3's sharpest form (accumulator watermark written only after a whole pass
//!   → interrupted cold sync pays a full rebuild). Blocked: ztest's nemesis registers and prints
//!   via `describe`, never reaches the cluster; no restart primitive
//!
//! `ztest sync start zaino_index_set_atomicity`

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use ztest::prelude::*;
use ztest::snapshots::ORCHARD_MAINNET;
use ztest::sync::{
    hours, mins, secs, Op, OpSet, Severity, Snapshot, SyncCtx, SyncOutcome, SyncRunner, Verdict,
    Violation,
};

// ── cadences and caps ────────────────────────────────────────────────────────────────────────

/// 5 s would spend an hours-long build scraping; 15 s still outresolves every cadence below
const TICK: std::time::Duration = secs(15);

/// Set here, not by `sync_test(timeout = ..)` — that records the inventory cap for `sync list` +
/// QoS admission and never reaches `SyncEngine`. Half Ironwood's 48h. UNMEASURED
const RUN_CAP: std::time::Duration = hours(24);

/// Generous: one commit = a large batch, and the first minutes open a 30 GB state dir. Shorter than
/// a plausible φ=2 pass on purpose — [`index_advances`]
const STALL_WINDOW: std::time::Duration = mins(15);

/// Completion fires on the last finalised commit; φ=2 + NFS anchor/fill still to run, both unbound
/// by design. Not measured — expiry = wedged, not slow
const INDEX_UP_WINDOW: std::time::Duration = mins(30);

/// Slack: `gettxoutsetinfo` folds the whole NFS per call, and the answer is a step, not a curve
const INDEX_UP_POLL: std::time::Duration = secs(10);

/// Grace before [`required_families_are_published`] may violate — the gauges it needs appear at
/// different times (first batch commit, NFS anchor resolving over mainnet), not together
const PREFLIGHT_GRACE: std::time::Duration = mins(30);

// ── chain constants ──────────────────────────────────────────────────────────────────────────

/// Mainnet NU5, 6,000 blocks below the pin. Consensus constant, no ztest table of those; a wrong
/// value latches [`crossed_activation`] early rather than breaking → review-checked
const STRADDLED_ACTIVATION: u32 = 1_687_104;

/// Derived (2022-06-15 18:18 UTC) = first 500-block window above 3× baseline, permanent from
/// 1,706,605. No published source gives one (ECC says "June 2022"). Asserted, not commented:
/// every budget here assumes the pre-spam regime — [`fixture_is_pre_sandblast`]
const SANDBLAST_ONSET: u32 = 1_704_323;

/// = zebra's `MAX_BLOCK_REORG_HEIGHT` + 1; writer aims at `tip - MAX_NONFINALISED_DEPTH`, never the
/// tip. Consensus bound, not a knob. Mirrored, not imported (workspace links no zaino code)
const NON_FINALISED_DEPTH: u32 = 1_001;

/// `ZAINO_TXOUTSET_ENTRY_LEN` = 32 txid + 4 index + 8 value + 20 script_hash + 1 script_type.
/// Schema table 9 guarantees `bytes_serialized == txouts * 65` → an oracle needing no validator
const TXOUTSET_ENTRY_LEN: u64 = 65;

// ── budgets ──────────────────────────────────────────────────────────────────────────────────

/// Peak normalised watermark skew = Inv 3 (§8.3) as one number.
///
/// ```text
///   skew(t) = ( finalized_height(t) - accumulator_height(t) ) / |C|
/// ```
///
/// - Inv 3 + §9.2 (one watermark for crash recovery) hold ⟺ every watermark advances together at
///   batch boundaries → conforming engine keeps skew ≤ β/|C|
/// - `validated_height` excluded (async by design, symptom = a re-read = latency not atomicity) →
///   [`VALIDATION_LAG_BUDGET`] instead
/// - β not a block count (`sync_write_batch_size` = 8 GiB *heap* > this chain) → batches close
///   on the blocks cap (`batch_blocks` tops at 512) or the 120 s checkpoint ⇒ φ=2 one batch behind
///   φ=0 ≈ 1e-4 of this chain
/// - 1 % ≈ 30 batches slack: 2 orders below the defer-a-whole-phase signature (skew ≈ 1.0). Cliff
///   detector, not a tuning gate. GUESS
/// - Records ≈ 0.999 today → Recorded; fatal when a descriptor-driven engine lands
const SKEW_BUDGET: f64 = 0.01;

/// φ=2's share of wall clock = §7.3 marginal cost, measured.
///
/// ```text
///   share = zaino_sync_accumulator_seconds_sum / elapsed
/// ```
///
/// - §7.3: an index joining a phase inside its bottleneck costs ΔT = 0, one creating a phase is
///   fully additive → this ratio = the whole marginal cost of index 9
/// - 25 % = where φ=2 stops being an epilogue and becomes a co-equal traversal (≈ 2× the
///   depth(G)=3 pipelined floor); post-refactor target ≤ 0.05 (§6.4 hides it behind φ=0)
/// - GUESS: rebuild = "almost entirely sequential scans" vs a per-block loop over 1.69M blocks →
///   minutes against hours, unmeasured
const ACCUMULATOR_SHARE_BUDGET: f64 = 0.25;

/// Separate claim from [`SKEW_BUDGET`] (async by design, lag costs latency). 10 % → fires when the
/// background validator has stopped keeping up, not when it merely trails. GUESS
const VALIDATION_LAG_BUDGET: f64 = 0.10;

/// #1261 = missing anchor fell through to genesis, re-anchored up to the lagging finalised tip,
/// grinding millions of blocks one at a time. zaino at the emission site: "Once at startup =
/// normal; repeated = the #1261 shape". Clean build expects 0 or 1 (initial `initialize` goes via
/// `resolve_anchor_block`, a different call site) — unconfirmed on a live pod, so 2 = that + 1
const MAX_REANCHORS: u64 = 2;

/// `block_fetch_seconds` / `block_assemble_seconds` = separate histograms off one scrape → a block
/// in flight is counted by one and not the other. 64 covers a batch of straddle
const SPAN_COUNT_SLACK: u64 = 64;

/// Gauges set at different instants in one pass, scraped together → a reading can straddle a set.
/// 2 < any batch, > any straddle — slack against a false fatal on a 24h run, not a weakened Inv 1
/// (reading a dependency early inverts by a batch or a chain, never by one block)
const ORDER_SLACK: u32 = 2;

// ── metric families ──────────────────────────────────────────────────────────────────────────

/// Cross-repo contract nothing compiles: `zaino_state::metric_names` owns the strings, this file
/// restates them, a rename shows up only as a probe going quiet. `run.requires_work` guards the
/// per-op counters and nothing else → [`super::required_families_are_published`] guards these
mod family {
    use ztest::prelude::{Family, family};

    pub(super) const CHAIN_TIP: Family = family("zaino_chain_tip_height");
    pub(super) const TARGET: Family = family("zaino_sync_target_height");
    pub(super) const FETCHED: Family = family("zaino_sync_fetched_height");
    pub(super) const FINALIZED: Family = family("zaino_sync_finalized_height");
    pub(super) const ACCUMULATOR: Family = family("zaino_sync_accumulator_height");
    pub(super) const VALIDATED: Family = family("zaino_db_validated_height");
    pub(super) const NFS_TIP: Family = family("zaino_nfs_tip_height");

    pub(super) const NFS_REANCHORS: Family = family("zaino_nfs_reanchors_total");
    pub(super) const ACCUMULATOR_SECONDS: Family = family("zaino_sync_accumulator_seconds");
    pub(super) const BLOCK_FETCH: Family = family("zaino_sync_block_fetch_seconds");
    pub(super) const BLOCK_ASSEMBLE: Family = family("zaino_sync_block_assemble_seconds");

    /// Gauges the probes treat as required — absent = its probe unfailable = worse than a red run
    pub(super) const REQUIRED_GAUGES: [Family; 6] =
        [CHAIN_TIP, TARGET, FETCHED, FINALIZED, ACCUMULATOR, NFS_TIP];
}

/// NU5's 6,000-block tail funds the Orchard pool. No `IronwoodAction` — NU6.3 activates 1.7M blocks
/// above this pin → unmeasured counter, every `require` on it a panic
const INDEXED_POOLS: [Op; 2] = [Op::SaplingOutput, Op::OrchardAction];

// ── the profile ──────────────────────────────────────────────────────────────────────────────

#[ztest::needs(ORCHARD_MAINNET)]
#[ztest::sync_test(
    name = "zaino_index_set_atomicity",
    description = "index-set conformance over a pinned pre-sandblast mainnet snapshot",
    subject = indexer,
    timeout = "24h",
    qos = sync,
    footprint = "6c/24Gi",
    tags = ["mainnet", "zaino", "index", "orchard", "nu5", "model", "atomicity"],
)]
async fn zaino_index_set_atomicity(mut run: SyncRunner) -> SyncOutcome {
    // `?` unavailable (body returns SyncOutcome) → setup failure converts via `From`
    let (zebra, zaino) = match run
        .topology(|t| {
            // Per-pod, not the even split: zebra reads a frozen snapshot and never grows; zaino
            // holds an open LMDB write txn plus separate 8 GiB write-batch / rebuild budgets. Must
            // sum inside `footprint`. No `.disk(..)` on zebra — clone never passes the seed floor
            let zebra = t.add_validator(
                Validator::zebrad("6.2.3")
                    .snapshot(ORCHARD_MAINNET)
                    .resources(Cpu::cores(2), Mem::gib(4)),
            );
            let zaino = t.add_indexer(
                // `no_tls_with_prometheus` load-bearing (its exporter = the whole progress + cost
                // model); public-bind restated because overriding the feature list drops it
                dev!(
                    Indexer::Zainod,
                    "../../Dockerfile",
                    context = "../..",
                    features = [
                        "no_tls_with_prometheus",
                        "allow_unencrypted_public_json_rpc_bind"
                    ]
                )
                // Private CoW clone, read-only; zaino's own index starts empty
                .snapshot(ORCHARD_MAINNET)
                // `Fetch` forwards to the validator and builds no index → nothing to assert
                .tuning(ZainoTuning::State)
                // Sync tier wants a reserved PVC, not node ephemeral (eviction at hour 18 = run)
                // UNMEASURED: Ironwood declares 325 GiB over 3.43M post-sandblast blocks; this rung
                // = 49 % of that height, far less of its transactions, and the dominant tables
                // (`spent`, `txid_location`) scale with txs not block bytes → est. 65-80 GiB
                .disk(Disk::gib(96))
                .resources(Cpu::cores(4), Mem::gib(20)),
            );
            (zebra, zaino)
        })
        .await
    {
        Ok(handles) => handles,
        Err(e) => return e.into(),
    };

    // Manifest-pinned, cross-checked against the validator in `topology` — that provenance = what
    // makes them an oracle (zaino and zebra can be wrong together)
    let chain = run.chain();
    let span = chain.tip_height as f64;

    run.sync(zaino.clone());
    run.tick(TICK).timeout(RUN_CAP);
    // Checked against one live reading pre-run; otherwise a series-name mismatch surfaces as a
    // `require` panic hours in, naming the op but never the series to go find
    run.requires_work(OpSet::of(&INDEXED_POOLS));

    // Probes are `Fn` → cross-tick state lives behind an `Arc`. `started` taken here, not at the
    // first tick (overstates elapsed by ≤ 1 tick ⇒ strictly stricter cost-share verdict)
    let peak_skew = Arc::new(AtomicU32::new(0));
    let peak_validation_lag = Arc::new(AtomicU64::new(0));
    let last_finalized = Arc::new(AtomicU32::new(0));
    let started = Instant::now();

    // ── preflight: everything below is unfalsifiable without these ───────────────────────────
    run.at_completion(Severity::Fatal)
        .named("fixture_is_pre_sandblast")
        .check(move |s: &Snapshot| fixture_is_pre_sandblast(s, chain));
    {
        let zaino = zaino.clone();
        run.always(Severity::Fatal)
            .named("required_families_are_published")
            .every(mins(5))
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { required_families_are_published(s, &zaino, started).await })
            });
    }

    // ── §8.1 Inv 1: dependency precedence ────────────────────────────────────────────────────
    {
        let zaino = zaino.clone();
        run.always(Severity::Fatal)
            .named("dependency_precedence")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { dependency_precedence(s, &zaino).await })
            });
    }

    // ── §8.3 Inv 3: batch atomicity as peak watermark skew ───────────────────────────────────
    // Split observe/judge: skew = a property of the schedule, identical every tick
    {
        let zaino = zaino.clone();
        let peak = peak_skew.clone();
        run.always(Severity::Recorded)
            .named("watermark_skew_observed")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let (zaino, peak) = (zaino.clone(), peak.clone());
                Box::pin(async move { observe_watermark_skew(s, &zaino, &peak).await })
            });
    }
    {
        let peak = peak_skew.clone();
        run.at_completion(Severity::Recorded)
            .named("batch_atomicity_within_budget")
            .check(move |s: &Snapshot| batch_atomicity_within_budget(s, &peak, span));
    }

    // ── §8.2 Inv 2: composition-type conformance ─────────────────────────────────────────────
    // Associativity across reduction-tree *shape* = `zaino_accumulator_monoid_parity`'s job
    run.at_completion(Severity::Fatal)
        .named("accumulator_self_consistent")
        .check_rpc(move |s, cx| Box::pin(accumulator_self_consistent(s, cx)));

    // ── extraction accounting (§3.2) ─────────────────────────────────────────────────────────
    {
        let zaino = zaino.clone();
        run.always(Severity::Fatal)
            .named("extraction_spans_agree")
            .every(mins(2))
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { extraction_spans_agree(s, &zaino).await })
            });
    }

    // ── §7.3 cost model. Recorded: no baseline yet, the first run's answer *is* it ───────────
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Recorded)
            .named("phase_two_cost_share")
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { phase_two_cost_share(s, &zaino, started).await })
            });
    }

    // ── background maintenance ───────────────────────────────────────────────────────────────
    {
        let zaino = zaino.clone();
        let peak = peak_validation_lag.clone();
        run.always(Severity::Recorded)
            .named("validation_keeps_up")
            .every(mins(2))
            .check_rpc(move |s, _cx| {
                let (zaino, peak) = (zaino.clone(), peak.clone());
                Box::pin(async move { validation_keeps_up(s, &zaino, &peak, span).await })
            });
    }
    {
        let zaino = zaino.clone();
        run.always(Severity::Fatal)
            .named("nfs_anchored_once")
            .every(mins(2))
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { nfs_anchored_once(s, &zaino).await })
            });
    }

    // ── safety ───────────────────────────────────────────────────────────────────────────────
    {
        let zaino = zaino.clone();
        let last = last_finalized.clone();
        run.always(Severity::Fatal)
            .named("finalised_index_append_only")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let (zaino, last) = (zaino.clone(), last.clone());
                Box::pin(async move { finalised_index_append_only(s, &zaino, &last).await })
            });
    }
    run.always(Severity::Recorded)
        .named("indexed_work_monotonic")
        .each_tick()
        .check(indexed_work_monotonic);
    {
        // Captured clone, not `cx` — `SyncCtx` carries only the indexer
        let zebra = zebra.clone();
        run.always(Severity::Fatal)
            .named("index_within_pinned_tip")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let zebra = zebra.clone();
                Box::pin(async move { index_within_pinned_tip(s, &zebra, chain).await })
            });
    }

    // ── liveness ─────────────────────────────────────────────────────────────────────────────
    run.eventually(Severity::Fatal)
        .named("index_advances")
        .window(STALL_WINDOW)
        .check(index_advances);

    // ── coverage ─────────────────────────────────────────────────────────────────────────────
    run.sometimes()
        .named("observed_a_partial_index")
        .check(observed_a_partial_index);
    run.sometimes()
        .named("crossed_the_straddled_activation")
        .check(crossed_activation);
    {
        let zaino = zaino.clone();
        run.sometimes()
            .named("observed_phase_two_run")
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { observed_phase_two_run(s, &zaino).await })
            });
    }

    // ── terminal ─────────────────────────────────────────────────────────────────────────────
    run.at_completion(Severity::Fatal)
        .named("index_serves_the_pinned_tip")
        .check_rpc(move |s, cx| Box::pin(index_serves_the_pinned_tip(s, cx, chain)));
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Fatal)
            .named("finalised_seam_within_reorg_bound")
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { finalised_seam_within_reorg_bound(s, &zaino, chain).await })
            });
    }
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Fatal)
            .named("watermarks_converge_at_completion")
            .check_rpc(move |s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { watermarks_converge(s, &zaino, chain).await })
            });
    }

    run.run().await
}

// ── metric helpers ───────────────────────────────────────────────────────────────────────────

/// How long a `/metrics` read may take before the pod is called wedged. A probe budget, not a
/// measurement: these run every tick against a pod that is also indexing
const SCRAPE_TIMEOUT: std::time::Duration = secs(10);

/// `Ok(None)` = family absent ≠ zero (several unset in the opening minutes; absence-as-0 invents
/// an ordering violation out of a starting pod). Negative/non-finite = broken exporter → error
///
/// - `reduce`, not `height_gauge`: the latter folds a broken reading into `None`, which every
///   probe here reads as "not published yet" and would pass on
async fn gauge(zaino: &ZainoIndexer, family: Family) -> Result<Option<u32>, String> {
    let exposition = zaino.read(SCRAPE_TIMEOUT).await.map_err(|e| format!("{family}: {e}"))?;
    match exposition.reduce(family, Reduce::Max) {
        Some(v) if v.is_finite() && v >= 0.0 => Ok(Some(v as u32)),
        Some(v) => Err(format!("{family} read as {v}, which is not a height")),
        None => Ok(None),
    }
}

/// `reduce` sums every label set → `mode=rebuild` vs `delta` indistinguishable, likewise `stage=`
/// / `backend=`. Narrowing one needs [`family_where`], which would make [`phase_two_cost_share`]
/// a direct traversal count instead of a cost share
async fn counter(zaino: &ZainoIndexer, family: Family) -> Result<Option<u64>, String> {
    let exposition = zaino.read(SCRAPE_TIMEOUT).await.map_err(|e| format!("{family}: {e}"))?;
    match exposition.reduce(family, Reduce::Sum) {
        Some(v) if v.is_finite() && v >= 0.0 => Ok(Some(v as u64)),
        Some(v) => Err(format!("{family} read as {v}, which is not a count")),
        None => Ok(None),
    }
}

/// Histogram `_count`, folded across label sets ([`counter`] re: the fold)
async fn hist_count(zaino: &ZainoIndexer, family: Family) -> Result<Option<u64>, String> {
    let exposition = zaino.read(SCRAPE_TIMEOUT).await.map_err(|e| format!("{family}: {e}"))?;
    match exposition.tally(family) {
        Some(t) if t.count.is_finite() && t.count >= 0.0 => Ok(Some(t.count as u64)),
        Some(t) => Err(format!("{family}_count read as {}, which is not a count", t.count)),
        None => Ok(None),
    }
}

/// Histogram `_sum` in seconds, folded across label sets ([`counter`] re: the fold)
async fn hist_sum(zaino: &ZainoIndexer, family: Family) -> Result<Option<f64>, String> {
    let exposition = zaino.read(SCRAPE_TIMEOUT).await.map_err(|e| format!("{family}: {e}"))?;
    match exposition.tally(family) {
        Some(t) if t.sum.is_finite() && t.sum >= 0.0 => Ok(Some(t.sum)),
        Some(t) => Err(format!("{family}_sum read as {}, which is not a duration", t.sum)),
        None => Ok(None),
    }
}

// ── preflight ────────────────────────────────────────────────────────────────────────────────

/// A deeper rung still produces numbers, describing a different chain → repointing fails loud
/// instead of quietly changing what is measured
fn fixture_is_pre_sandblast(_s: &Snapshot, chain: ChainSnapshot) -> Verdict {
    ztest::sync_ensure!(
        chain.tip_height < SANDBLAST_ONSET,
        "this profile's cost budgets assume the pre-sandblast regime, but the fixture pins \
         {} which is at or above the derived onset {SANDBLAST_ONSET}; either repoint it below \
         the onset or recalibrate SKEW_BUDGET and ACCUMULATOR_SHARE_BUDGET against the spam \
         regime and say so here",
        chain.tip_height
    );
    Verdict::Satisfied
}

/// Absent family → its probe reads `None` forever → `Pending` ≡ green on a passing run. Catches a
/// silently unfalsifiable profile.
///
/// - `Pending` inside [`PREFLIGHT_GRACE`]: these do not appear together. `chain_tip_height` is set
///   on sync-loop iteration 1, but `finalized_height` is suppressed while `start_height == 0` (an
///   empty db) until the first batch commits, and `nfs_tip_height` waits on the anchor resolving
///   over a mainnet chain — so "any one present ⇒ all required" would false-fatal a 24h run
async fn required_families_are_published(
    s: &Snapshot,
    zaino: &ZainoIndexer,
    started: Instant,
) -> Verdict {
    let mut missing = Vec::new();
    for family in family::REQUIRED_GAUGES {
        match gauge(zaino, family).await {
            Ok(Some(_)) => {}
            Ok(None) => missing.push(family),
            Err(e) => return Verdict::ProbeError(e),
        }
    }
    if started.elapsed() < PREFLIGHT_GRACE {
        return Verdict::Pending;
    }
    ztest::sync_ensure!(
        missing.is_empty(),
        "{PREFLIGHT_GRACE:?} in, at frontier {}, zaino does not publish {missing:?}; every probe \
         reading one of those is unfalsifiable, so this run would pass without testing them. \
         The names are restated in this file's `family` module and owned by \
         `zaino_state::metric_names` — a rename on either side lands here",
        s.height()
    );
    Verdict::Satisfied
}

// ── §8.1 Inv 1 ───────────────────────────────────────────────────────────────────────────────

/// One DAG edge: `(lower, lower_name, upper, upper_name, why)`
type PrecedenceEdge<'a> = (Option<u32>, &'a str, Option<u32>, &'a str, &'a str);

/// Trivial today (φ=2 runs after φ=0/1 finish the chain) → registered now so per-edge scheduling
/// (§5.2) inherits a live guard. Edge checked only when both gauges set; [`ORDER_SLACK`] straddle
async fn dependency_precedence(s: &Snapshot, zaino: &ZainoIndexer) -> Verdict {
    let read = |f| gauge(zaino, f);
    let (tip, target, fetched, finalized, accumulator, validated, nfs_tip) = match (
        read(family::CHAIN_TIP).await,
        read(family::TARGET).await,
        read(family::FETCHED).await,
        read(family::FINALIZED).await,
        read(family::ACCUMULATOR).await,
        read(family::VALIDATED).await,
        read(family::NFS_TIP).await,
    ) {
        (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e), Ok(f), Ok(g)) => (a, b, c, d, e, f, g),
        (Err(e), ..) => return Verdict::ProbeError(e),
        (_, Err(e), ..) => return Verdict::ProbeError(e),
        (_, _, Err(e), ..) => return Verdict::ProbeError(e),
        (_, _, _, Err(e), ..) => return Verdict::ProbeError(e),
        (_, _, _, _, Err(e), ..) => return Verdict::ProbeError(e),
        (_, _, _, _, _, Err(e), _) => return Verdict::ProbeError(e),
        (_, _, _, _, _, _, Err(e)) => return Verdict::ProbeError(e),
    };

    // Data, not a run of `sync_ensure!` calls → reads as the DAG it is; a new index = a new row
    let edges: [PrecedenceEdge<'_>; 6] = [
        (
            accumulator,
            "accumulator_height",
            finalized,
            "finalized_height",
            "phi = 2 depends on the block tables (transparent, txid_location, spent); a \
             frontier above theirs means it consumed entries that were not readable",
        ),
        (
            validated,
            "validated_height",
            finalized,
            "finalized_height",
            "validation certifies written blocks; a frontier above the writer's means it \
             certified something that was never committed",
        ),
        (
            finalized,
            "finalized_height",
            fetched,
            "fetched_height",
            "a batch cannot commit blocks that extraction never produced",
        ),
        (
            fetched,
            "fetched_height",
            target,
            "target_height",
            "extraction ran past the write path's goal, so the surplus is work no index will \
             be asked to keep",
        ),
        (
            target,
            "target_height",
            tip,
            "chain_tip_height",
            "the write path is aimed above the chain it indexes",
        ),
        (
            finalized,
            "finalized_height",
            nfs_tip,
            "nfs_tip_height",
            "the non-finalised window is anchored below the finalised frontier, so the span \
             between them is covered by neither half of the index",
        ),
    ];

    for (lower, lower_name, upper, upper_name, why) in edges {
        let (Some(lower), Some(upper)) = (lower, upper) else {
            continue;
        };
        ztest::sync_ensure!(
            lower <= upper.saturating_add(ORDER_SLACK),
            "Invariant 1 (dependency precedence): {lower_name} is {lower}, above {upper_name} \
             at {upper} by more than the {ORDER_SLACK}-block scrape slack. {why}"
        );
    }
    let _ = s;
    Verdict::Satisfied
}

// ── §8.3 Inv 3 ───────────────────────────────────────────────────────────────────────────────

/// Observing half of the pair. Saturating — ordering = [`dependency_precedence`]'s job, duplicated
/// here it would report one bug as two. Absent accumulator = φ=2 never ran = max skew, not unknown
async fn observe_watermark_skew(_s: &Snapshot, zaino: &ZainoIndexer, peak: &AtomicU32) -> Verdict {
    let finalized = match gauge(zaino, family::FINALIZED).await {
        Ok(Some(h)) => h,
        Ok(None) => return Verdict::Pending,
        Err(e) => return Verdict::ProbeError(e),
    };
    let accumulator = match gauge(zaino, family::ACCUMULATOR).await {
        Ok(Some(h)) => h,
        Ok(None) => 0,
        Err(e) => return Verdict::ProbeError(e),
    };
    peak.fetch_max(finalized.saturating_sub(accumulator), Ordering::Relaxed);
    Verdict::Satisfied
}

/// Peak skew within [`SKEW_BUDGET`] (derivation + why Recorded, there). Reads `peak`, never the end
/// state — at completion skew = 0, so an end-state probe reports green on the violated property
fn batch_atomicity_within_budget(s: &Snapshot, peak: &AtomicU32, span: f64) -> Verdict {
    let blocks = peak.load(Ordering::Relaxed);
    if span <= 0.0 {
        return Verdict::ProbeError("chain span is zero; cannot normalise skew".into());
    }
    let skew = f64::from(blocks) / span;
    let budget_blocks = (SKEW_BUDGET * span) as u32;
    ztest::sync_ensure!(
        skew <= SKEW_BUDGET,
        "Invariant 3 (batch atomicity): peak watermark skew was {blocks} blocks \
         ({skew:.4} of the chain), against a budget of {budget_blocks} blocks \
         ({SKEW_BUDGET:.4}). The finalised writer and the txout-set accumulator did not \
         commit as one batch. A skew near 1.0 is the signature of an index deferred to the \
         end of the chain rather than pipelined a batch behind its dependencies; a skew of a \
         few hundred blocks is a batch-boundary stagger and is what a conforming schedule \
         looks like. Run ended at frontier {}",
        s.height()
    );
    Verdict::Satisfied
}

/// The one half of Inv 3 today's schedule satisfies → green while [`batch_atomicity_within_budget`]
/// records the violation ("eventually catches up" ≠ "kept pace"). `validated_height` excluded
async fn watermarks_converge(s: &Snapshot, zaino: &ZainoIndexer, chain: ChainSnapshot) -> Verdict {
    let deadline = Instant::now() + INDEX_UP_WINDOW;
    loop {
        let last = match (
            gauge(zaino, family::FINALIZED).await,
            gauge(zaino, family::ACCUMULATOR).await,
        ) {
            (Ok(Some(finalized)), Ok(Some(accumulator))) => {
                if accumulator >= finalized {
                    return Verdict::Satisfied;
                }
                format!(
                    "the accumulator watermark is {accumulator}, still {} blocks below the \
                     finalised frontier {finalized}",
                    finalized - accumulator
                )
            }
            (Ok(_), Ok(None)) => "the accumulator watermark is still unpublished, so phi = 2 \
                                  has not committed anything"
                .to_string(),
            (Ok(None), Ok(_)) => "the finalised frontier is unpublished".to_string(),
            (Err(e), _) | (_, Err(e)) => return Verdict::ProbeError(e),
        };
        if Instant::now() >= deadline {
            return violated(
                s.height(),
                format!(
                    "{INDEX_UP_WINDOW:?} after the finalised writer committed its last batch \
                     at frontier {} (pin {}), {last}. Invariant 3 permits a batch of skew \
                     during construction and none at rest",
                    s.height(),
                    chain.tip_height
                ),
            );
        }
        tokio::time::sleep(INDEX_UP_POLL).await;
    }
}

// ── §8.2 Inv 2 ───────────────────────────────────────────────────────────────────────────────

/// Two relations over the served accumulator; both absolute (no validator, no second zaino):
///
/// - `bytes_serialized == txouts * `[`TXOUTSET_ENTRY_LEN`] — both fields moved by one ⊕, so the
///   identity states its counter components stayed in step through every merge. zaino *also* checks
///   this before serving (`chain_index.rs`, `get_tx_out_set_info`) → a broken merge surfaces as an
///   RPC error carrying "bytes_serialized invariant violated", not as skewed fields. Kept as the
///   end-to-end check that `zaino-serve`'s separate wire struct did not reformat them apart
/// - `transactions <= txouts` — a tx counted only while it holds ≥ 1 unspent output. NOT enforced
///   anywhere, and the NFS fold that decrements it (`tx_unspent_count` seeding, per-tx 0↔>0
///   transitions) is exactly the arithmetic that would break it
///
/// Waits (φ=2 may not have run at completion)
async fn accumulator_self_consistent(s: &Snapshot, cx: &SyncCtx) -> Verdict {
    let Some(ix) = cx.indexer() else {
        return Verdict::ProbeError("accumulator_self_consistent: no indexer bound".into());
    };
    let rpc = match ix.json_rpc().await {
        Ok(rpc) => rpc,
        Err(e) => return Verdict::ProbeError(format!("zaino json_rpc: {e}")),
    };
    let deadline = Instant::now() + INDEX_UP_WINDOW;
    loop {
        let last = match rpc
            .call_value("gettxoutsetinfo", serde_json::json!([]))
            .await
        {
            Ok(v) => {
                let u64_at = |k| v.get(k).and_then(serde_json::Value::as_u64);
                match (
                    u64_at("txouts"),
                    u64_at("bytes_serialized"),
                    u64_at("transactions"),
                ) {
                    (Some(outputs), Some(bytes), Some(transactions)) => {
                        let want = outputs.saturating_mul(TXOUTSET_ENTRY_LEN);
                        ztest::sync_ensure!(
                            bytes == want,
                            "Invariant 2 (merge determinism): the txout-set accumulator reports \
                             {outputs} unspent outputs and {bytes} serialized bytes, but the \
                             schema defines bytes_serialized == txouts * {TXOUTSET_ENTRY_LEN}, \
                             which is {want}. The monoid's counter components are out of step, \
                             so some merge added an entry to one and not the other"
                        );
                        ztest::sync_ensure!(
                            transactions <= outputs,
                            "Invariant 2 (merge determinism): the txout-set accumulator reports \
                             {transactions} transactions holding at least one unspent output, \
                             but only {outputs} unspent outputs exist. Each counted transaction \
                             owns one, so transactions cannot exceed txouts — the per-tx 0<->>0 \
                             transition that decrements the counter has lost track"
                        );
                        return Verdict::Satisfied;
                    }
                    _ => "zaino still answers `gettxoutsetinfo` without the accumulator fields, \
                          so phi = 2 has not published a result yet"
                        .to_string(),
                }
            }
            Err(e) => format!("`gettxoutsetinfo` did not answer: {e}"),
        };
        if Instant::now() >= deadline {
            return violated(
                s.height(),
                format!(
                    "{INDEX_UP_WINDOW:?} after the finalised writer committed its last batch \
                     at frontier {}, {last}",
                    s.height()
                ),
            );
        }
        tokio::time::sleep(INDEX_UP_POLL).await;
    }
}

// ── extraction accounting ────────────────────────────────────────────────────────────────────

/// zaino's contract: assemble exceeds fetch only by reorg rebuilds (re-assemble, no re-fetch) →
/// frozen chain ⇒ equal within [`SPAN_COUNT_SLACK`]. Precedent: the accounting this replaced had
/// two stages behind one label, a derived span silently negative. Misses → `fetch_misses_total`
async fn extraction_spans_agree(s: &Snapshot, zaino: &ZainoIndexer) -> Verdict {
    let (fetch, assemble) = match (
        hist_count(zaino, family::BLOCK_FETCH).await,
        hist_count(zaino, family::BLOCK_ASSEMBLE).await,
    ) {
        (Ok(Some(f)), Ok(Some(a))) => (f, a),
        (Ok(_), Ok(_)) => return Verdict::Pending,
        (Err(e), _) | (_, Err(e)) => return Verdict::ProbeError(e),
    };
    let diff = fetch.abs_diff(assemble);
    ztest::sync_ensure!(
        diff <= SPAN_COUNT_SLACK,
        "extraction spans disagree by {diff} blocks at frontier {}: block_fetch counted \
         {fetch}, block_assemble counted {assemble}. On a frozen chain the two must match — \
         zaino's contract is that assemble exceeds fetch only by reorg rebuilds, and this \
         chain cannot reorg. A gap larger than the {SPAN_COUNT_SLACK}-block scrape straddle \
         means the three spans are no longer disjoint or no longer cover the same work",
        s.height()
    );
    Verdict::Satisfied
}

// ── §7.3 cost model ──────────────────────────────────────────────────────────────────────────

/// Threshold + why it is a guess: [`ACCUMULATOR_SHARE_BUDGET`]. Folded over `mode` = `current +
/// delta + rebuild`. Absent = φ=2 never ran = [`observed_phase_two_run`]'s find, not billed twice
async fn phase_two_cost_share(s: &Snapshot, zaino: &ZainoIndexer, started: Instant) -> Verdict {
    let seconds = match hist_sum(zaino, family::ACCUMULATOR_SECONDS).await {
        Ok(Some(v)) => v,
        Ok(None) => return Verdict::Satisfied,
        Err(e) => return Verdict::ProbeError(e),
    };
    let elapsed = started.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return Verdict::ProbeError("elapsed run time is zero".into());
    }
    let share = seconds / elapsed;
    ztest::sync_ensure!(
        share <= ACCUMULATOR_SHARE_BUDGET,
        "phase 2 (txout-set accumulator) spent {seconds:.0}s of a {elapsed:.0}s run, a share \
         of {share:.3} against a budget of {ACCUMULATOR_SHARE_BUDGET:.3}. Section 7.3: an \
         index that creates its own phase is fully additive, so this is the whole marginal \
         cost of that index under the current schedule. A share this high means the run paid \
         for roughly a second traversal of the chain rather than a tail on the first. Frontier \
         at completion {}",
        s.height()
    );
    Verdict::Satisfied
}

// ── background maintenance ───────────────────────────────────────────────────────────────────

/// Outside the Inv 3 skew (async by design, symptom = an on-demand re-read above it). Catches a
/// validator that stopped, not one that trails; Recorded — a slow validator corrupts nothing
async fn validation_keeps_up(
    s: &Snapshot,
    zaino: &ZainoIndexer,
    peak: &AtomicU64,
    span: f64,
) -> Verdict {
    let (finalized, validated) = match (
        gauge(zaino, family::FINALIZED).await,
        gauge(zaino, family::VALIDATED).await,
    ) {
        (Ok(Some(f)), Ok(Some(v))) => (f, v),
        (Ok(_), Ok(_)) => return Verdict::Pending,
        (Err(e), _) | (_, Err(e)) => return Verdict::ProbeError(e),
    };
    let lag = finalized.saturating_sub(validated);
    peak.fetch_max(u64::from(lag), Ordering::Relaxed);
    if span <= 0.0 {
        return Verdict::ProbeError("chain span is zero; cannot normalise validation lag".into());
    }
    let normalised = f64::from(lag) / span;
    ztest::sync_ensure!(
        normalised <= VALIDATION_LAG_BUDGET,
        "the structural validation frontier is {validated}, {lag} blocks behind the finalised \
         frontier {finalized} ({normalised:.3} of the chain, budget \
         {VALIDATION_LAG_BUDGET:.3}). Validation is asynchronous by design, so a trailing \
         frontier is normal and only costs an on-demand re-read on the path above it; a lag \
         this large means it is not keeping up at all. Snapshot frontier {}",
        s.height()
    );
    Verdict::Satisfied
}

/// #1261 guard ([`MAX_REANCHORS`] for the bound). One counter, chain-length independent, and
/// invisible in every other series — the frontier still advances, having done the work many times
async fn nfs_anchored_once(s: &Snapshot, zaino: &ZainoIndexer) -> Verdict {
    let reanchors = match counter(zaino, family::NFS_REANCHORS).await {
        Ok(Some(n)) => n,
        Ok(None) => return Verdict::Pending,
        Err(e) => return Verdict::ProbeError(e),
    };
    ztest::sync_ensure!(
        reanchors <= MAX_REANCHORS,
        "the non-finalised state has re-anchored {reanchors} times by frontier {} (budget \
         {MAX_REANCHORS}). One at startup is normal; repetition is the #1261 shape, where a \
         missing anchor falls through to genesis and re-anchors up to the lagging finalised \
         tip, re-walking the window every pass",
        s.height()
    );
    Verdict::Satisfied
}

// ── safety ───────────────────────────────────────────────────────────────────────────────────

/// zaino's glossary: finalised state = "append-only: never incrementally rolled back". No
/// reorg-depth tolerance (unlike on a live chain) — peerless zebrad ⇒ no reorg can have asked.
///
/// - Reads [`family::FINALIZED`], NOT `s.height()`: ztest's `live_height` prefers `HEIGHTS.live`
///   = `fetched_height`, which is *legitimately* non-monotone (an interrupted pass restarts at
///   committed_tip + 1 and re-fetches) → gating on it asserts something that is not an invariant
/// - `last` starts at 0, so the first reading always passes
async fn finalised_index_append_only(
    _s: &Snapshot,
    zaino: &ZainoIndexer,
    last: &AtomicU32,
) -> Verdict {
    let now = match gauge(zaino, family::FINALIZED).await {
        Ok(Some(h)) => h,
        Ok(None) => return Verdict::Pending,
        Err(e) => return Verdict::ProbeError(e),
    };
    let prev = last.fetch_max(now, Ordering::Relaxed);
    ztest::sync_ensure!(
        now >= prev,
        "finalised index frontier went backwards {prev} -> {now} on a frozen chain, where no \
         reorg can have asked it to"
    );
    Verdict::Satisfied
}

/// ⊕_append observed (disjoint keys → running sum); a fall = double-count-and-correct or a
/// re-entered range. `require` not `get` — comparing two absent values makes this unfailable
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

/// Two independent statements of where the chain ends: the validator's live height + the manifest
/// pin. Manifest = stronger (written before either pod existed ⇒ cannot be dragged along by
/// whatever zaino and zebra agree to be wrong about).
///
/// - `s.height()` here and in the coverage probes = `fetched_height`, the ingest frontier, which is
///   what "has zaino reached blocks past X" wants. Probes naming the *committed* frontier read
///   [`family::FINALIZED`] instead — ztest's `live_height` prefers `HEIGHTS.live`, and they differ
///   by up to a batch
async fn index_within_pinned_tip(
    s: &Snapshot,
    validator: &ZebraValidator,
    chain: ChainSnapshot,
) -> Verdict {
    let pinned = chain.tip_height;
    ztest::sync_ensure!(
        s.height() <= pinned,
        "index frontier {} is above the snapshot's pinned tip {pinned}; the chain cannot have \
         grown, so the frontier is describing blocks that do not exist",
        s.height()
    );
    let live = match validator.chain_height().await {
        Ok(h) => u32::from(h),
        Err(e) => return Verdict::ProbeError(format!("validator chain_height: {e}")),
    };
    ztest::sync_ensure!(
        live == pinned,
        "the validator reports height {live} but the snapshot pins {pinned}: this chain is not \
         frozen, and every invariant in this profile that assumes it is has been measuring \
         something else"
    );
    ztest::sync_ensure!(
        s.height() <= live,
        "index frontier {} is ahead of the validator it indexes ({live})",
        s.height()
    );
    Verdict::Satisfied
}

// ── liveness ─────────────────────────────────────────────────────────────────────────────────

/// Frontier idle while φ=2 runs → a long *mid-run* φ=2 reads as a stall, correctly (§6.4 wants it
/// pipelined, the writer not idle on it). Terminal φ=2 escapes: liveness stops at completion, which
/// fires on the last finalised commit — before the work the terminal probes wait through
fn index_advances(s: &Snapshot) -> Verdict {
    if s.progressed_within(STALL_WINDOW) {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── coverage ─────────────────────────────────────────────────────────────────────────────────

/// Anti-vacuity latch: zaino's state backend proxies its validator until its own index serves → a
/// subject reading a proxied height opens at the tip, completes on tick one, observes nothing.
/// Never latching = that happened, and every safety probe ran against a static frontier
fn observed_a_partial_index(s: &Snapshot) -> Verdict {
    match s.target() {
        Some(target) if s.height() < target => Verdict::Satisfied,
        _ => Verdict::Pending,
    }
}

/// Below NU5 `Op::OrchardAction` legitimately flat all run → `indexed_work_monotonic` passes
/// proving nothing
fn crossed_activation(s: &Snapshot) -> Verdict {
    if s.height() >= STRADDLED_ACTIVATION {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

/// Anti-vacuity for every φ=2 claim: [`accumulator_self_consistent`] waits for it and
/// [`phase_two_cost_share`] passes without it → this latch pays for that tolerance, once
async fn observed_phase_two_run(_s: &Snapshot, zaino: &ZainoIndexer) -> Verdict {
    match hist_count(zaino, family::ACCUMULATOR_SECONDS).await {
        Ok(Some(n)) if n > 0 => Verdict::Satisfied,
        Ok(_) => Verdict::Pending,
        Err(e) => Verdict::ProbeError(e),
    }
}

// ── terminal ─────────────────────────────────────────────────────────────────────────────────

/// One call, both claims, neither observable without the other:
///
/// - Still catching up → the empty object zcashd returns when stats collection fails (spent-index
///   invariants do not hold yet) ⇒ an answer *with* a height = the index is up
/// - That height = `non_finalized_snapshot.best_tip`, no proxy path through it ⇒ unlike each height
///   zaino forwards, it cannot be the validator's answer wearing zaino's name
/// - So the tip claim is the NFS's; other half = [`finalised_seam_within_reorg_bound`]
/// - Waits, retrying errors: a refused connection while the pod scans the whole chain is expected
async fn index_serves_the_pinned_tip(s: &Snapshot, cx: &SyncCtx, chain: ChainSnapshot) -> Verdict {
    let Some(ix) = cx.indexer() else {
        return Verdict::ProbeError("index_serves_the_pinned_tip: no indexer bound".into());
    };
    let rpc = match ix.json_rpc().await {
        Ok(rpc) => rpc,
        Err(e) => return Verdict::ProbeError(format!("zaino json_rpc: {e}")),
    };
    let pinned = chain.tip_height;
    let deadline = Instant::now() + INDEX_UP_WINDOW;
    loop {
        // Why this attempt failed, so expiry names the state, not only the height
        let last = match rpc
            .call_value("gettxoutsetinfo", serde_json::json!([]))
            .await
        {
            // Keyed on the field, not a shape name: the two arms serialize untagged
            Ok(v) => match v.get("height").and_then(serde_json::Value::as_u64) {
                Some(h) if h == u64::from(pinned) => return Verdict::Satisfied,
                Some(h) => format!(
                    "the index is serving but its non-finalised tip is {h}, not the pinned \
                     {pinned}"
                ),
                None => "zaino still answers `gettxoutsetinfo` empty, so its finalised state \
                         is not caught up and the index is not serving"
                    .to_string(),
            },
            Err(e) => format!("`gettxoutsetinfo` did not answer: {e}"),
        };
        if Instant::now() >= deadline {
            return violated(
                s.height(),
                format!(
                    "{INDEX_UP_WINDOW:?} after the finalised writer committed its last batch \
                     at frontier {}, {last}",
                    s.height()
                ),
            );
        }
        tokio::time::sleep(INDEX_UP_POLL).await;
    }
}

/// Writer aimed at `tip - MAX_NONFINALISED_DEPTH`, never the tip → `frontier == pinned` asserts a
/// height the design forbids. The bound is what is worth asserting: deeper on a frozen chain = a
/// writer short of its own target, not a chain that moved.
///
/// - Reads [`family::FINALIZED`], not `s.height()` (= `fetched_height`) — the seam is a property
///   of the committed frontier, and the two differ by up to a batch
/// - Zero margin by construction: `target` = `tip - OPERATIONAL_NFS_DEPTH`, so a completed build
///   lands at `lag == NON_FINALISED_DEPTH` exactly. Enabling `fast-test-seam` on the pod would
///   move zaino's depth to 100 and fail this — the feature must stay off the `dev!` list
async fn finalised_seam_within_reorg_bound(
    _s: &Snapshot,
    zaino: &ZainoIndexer,
    chain: ChainSnapshot,
) -> Verdict {
    let pinned = chain.tip_height;
    let frontier = match gauge(zaino, family::FINALIZED).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Verdict::ProbeError("finalised frontier unpublished at completion".into())
        }
        Err(e) => return Verdict::ProbeError(e),
    };
    // Repeated from `index_within_pinned_tip`: the subtraction would otherwise wrap into a pass
    ztest::sync_ensure!(
        frontier <= pinned,
        "finalised frontier {frontier} is above the snapshot's pinned tip {pinned}"
    );
    let lag = pinned - frontier;
    ztest::sync_ensure!(
        lag <= NON_FINALISED_DEPTH,
        "finalised frontier finished at {frontier}, {lag} blocks below the pinned tip \
         {pinned}: zaino finalises up to `tip - {NON_FINALISED_DEPTH}`, so anything deeper is \
         the writer falling short of its own target on a chain that cannot have moved"
    );
    Verdict::Satisfied
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────

fn violated(height: u32, detail: String) -> Verdict {
    Verdict::Violated(Violation {
        probe: String::new(),
        height: Some(height),
        detail,
    })
}
