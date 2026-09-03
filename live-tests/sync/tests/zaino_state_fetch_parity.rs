//! Differential across zaino's two ingest paths: two pods index one pinned mainnet chain from one
//! validator — direct off zebra's state DB, rpc over JSON-RPC — and their indexes must match all
//! the way up.
//!
//! Sharp because `backend = "direct" | "rpc"` (`state`/`fetch` = legacy aliases) picks *only* how
//! zaino reaches its validator; one `NodeBackedIndexerService` builds the index either way:
//!
//! ```text
//! Rpc    → ValidatorConnector::spawn_fetch ─┐
//!                                           ├→ NodeBackedChainIndex::new(source, ..)
//! Direct → ValidatorConnector::spawn_state ─┘
//! ```
//!
//! - Same indexing code both sides → two independent constructions of one function over one chain
//! - Disagreement therefore lands on the source layer (block decoded differently, ordering not
//!   shared, something one path sees) — none of a zaino-vs-zebra comparison's index-bug-or-RPC-
//!   translation ambiguity
//! - NOT an authority check: two sources wrong the same way agree. `zaino_index_construction`
//!   holds the index to external truth; this asks whether the ingest path changes the answer
//!
//! Fixture = mainnet pinned 3,434,143 (NU6.3/Ironwood rung, deepest artifact either network ships):
//!
//! - Mainnet load-bearing: a differential is only as sharp as its corpus, and the source layer
//!   diverges most on dense irregular blocks (testnet's sparse history = where paths agree easiest)
//! - Spans every mainnet activation → v5 txs, funded Orchard pool, NU6.3 rules on both sides, so
//!   `z_gettreestate` has trees to disagree about (the old Blossom rung had none)
//! - Spans both density regimes: sandblast onset derived, not cited (ECC says only "June 2022")
//!   = first 500-block window above 3x baseline @ 1,704,323, permanent from 1,706,605; this pin
//!   sits ~1.7M above it
//! - Heights all read off the manifest → repointing = the handle alone
//!
//! Volumes:
//!
//! - Seed PVC = extracted chain archive, sized by `seed_size_for` off its manifest
//! - Neither index seeded (both from empty) → each declares its own `INDEX_DISK_GIB` PVC, and only
//!   the direct pod's CoW clone touches the seed
//!
//! Launched detached: `ztest sync start zaino_state_fetch_parity`

use serde_json::{json, Value};
use ztest::prelude::*;
use ztest::snapshots::IRONWOOD_MAINNET;
use ztest::sync::{
    hours, mins, secs, Severity, Snapshot, SyncCtx, SyncOutcome, SyncRunner, Verdict, Violation,
};

/// Snapshot cadence. Two indexers over 3.4M dense blocks = an hours-long run; every tick costs
/// two exporter scrapes and the frontier needs no finer resolution
const TICK: std::time::Duration = secs(15);

/// Run cap, set here — `sync_test(timeout = ..)` records the inventory's declared cap but never
/// reaches `SyncEngine`
const RUN_CAP: std::time::Duration = hours(48);

/// Bound subject's frontier may sit this long before the run is called stalled. Wider than the
/// direct path's: this binds the JSON-RPC indexer, whose progress arrives in whole wire batches
const STALL_WINDOW: std::time::Duration = mins(30);

/// Full parity sweeps per run — the coverage/cost dial, held fixed across rungs.
///
/// - Sweep = the expensive probe (several RPC round trips against both pods) → paced by chain
///   progress, not the clock, spreading evenly across history
/// - Count fixed + stride derived from the manifest, never the reverse: a fixed stride was 66
///   sweeps over Blossom's 659,600 and would be 169 here, taxing a deeper run for being deeper
const SWEEPS_PER_RUN: u32 = 66;

/// Unbound indexer's grace after the subject finishes, before the terminal sweep gives up on
/// comparing two complete indexes. Engine completes on the *subject* and the pods do not finish
/// together — this covers the gap, not the build
const CONVERGE_BUDGET: std::time::Duration = hours(4);

/// Re-probe interval inside [`CONVERGE_BUDGET`]
const CONVERGE_POLL: std::time::Duration = secs(30);

/// Per-pod index reservation. Two full indexes over 3.4M mainnet blocks, each in its own
/// PVC — the sync tier refuses an `emptyDir` scratch, and a 48-hour build evicted under
/// DiskPressure costs the whole run. Unmeasured; revisit off `zaino_db_used_bytes`
const INDEX_DISK_GIB: u64 = 325;

/// Per-indexer memory, both pods alike.
///
/// - Same as `zaino_sync`'s indexer on this fixture, unmeasured there too
/// - Covers the open LMDB write txn's dirty pages (the run's real memory cost)
/// - Fetch = State: it builds its own index over the same chain, so the write txn is the same
///   shape whatever fed it. First number to revisit once a run reports
const INDEX_MEM_GIB: u64 = 20;

/// JSON-RPC error a *proxied* call returns. Matched on wire text (ztest's `RpcError` keeps the
/// code inside its transport source, not as a field) — brittle, but the code is its stable half
const PROXY_METHOD_NOT_FOUND: &str = "-32601";

#[ztest::needs(IRONWOOD_MAINNET)]
#[ztest::sync_test(
    name = "zaino_state_fetch_parity",
    description = "two ingest paths index one pinned NU6.3 mainnet snapshot; their indexes must agree",
    subject = indexer,
    timeout = "48h",
    qos = sync,
    footprint = "10c/44Gi",
    tags = ["mainnet", "zaino", "index", "differential", "ironwood", "nu6.3"],
)]
async fn zaino_state_fetch_parity(mut run: SyncRunner) -> SyncOutcome {
    // One validator, two indexers over it.
    // - Direct pod gets its own CoW clone of the archive (it reads a zebra state DB)
    // - Rpc pod gets none: `materialize_opts` withholds it (that pod sources blocks from the
    //   validator and would never open the mount)
    // - Pods sized as `zaino_sync`'s, two indexers not one; must sum inside `footprint`
    let (zaino_state, zaino_fetch) = match run
        .topology(|t| {
            t.add_validator(
                Validator::zebrad("6.2.3")
                    .snapshot(IRONWOOD_MAINNET)
                    .resources(Cpu::cores(2), Mem::gib(4)),
            );
            let zaino_state = t.add_indexer(
                zaino()
                    .snapshot(IRONWOOD_MAINNET)
                    .tuning(ZainoTuning::State)
                    .disk(Disk::gib(INDEX_DISK_GIB))
                    .resources(Cpu::cores(4), Mem::gib(INDEX_MEM_GIB)),
            );
            let zaino_fetch = t.add_indexer(
                zaino()
                    .snapshot(IRONWOOD_MAINNET)
                    .tuning(ZainoTuning::Fetch)
                    .disk(Disk::gib(INDEX_DISK_GIB))
                    .resources(Cpu::cores(4), Mem::gib(INDEX_MEM_GIB)),
            );
            (zaino_state, zaino_fetch)
        })
        .await
    {
        Ok(handles) => handles,
        Err(e) => return e.into(),
    };

    let chain = run.chain();

    // Rpc pod = the bound subject, and which one matters (completion predicate is the subject's).
    // - Pods do not finish together: direct reads a local RocksDB, rpc pulls all 3,434,143 over
    //   JSON-RPC → rpc = long pole by a wide margin
    // - Binding the laggard makes completion nearly "both done"; `indexes_converged` covers the gap
    run.sync(zaino_fetch);
    run.tick(TICK).timeout(RUN_CAP);

    // ── safety: neither index misbehaves, and they never disagree ──
    run.always(Severity::Fatal)
        .named("subject_index_append_only")
        .every(secs(30))
        .check(subject_index_append_only);
    {
        let zaino_state = zaino_state.clone();
        run.always(Severity::Fatal)
            .named("frontiers_within_pin")
            .every(secs(30))
            .check_rpc(move |s, _cx| {
                let zaino_state = zaino_state.clone();
                Box::pin(async move { frontiers_within_pin(s, &zaino_state, chain).await })
            });
    }
    {
        let zaino_state = zaino_state.clone();
        run.always(Severity::Fatal)
            .named("indexes_agree_over_common_window")
            // `.max(1)` so a fixture shorter than the sweep count cannot ask for a
            // stride of zero; on any real rung this is thousands of blocks.
            .every_blocks((chain.tip_height / SWEEPS_PER_RUN).max(1))
            .check_rpc(move |s, cx| {
                let zaino_state = zaino_state.clone();
                Box::pin(async move { indexes_agree_over_common_window(s, cx, &zaino_state).await })
            });
    }

    // ── liveness ──
    run.eventually(Severity::Fatal)
        .named("subject_index_advances")
        .window(STALL_WINDOW)
        .check(subject_index_advances);

    // ── coverage: without these the sweeps above can all be vacuous ──
    {
        let zaino_state = zaino_state.clone();
        run.sometimes()
            .named("observed_both_mid_build")
            .check_rpc(move |s, _cx| {
                let zaino_state = zaino_state.clone();
                Box::pin(async move { observed_both_mid_build(s, &zaino_state).await })
            });
    }
    {
        let zaino_state = zaino_state.clone();
        run.sometimes()
            .named("both_answered_from_their_own_index")
            .check_rpc(move |s, cx| {
                let zaino_state = zaino_state.clone();
                Box::pin(
                    async move { both_answered_from_their_own_index(s, cx, &zaino_state).await },
                )
            });
    }

    // ── terminal: both indexes complete, and identical at the pinned tip ──
    {
        let zaino_state = zaino_state.clone();
        run.at_completion(Severity::Fatal)
            .named("indexes_converged")
            .check_rpc(move |s, _cx| {
                let zaino_state = zaino_state.clone();
                Box::pin(async move { indexes_converged(s, &zaino_state, chain).await })
            });
    }
    {
        let zaino_state = zaino_state.clone();
        run.at_completion(Severity::Fatal)
            .named("indexes_agree_at_the_pinned_tip")
            .check_rpc(move |s, cx| {
                let zaino_state = zaino_state.clone();
                Box::pin(async move {
                    indexes_agree_at_the_pinned_tip(s, cx, &zaino_state, chain).await
                })
            });
    }

    run.run().await
}

/// One image for both indexers, built from this repo with metrics.
///
/// - Metrics feature load-bearing: its frontier gauge = the only surface either pod's index
///   progress reads from, and this profile reads both
/// - Function, not a `let`: `dev!` bakes a build spec, and the two pods must differ in nothing
///   but tuning (sharing one builder = cloning a half-configured component and hoping)
fn zaino() -> ztest::component::Indexer<ztest::backends::zainod::ZainoBackend> {
    dev!(
        Indexer::Zainod,
        "../../Dockerfile",
        context = "../..",
        features = [
            "no_tls_with_prometheus",
            "allow_unencrypted_public_json_rpc_bind"
        ]
    )
}

// ── safety ───────────────────────────────────────────────────────────────

/// Subject's finalised index never rolls back. Peerless zebrad → frozen chain → no reorg can
/// have asked for one, so backwards motion = the index losing ground it had written
fn subject_index_append_only(s: &Snapshot) -> Verdict {
    ztest::sync_ensure!(
        s.height() >= s.prev_height(),
        "the rpc indexer's frontier went backwards {} -> {} on a frozen chain",
        s.prev_height(),
        s.height()
    );
    Verdict::Satisfied
}

/// Neither index claims more chain than the artifact pins.
///
/// - Against the manifest, not either pod: the two indexers are each other's reference for
///   *content*, and a shared misreading of where the chain ends is what a differential cannot see
/// - Pin written by the producer before either pod existed
async fn frontiers_within_pin(
    s: &Snapshot,
    direct: &ZainoIndexer,
    chain: ChainSnapshot,
) -> Verdict {
    let pinned = chain.tip_height;
    let direct_at = match direct.index_frontier().await {
        Ok(h) => h,
        Err(e) => return Verdict::ProbeError(format!("direct index_frontier: {e}")),
    };
    for (label, height) in [("rpc", s.height()), ("direct", direct_at)] {
        ztest::sync_ensure!(
            height <= pinned,
            "the {label} indexer's frontier {height} is above the snapshot's pinned tip \
             {pinned}; the chain cannot have grown, so it is describing blocks that do \
             not exist"
        );
    }
    Verdict::Satisfied
}

/// Spine of the profile: the two indexes agree over everything both have built.
///
/// - Window = `min(frontier_rpc, frontier_direct)`, the height both have written
/// - Above it one pod answers from its index while the other still proxies the validator → a
///   difference there is a scheduling artifact, not a disagreement
/// - Window widens as the run proceeds; every sweep re-reads it
async fn indexes_agree_over_common_window(
    s: &Snapshot,
    cx: &SyncCtx,
    direct: &ZainoIndexer,
) -> Verdict {
    let direct_at = match direct.index_frontier().await {
        Ok(h) => h,
        Err(e) => return Verdict::ProbeError(format!("direct index_frontier: {e}")),
    };
    let window = s.height().min(direct_at);
    if window == 0 {
        // Neither pod has written anything both can be asked about yet.
        return Verdict::Satisfied;
    }
    compare_indexes(cx, direct, window, sweep_heights(window)).await
}

// ── liveness ─────────────────────────────────────────────────────────────

fn subject_index_advances(s: &Snapshot) -> Verdict {
    if s.progressed_within(STALL_WINDOW) {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── coverage ─────────────────────────────────────────────────────────────

/// At least one tick caught both pods mid-build. Anti-vacuity latch for the differential:
/// attaching after both indexes exist compares two finished artifacts and witnesses no
/// construction (a fine end-state test, and not this one)
async fn observed_both_mid_build(s: &Snapshot, direct: &ZainoIndexer) -> Verdict {
    let Some(target) = s.target() else {
        return Verdict::Pending;
    };
    let direct_at = match direct.index_frontier().await {
        Ok(h) => h,
        Err(e) => return Verdict::ProbeError(format!("direct index_frontier: {e}")),
    };
    let mid_build = |h: u32| h > 0 && h < target;
    if mid_build(s.height()) && mid_build(direct_at) {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

/// At least one tick caught both pods answering from their own index, not forwarding to the
/// validator. Load-bearing coverage probe here.
///
/// - Mid-build zaino forwards rather than errors, and `chain_tips_for_snapshot` is generic over
///   the source → both connection types do it
/// - Two proxying pods agree perfectly → every sweep above = the validator vs itself, twice,
///   passing having measured nothing
/// - `getaddressdeltas` = the discriminator, one-sided: zebra implements no such method and
///   `ChainIndex::get_address_deltas` forwards rather than synthesizes, so only a read-state pod
///   (`direct`) can answer and `rpc` returns `-32601` for the run's life, by construction
/// - Distinctness therefore = `direct` answers ∧ `rpc` refuses (requiring both to answer latches
///   `Pending` forever)
async fn both_answered_from_their_own_index(
    s: &Snapshot,
    cx: &SyncCtx,
    direct: &ZainoIndexer,
) -> Verdict {
    let (rpc_rpc, direct_rpc) = match clients(cx, direct).await {
        Ok(pair) => pair,
        Err(v) => return v,
    };
    // A selector no index needs to be populated to answer: the probe is whether
    // the *method* is served, and an empty result proves that as well as a
    // populated one.
    let selector = json!([{ "addresses": [], "start": 0, "end": s.height().max(1) }]);
    match direct_rpc
        .call_value("getaddressdeltas", selector.clone())
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains(PROXY_METHOD_NOT_FOUND) => return Verdict::Pending,
        Err(e) => return Verdict::ProbeError(format!("direct getaddressdeltas: {e}")),
    }
    match rpc_rpc.call_value("getaddressdeltas", selector).await {
        Err(e) if e.to_string().contains(PROXY_METHOD_NOT_FOUND) => Verdict::Satisfied,
        Ok(v) => Verdict::ProbeError(format!(
            "the rpc pod served getaddressdeltas ({v}); it holds no read-state, so either \
             the tuning did not take or both pods share one source and every sweep in this \
             profile is an identity"
        )),
        Err(e) => Verdict::ProbeError(format!("rpc getaddressdeltas: {e}")),
    }
}

// ── terminal ─────────────────────────────────────────────────────────────

/// Both indexes reached the pinned tip.
///
/// - Engine completes on the subject → direct may still be finishing
/// - Waits that gap out, bounded (a pod that never converges = a finding, not a wait)
/// - So the sweep after compares two *complete* indexes, not one against a prefix
async fn indexes_converged(s: &Snapshot, direct: &ZainoIndexer, chain: ChainSnapshot) -> Verdict {
    let pinned = chain.tip_height;
    ztest::sync_ensure!(
        s.height() == pinned,
        "the rpc indexer finished at {} but the snapshot pins its tip at {pinned}",
        s.height()
    );
    let started = std::time::Instant::now();
    loop {
        match direct.index_frontier().await {
            Ok(h) if h == pinned => return Verdict::Satisfied,
            Ok(h) => {
                if started.elapsed() >= CONVERGE_BUDGET {
                    return violated(
                        s.height(),
                        format!(
                            "the rpc indexer reached the pinned tip {pinned}, but \
                             {CONVERGE_BUDGET:?} later the direct indexer is still at {h}: the \
                             two ingest paths did not converge on the same chain"
                        ),
                    );
                }
                tokio::time::sleep(CONVERGE_POLL).await;
            }
            Err(e) => return Verdict::ProbeError(format!("direct index_frontier: {e}")),
        }
    }
}

/// The full sweep over two finished indexes, at the heights worth querying.
///
/// [`boundary_heights`] rather than the sampled ladder the mid-run sweeps use: by
/// here both indexes are complete, so the interesting heights are the pinned ones
/// — either side of the activation this fixture straddles, and the mature tip.
async fn indexes_agree_at_the_pinned_tip(
    _s: &Snapshot,
    cx: &SyncCtx,
    direct: &ZainoIndexer,
    chain: ChainSnapshot,
) -> Verdict {
    let tip = chain.tip_height;
    compare_indexes(cx, direct, tip, boundary_heights(tip)).await
}

/// Heights a finished index is worth comparing at.
///
/// - Straddles the activation both sides (the boundary the fixture exists for, and where a rule
///   change could make two implementations diverge)
/// - Plus a height whose coinbase is certainly spendable
/// - `tip - 1`, not the tip: at the exact tip an off-by-one pin surfaces as an RPC error, not the
///   disagreement a differential exists to catch
fn boundary_heights(tip: u32) -> Vec<u32> {
    let mut heights = vec![
        STRADDLED_ACTIVATION - 1,
        STRADDLED_ACTIVATION,
        STRADDLED_ACTIVATION + 1,
        tip.saturating_sub(COINBASE_MATURITY_MARGIN),
        tip.saturating_sub(1),
    ];
    heights.sort_unstable();
    heights.dedup();
    heights
}

/// Mainnet NU6.3, the upgrade `IRONWOOD_MAINNET` straddles — 6,000 blocks below its pin
const STRADDLED_ACTIVATION: u32 = 3_428_143;

/// Blocks below the tip whose coinbase outputs are certainly spendable. 10x the
/// 100-block maturity: a query against an immature coinbase returns a legitimately
/// empty result, which a differential test misreads as agreement
const COINBASE_MATURITY_MARGIN: u32 = 1_000;

// ── the comparison ───────────────────────────────────────────────────────

/// The heights a mid-run sweep queries, given the window both indexes cover.
///
/// A geometric ladder rather than a stride: it puts most of its samples near the
/// frontier, where the freshly written blocks are and where an ordering bug
/// would show, while still reaching back to the bottom of the index on every
/// sweep so a range that was corrupted long ago cannot stop being checked.
fn sweep_heights(window: u32) -> Vec<u32> {
    let mut heights = vec![1, window];
    let mut back = 1u32;
    while back < window {
        heights.push(window - back);
        back = back.saturating_mul(8);
    }
    heights.sort_unstable();
    heights.dedup();
    heights
}

/// Ask both pods the same index-derived questions and require identical answers.
///
/// Every method here is answered *from the index* on both sides, which is the
/// whole point — a comparison either pod could satisfy by forwarding to the
/// validator would be the validator agreeing with itself. `getaddressdeltas` is
/// the strongest of them for that reason: zebra cannot serve it at all, so an
/// answer can only have been synthesized from the pod's own index.
async fn compare_indexes(
    cx: &SyncCtx,
    direct: &ZainoIndexer,
    window: u32,
    heights: Vec<u32>,
) -> Verdict {
    let (rpc, direct) = match clients(cx, direct).await {
        Ok(pair) => pair,
        Err(v) => return v,
    };

    for height in heights {
        // The block itself, in the consensus serialization: byte-exact, and it
        // admits none of the numeric latitude a JSON comparison allows.
        for params in [
            json!([height.to_string(), 0]),
            json!([height.to_string(), 1]),
        ] {
            if let Some(v) = disagreed(&rpc, &direct, "getblock", params, height).await {
                return v;
            }
        }
        // The derived commitment-tree state at that height — the index-built
        // artifact most likely to drift between two ingest paths.
        let params = json!([height.to_string()]);
        if let Some(v) = disagreed(&rpc, &direct, "z_gettreestate", params, height).await {
            return v;
        }
    }

    // The address index over everything both pods have built. Zebra serves no
    // `getaddressdeltas`, so neither answer can be a forwarded one.
    let selector = json!([{ "addresses": [], "start": 0, "end": window }]);
    if let Some(v) = disagreed(&rpc, &direct, "getaddressdeltas", selector, window).await {
        return v;
    }
    Verdict::Satisfied
}

/// One method compared across both pods. `Some(verdict)` when it disagreed or
/// could not be asked; `None` when the two answered identically.
async fn disagreed(
    rpc: &JsonRpcClient,
    direct: &JsonRpcClient,
    method: &'static str,
    params: Value,
    height: u32,
) -> Option<Verdict> {
    let ask = |client: &JsonRpcClient, params: Value| {
        let client = client.clone();
        async move { client.call_value(method, params).await }
    };
    let (from_rpc, from_direct) =
        tokio::join!(ask(rpc, params.clone()), ask(direct, params.clone()));
    match (from_rpc, from_direct) {
        (Ok(a), Ok(b)) if a == b => None,
        (Ok(a), Ok(b)) => Some(violated(
            height,
            format!(
                "the two ingest paths built different indexes: {method}({params}) at {height}\n  \
                 rpc:    {a}\n  direct: {b}"
            ),
        )),
        // One answered and the other refused: still a disagreement about what
        // the index holds, and reporting it as a probe error would file a
        // finding about zaino as a fault in the harness.
        (Ok(a), Err(e)) => Some(violated(
            height,
            format!("{method}({params}) at {height}: rpc answered {a}, direct failed: {e}"),
        )),
        (Err(e), Ok(b)) => Some(violated(
            height,
            format!("{method}({params}) at {height}: direct answered {b}, rpc failed: {e}"),
        )),
        // Neither could answer. Two pods refusing the same call in the same way
        // is not a difference between them, so it is not this probe's finding.
        (Err(_), Err(_)) => None,
    }
}

/// JSON-RPC clients for the bound indexer and the captured one.
async fn clients(
    cx: &SyncCtx,
    direct: &ZainoIndexer,
) -> Result<(JsonRpcClient, JsonRpcClient), Verdict> {
    let Some(subject) = cx.indexer() else {
        return Err(Verdict::ProbeError("no indexer bound to this run".into()));
    };
    let rpc = subject
        .json_rpc()
        .await
        .map_err(|e| Verdict::ProbeError(format!("rpc indexer json_rpc: {e}")))?;
    let direct = direct
        .json_rpc()
        .await
        .map_err(|e| Verdict::ProbeError(format!("direct indexer json_rpc: {e}")))?;
    Ok((rpc, direct))
}

/// A named terminal/streamed violation.
fn violated(height: u32, detail: String) -> Verdict {
    Verdict::Violated(Violation {
        probe: String::new(),
        height: Some(height),
        detail,
    })
}

/// Sampling ladder = this profile's coverage policy (how much of two full-chain indexes a sweep
/// compares), pinned here rather than read off the arithmetic.
///
/// - Windows below span the current fixture (3,434,143) and a deeper rung: the ladder's
///   properties must hold wherever the profile is repointed
#[cfg(test)]
mod tests {
    use super::sweep_heights;

    /// A sweep may only ask about blocks both indexes have written.
    #[test]
    fn every_sample_lies_inside_the_common_window() {
        for window in [1, 2, 9, 1_000, 3_434_143, 4_140_000] {
            let heights = sweep_heights(window);
            assert!(
                heights.iter().all(|&h| h >= 1 && h <= window),
                "window {window} sampled outside 1..={window}: {heights:?}"
            );
            assert!(
                heights.windows(2).all(|w| w[0] < w[1]),
                "window {window} is not sorted and deduped: {heights:?}"
            );
        }
    }

    /// Both ends every time: the frontier, where the freshly written blocks are
    /// and where an ordering bug shows, and the bottom of the index, so a range
    /// corrupted early cannot stop being checked as the window grows past it.
    #[test]
    fn both_ends_of_the_window_are_always_sampled() {
        for window in [1, 2, 9, 1_000, 3_434_143, 4_140_000] {
            let heights = sweep_heights(window);
            assert!(heights.contains(&1), "window {window} skipped the base");
            assert!(
                heights.contains(&window),
                "window {window} skipped the frontier"
            );
        }
    }

    /// The ladder is geometric, so it stays a handful of samples even over
    /// millions of blocks — a sweep costs RPC round trips against two pods, and
    /// a linear stride here would make the profile unaffordable rather than
    /// thorough.
    #[test]
    fn the_ladder_stays_small_over_a_full_chain() {
        for window in [3_434_143, 4_140_000] {
            let heights = sweep_heights(window);
            assert!(
                heights.len() <= 12,
                "a full-chain sweep over {window} grew to {} samples: {heights:?}",
                heights.len()
            );
        }
    }

    /// Weighted to the frontier — over half the ladder in the newest tenth (uniform sampling
    /// would spend a sweep re-reading history every previous sweep already agreed about)
    #[test]
    fn most_samples_sit_near_the_frontier() {
        for window in [3_434_143u32, 4_140_000] {
            let heights = sweep_heights(window);
            let newest_tenth = window - window / 10;
            let near = heights.iter().filter(|&&h| h >= newest_tenth).count();
            assert!(
                near * 2 > heights.len(),
                "over {window}, only {near} of {} samples are in the newest tenth: {heights:?}",
                heights.len()
            );
        }
    }
}
