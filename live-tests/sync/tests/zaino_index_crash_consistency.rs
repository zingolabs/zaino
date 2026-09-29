//! Crash consistency: zaino SIGKILLed mid-build, three times, over a frozen mainnet chain.
//!
//! - Durable = fsynced before published → a kill may lose pre-commit, never a durable height
//! - Each restart resumes from its durable extent and replays; the files must stay valid
//!   (`zainod verify`) and the served answers must stay zebra's
//! - Frozen chain (peerless zebra at `ORCHARD_MAINNET`) → at rest the durable extent is exact:
//!   `pin - FINALISED_DEPTH` (bulk→tip transition writes its partial batch)
//! - Restarts beyond the scheduled kills = zaino crashed on its own
//! - Every served answer (headers, tx contents, trees, subtrees, transparent) held to zebra
//!
//! `ztest sync start zaino_index_crash_consistency`

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use ztest::backends::zainod::family;
use ztest::prelude::*;
use ztest::snapshots::ORCHARD_MAINNET;
use ztest::sync::{
    hours, mins, secs, Op, OpSet, Severity, Snapshot, SyncCtx, SyncOutcome, SyncRunner, Verdict,
    Violation,
};
use ztest::sync_ensure;

const TICK: Duration = secs(15);
/// UNMEASURED; 1.69M pre-sandblast blocks + three restarts and their replays
const RUN_CAP: Duration = hours(24);
/// Kills land well inside the build (first minutes = pod start + zebrad opening its state)
const KILLS: [Duration; 3] = [mins(10), mins(25), mins(45)];
/// Kubelet crash backoff doubles per kill (10 s → 20 s → 40 s) + replay of the pre-commit window
const STALL_WINDOW: Duration = mins(15);
const READY_WINDOW: Duration = mins(20);
const TERMINAL_WINDOW: Duration = mins(30);
const TERMINAL_POLL: Duration = secs(10);
/// Full walk of every index file (CRC + decode per record); UNMEASURED
const VERIFY_TIMEOUT: Duration = hours(1);
const SCRAPE: Duration = secs(10);

/// Set on zaino, not mirrored from its default
const FINALISED_DEPTH: u32 = 1_000;
/// Frozen chain → no motion; one block absorbs a scrape straddling a step
const ORDER_SLACK: u32 = 1;
/// UNMEASURED: index files for 1.69M blocks
const INDEX_DISK_GIB: u64 = 64;
/// p2pkh of hash160 `00…00` (never funded) = a cheap transparent request for the gate probe
const UNFUNDED_T_ADDR: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
const ZAINO: &str = "zaino";
const FETCHED_OPS: [Op; 5] =
    [Op::TransparentIn, Op::TransparentOut, Op::SaplingSpend, Op::SaplingOutput, Op::OrchardAction];

#[ztest::needs(ORCHARD_MAINNET)]
#[ztest::sync_test(
    name = "zaino_index_crash_consistency",
    description = "zaino SIGKILLed three times mid-build over a frozen mainnet chain; durable extents, files and served answers survive",
    subject = indexer,
    timeout = "24h",
    qos = sync,
    footprint = "6c/20Gi",
    tags = ["mainnet", "zaino", "index", "crash", "restart", "orchard", "nu5"],
)]
async fn zaino_index_crash_consistency(mut run: SyncRunner) -> SyncOutcome {
    let (zebra, zaino) = match run
        .topology(|t| {
            t.set_ready_timeout(READY_WINDOW);
            // TODO: git form (`git = …, rev = …`) once perf/lazy-point-decompression and its
            // librustzcash are pushed; local sibling until then
            let zebra = t.add_validator(
                dev!(
                    Validator::Zebrad,
                    "../../../zebra/docker/Dockerfile",
                    context = "../../../zebra",
                    version = "6.3.0"
                )
                .snapshot(ORCHARD_MAINNET)
                .resources(Cpu::cores(2), Mem::gib(4)),
            );
            let zaino = t.add_indexer(
                dev!(
                    Indexer::Zainod,
                    "../../Dockerfile",
                    context = "../..",
                    features = ["prometheus"]
                )
                .named(ZAINO)
                .restartable()
                .snapshot(ORCHARD_MAINNET)
                .finalised_depth(FINALISED_DEPTH)
                .disk(Disk::gib(INDEX_DISK_GIB))
                .resources(Cpu::cores(4), Mem::gib(16)),
            );
            (zebra, zaino)
        })
        .await
    {
        Ok(handles) => handles,
        Err(e) => return e.into(),
    };
    let pin = run.chain().tip_height;

    run.sync(zaino.clone());
    run.named("index").tick(TICK).timeout(RUN_CAP);
    run.requires_work(OpSet::of(&FETCHED_OPS));
    for (n, at) in KILLS.into_iter().enumerate() {
        run.nemesis().named(format!("kill-{n}")).at(at).kill(ZAINO);
    }

    let durable = Arc::new(ZainoIndex::ALL.map(|_| AtomicU32::new(0)));
    let refused_while_syncing = Arc::new(AtomicBool::new(false));

    {
        let (zaino, durable) = (zaino.clone(), durable.clone());
        run.always(Severity::Fatal)
            .named("durable_extents_survive_every_kill")
            .every(secs(30))
            .check_rpc(move |_s, _cx| {
                let (zaino, durable) = (zaino.clone(), durable.clone());
                Box::pin(async move { durable_extents_never_shrink(&zaino, &durable).await })
            });
    }
    {
        let zaino = zaino.clone();
        run.always(Severity::Fatal).named("watermarks_within_the_pin").every(secs(30)).check_rpc(
            move |_s, _cx| {
                let zaino = zaino.clone();
                Box::pin(async move { watermarks_within_the_pin(&zaino, pin).await })
            },
        );
    }
    {
        let (zaino, seen) = (zaino.clone(), refused_while_syncing.clone());
        run.always(Severity::Fatal).named("a_syncing_index_refuses").every(mins(1)).check_rpc(
            move |_s, _cx| {
                let (zaino, seen) = (zaino.clone(), seen.clone());
                Box::pin(async move { a_syncing_index_refuses(&zaino, &seen).await })
            },
        );
    }
    run.eventually(Severity::Fatal).named("index_advances").window(STALL_WINDOW).check(
        |s: &Snapshot| {
            if s.progressed_within(STALL_WINDOW) {
                Verdict::Satisfied
            } else {
                Verdict::Pending
            }
        },
    );

    run.sometimes().named("killed_mid_build").check(|s: &Snapshot| match s.target() {
        Some(target) if s.observed_restart() && s.height() < target => Verdict::Satisfied,
        _ => Verdict::Pending,
    });
    {
        let seen = refused_while_syncing.clone();
        run.sometimes().named("observed_the_gate_refuse").check(move |_s: &Snapshot| {
            if seen.load(Ordering::Relaxed) {
                Verdict::Satisfied
            } else {
                Verdict::Pending
            }
        });
    }

    run.at_completion(Severity::Fatal).named("no_unplanned_restart").check(|s: &Snapshot| {
        let (restarts, kills) = (s.restarts() as usize, KILLS.len());
        sync_ensure!(restarts <= kills, "{restarts} restarts, {kills} kills: crashed on its own");
        Verdict::Satisfied
    });
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Fatal).named("every_index_serves").check_rpc(move |s, _cx| {
            let zaino = zaino.clone();
            let at = s.height();
            Box::pin(async move { every_index_serves(&zaino, at).await })
        });
    }
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Fatal).named("at_rest_on_the_pin").check_rpc(move |_s, _cx| {
            let zaino = zaino.clone();
            Box::pin(async move { at_rest_on_the_pin(&zaino, pin).await })
        });
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        run.at_completion(Severity::Fatal).named("indexes_agree_with_zebra").check_rpc(
            move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { indexes_agree_with_zebra(&zaino, &zebra, pin).await })
            },
        );
    }
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Fatal).named("index_files_verify_clean").check_rpc(
            move |_s, cx| {
                let (zaino, cx) = (zaino.clone(), cx.clone());
                Box::pin(async move { index_files_verify_clean(&zaino, &cx).await })
            },
        );
    }

    run.run().await
}

// ── safety ────────────────────────────────────────────────────────────────────────────────────

/// Durable = fsynced before published → a SIGKILL can take pre-commit, never this
async fn durable_extents_never_shrink(
    zaino: &ZainoIndexer,
    seen: &[AtomicU32; ZainoIndex::ALL.len()],
) -> Verdict {
    for (index, last) in ZainoIndex::ALL.into_iter().zip(seen) {
        let now = match zaino.finalized_height(index).await {
            Ok(Some(h)) => h,
            // Unpublished = the restarted process has not scraped in yet, not a shrink
            Ok(None) => continue,
            Err(e) => return Verdict::ProbeError(format!("{index:?} finalized: {e}")),
        };
        let prev = last.fetch_max(now + 1, Ordering::Relaxed);
        let was = prev.saturating_sub(1);
        sync_ensure!(now + 1 >= prev, "{index:?} durable shrank {was} -> {now} across a restart");
    }
    Verdict::Satisfied
}

/// Frozen chain → nothing may name a height past the pin, and nothing durable inside the reorg
/// window
async fn watermarks_within_the_pin(zaino: &ZainoIndexer, pin: u32) -> Verdict {
    let scrape = match zaino.read(SCRAPE).await {
        Ok(s) => s,
        Err(e) => return Verdict::ProbeError(format!("zaino /metrics: {e}")),
    };
    if let Some(best) = scrape.level(family::BEST_TIP) {
        sync_ensure!(best as u32 == pin, "best tip {best} on a chain frozen at {pin}");
    }
    if let Some(fetched) = scrape.level(family::FETCH_HEIGHT) {
        sync_ensure!(fetched as u32 <= pin + ORDER_SLACK, "fetched {fetched} past pin {pin}");
    }
    let floor = pin - FINALISED_DEPTH;
    for index in ZainoIndex::ALL {
        if let Some(finalized) = scrape.level(family::index_finalized_height(index)) {
            sync_ensure!(finalized as u32 <= floor, "{index:?} durable {finalized} > {floor}");
        }
    }
    Verdict::Satisfied
}

/// synced = false ⇒ the index answers UNAVAILABLE (after a restart too: pre-commit replays first)
///
/// - Answered while unsynced → re-read the gate (may have opened mid-call) before judging
async fn a_syncing_index_refuses(zaino: &ZainoIndexer, seen: &AtomicBool) -> Verdict {
    for index in ZainoIndex::ALL {
        if zaino.synced(index).await.ok().flatten() != Some(false) {
            continue;
        }
        let refusal = match index {
            ZainoIndex::CompactBlock => zaino.latest_block_height().await.err(),
            // serves no RPC of its own (compact_block reads it)
            ZainoIndex::ValueBalance => continue,
            ZainoIndex::BlockHash => zaino.get_block_by_hash(BlockHash([0; 32])).await.err(),
            ZainoIndex::TreeState => zaino.get_latest_tree_state().await.err(),
            ZainoIndex::TransparentAddress => {
                zaino.get_taddress_balance(vec![UNFUNDED_T_ADDR.to_owned()]).await.err()
            }
        };
        match refusal {
            Some(e) if e.grpc_code() == Some(tonic::Code::Unavailable) => {
                seen.store(true, Ordering::Relaxed)
            }
            answered if zaino.synced(index).await.ok().flatten() == Some(false) => {
                let answered = answered.map_or("an answer".to_owned(), |e| e.to_string());
                return violated(
                    0,
                    format!("{index:?} unsynced answered {answered}, not UNAVAILABLE"),
                );
            }
            _ => {}
        }
    }
    Verdict::Satisfied
}

// ── terminal ──────────────────────────────────────────────────────────────────────────────────

async fn every_index_serves(zaino: &ZainoIndexer, at: u32) -> Verdict {
    let deadline = Instant::now() + TERMINAL_WINDOW;
    loop {
        let mut unsynced = Vec::new();
        for index in ZainoIndex::ALL {
            match zaino.synced(index).await {
                Ok(Some(true)) => {}
                Ok(other) => unsynced.push((index, other)),
                Err(e) => return Verdict::ProbeError(format!("{index:?} synced: {e}")),
            }
        }
        if unsynced.is_empty() {
            return Verdict::Satisfied;
        }
        if Instant::now() >= deadline {
            return violated(
                at,
                format!("{TERMINAL_WINDOW:?} after completion, not serving: {unsynced:?}"),
            );
        }
        tokio::time::sleep(TERMINAL_POLL).await;
    }
}

/// Served tip = the pin; every durable extent = exactly `pin - FINALISED_DEPTH`
///
/// - Lower = a partial batch left unwritten at the bulk→tip transition (the pre-fix follower)
async fn at_rest_on_the_pin(zaino: &ZainoIndexer, pin: u32) -> Verdict {
    match zaino.latest_block_height().await {
        Ok(served) => {
            sync_ensure!(u32::from(served) == pin, "serves {}, pin {pin}", u32::from(served))
        }
        Err(e) => return Verdict::ProbeError(format!("zaino GetLatestBlock: {e}")),
    }
    let floor = pin - FINALISED_DEPTH;
    for index in ZainoIndex::ALL {
        match zaino.finalized_height(index).await {
            Ok(Some(h)) => {
                sync_ensure!(h == floor, "{index:?} durable {h}, expected {floor}")
            }
            Ok(None) => {
                return violated(pin, format!("{index:?} never published a durable extent"))
            }
            Err(e) => return Verdict::ProbeError(format!("{index:?} finalized: {e}")),
        }
    }
    Verdict::Satisfied
}

/// Every served index vs zebra over one ladder (frozen chain → the pin is the top)
async fn indexes_agree_with_zebra(
    zaino: &ZainoIndexer,
    zebra: &ZebraValidator,
    pin: u32,
) -> Verdict {
    let heights = match ladder(zebra, pin).await {
        Ok(h) => h,
        Err(v) => return v,
    };
    for verdict in [
        lightd_info_agrees(zaino, zebra).await,
        tree_states_agree(zaino, zebra, &heights).await,
        subtree_roots_agree(zaino, zebra).await,
        blocks_agree(zaino, zebra, &heights).await,
    ] {
        if !matches!(verdict, Verdict::Satisfied) {
            return verdict;
        }
    }
    match coinbase_addresses(zebra, &heights).await {
        Ok(addresses) => transparent_agrees(zaino, zebra, &addresses).await,
        Err(v) => v,
    }
}

/// Durable extents read first → the walk (files only grow) must reach at least them
async fn index_files_verify_clean(zaino: &ZainoIndexer, cx: &SyncCtx) -> Verdict {
    let Some(pod) = cx.indexer_pod() else {
        return Verdict::ProbeError("no indexer pod bound".into());
    };
    let mut floors = Vec::new();
    for index in ZainoIndex::ALL {
        match zaino.finalized_height(index).await {
            Ok(Some(h)) => floors.push((index, h)),
            Ok(None) => return violated(0, format!("{index:?} never published a durable extent")),
            Err(e) => return Verdict::ProbeError(format!("{index:?} finalized: {e}")),
        }
    }
    let output = match pod.exec(&["zainod", "verify"], VERIFY_TIMEOUT).await {
        Ok(o) => o,
        Err(e) => return Verdict::ProbeError(format!("exec zainod verify: {e}")),
    };
    let report: Value = match serde_json::from_str(&output.stdout) {
        Ok(r) => r,
        Err(e) => {
            return Verdict::ProbeError(format!(
                "zainod verify (exit {:?}) printed no JSON report: {e}\nstderr: {}",
                output.status, output.stderr
            ))
        }
    };
    if !output.success() || report["clean"] != json!(true) {
        return violated(
            0,
            format!("zainod verify: violations\n{report:#}\nstderr: {}", output.stderr),
        );
    }
    for (index, floor) in floors {
        let last = report[index.label()]["span"]["last_height"].as_u64();
        if last.is_none_or(|last| last < u64::from(floor)) {
            return violated(
                floor,
                format!("verify walked {index:?} to {last:?}, below durable {floor}"),
            );
        }
    }
    Verdict::Satisfied
}

// ── zebra oracles ─────────────────────────────────────────────────────────────────────────────

/// Every activation ±1 (zebra's own schedule, never compiled in), both ends, and a geometric
/// ladder back from `top`
async fn ladder(zebra: &ZebraValidator, top: u32) -> Result<Vec<u32>, Verdict> {
    let info = rpc(zebra, "getblockchaininfo", json!([])).await?;
    let activations = info["upgrades"]
        .as_object()
        .ok_or_else(|| Verdict::ProbeError(format!("getblockchaininfo has no upgrades: {info}")))?
        .values()
        .filter_map(|u| u["activationheight"].as_u64())
        .map(|h| h as u32);

    let mut heights = vec![1, top];
    for at in activations.filter(|h| *h > 1) {
        heights.extend([at - 1, at, at + 1]);
    }
    let mut back = 1u32;
    while back < top {
        heights.push(top - back);
        back = back.saturating_mul(4);
    }
    heights.retain(|h| *h >= 1 && *h <= top);
    heights.sort_unstable();
    heights.dedup();
    Ok(heights)
}

async fn lightd_info_agrees(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    let (info, served, truth) = match (
        zaino.indexer_info().await,
        zaino.latest_block_height().await,
        rpc(zebra, "getblockchaininfo", json!([])).await,
    ) {
        (Ok(i), Ok(s), Ok(t)) => (i, u64::from(u32::from(s)), t),
        (Err(e), ..) => return Verdict::ProbeError(format!("zaino GetLightdInfo: {e}")),
        (_, Err(e), _) => return Verdict::ProbeError(format!("zaino GetLatestBlock: {e}")),
        (.., Err(v)) => return v,
    };
    let sapling = truth["upgrades"]
        .as_object()
        .and_then(|u| u.values().find(|u| u["name"].as_str() == Some("Sapling")))
        .and_then(|u| u["activationheight"].as_u64())
        .unwrap_or(0);
    let pairs = [
        ("chainName", info.chain_name.clone(), truth["chain"].as_str().unwrap_or("").to_owned()),
        (
            "saplingActivationHeight",
            info.sapling_activation_height.to_string(),
            sapling.to_string(),
        ),
        (
            "consensusBranchId",
            info.consensus_branch_id.clone(),
            truth["consensus"]["chaintip"].as_str().unwrap_or("").to_owned(),
        ),
        ("blockHeight", info.block_height.to_string(), served.to_string()),
    ];
    match pairs.into_iter().find(|(_, a, b)| a != b) {
        Some((field, zaino, truth)) => violated(
            served as u32,
            format!("LightdInfo.{field}\n  zaino: {zaino}\n  truth: {truth}"),
        ),
        None => Verdict::Satisfied,
    }
}

/// `GetTreeState` byte-identical to `z_gettreestate`: hash, time, all three legacy trees
///
/// - `""` → `000000` (empty legacy tree) both sides: pre-activation spellings differ, trees don't
async fn tree_states_agree(
    zaino: &ZainoIndexer,
    zebra: &ZebraValidator,
    heights: &[u32],
) -> Verdict {
    let tree = |s: &str| {
        if s.is_empty() {
            "000000".to_owned()
        } else {
            s.to_owned()
        }
    };
    for &height in heights {
        let served = match zaino.get_tree_state(BlockHeight::from_u32(height)).await {
            Ok(t) => t,
            Err(e) => return violated(height, format!("zaino GetTreeState({height}): {e}")),
        };
        let truth = match rpc(zebra, "z_gettreestate", json!([height.to_string()])).await {
            Ok(v) => v,
            Err(v) => return v,
        };
        let final_state =
            |pool: &str| tree(truth[pool]["commitments"]["finalState"].as_str().unwrap_or(""));
        let pairs = [
            ("hash", served.hash.clone(), truth["hash"].as_str().unwrap_or("").to_owned()),
            ("time", served.time.to_string(), truth["time"].to_string()),
            ("sapling", tree(&served.sapling_tree), final_state("sapling")),
            ("orchard", tree(&served.orchard_tree), final_state("orchard")),
            ("ironwood", tree(&served.ironwood_tree), final_state("ironwood")),
        ];
        if let Some((field, zaino, zebra)) = pairs.into_iter().find(|(_, a, b)| a != b) {
            return violated(
                height,
                format!("tree {field} at {height}\n  zaino: {zaino}\n  zebra: {zebra}"),
            );
        }
    }
    Verdict::Satisfied
}

/// Whole `GetSubtreeRoots` list per pool == `z_getsubtreesbyindex` (root + completing height)
///
/// - One side answering while the other refuses = violation; both refusing = no disagreement
async fn subtree_roots_agree(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    let client = match zebra.json_rpc().await {
        Ok(c) => c,
        Err(e) => return Verdict::ProbeError(format!("zebra json_rpc: {e}")),
    };
    for (protocol, pool) in [
        (ShieldedProtocol::Sapling, "sapling"),
        (ShieldedProtocol::Orchard, "orchard"),
        (ShieldedProtocol::Ironwood, "ironwood"),
    ] {
        let (served, truth) = match (
            zaino.get_subtree_roots(0, protocol, 0).await,
            client.call_value("z_getsubtreesbyindex", json!([pool, 0])).await,
        ) {
            (Ok(s), Ok(t)) => (s, t),
            (Err(_), Err(_)) => continue,
            (Ok(s), Err(e)) => {
                return violated(
                    0,
                    format!("{pool} subtrees: zaino has {}, zebra refused: {e}", s.len()),
                )
            }
            (Err(e), Ok(t)) => {
                return violated(0, format!("{pool} subtrees: zebra has {t}, zaino refused: {e}"))
            }
        };
        let truth: Vec<(String, u64)> = truth["subtrees"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                (s["root"].as_str().unwrap_or("").to_owned(), s["end_height"].as_u64().unwrap_or(0))
            })
            .collect();
        let served: Vec<(String, u64)> =
            served.iter().map(|s| (plain_hex(&s.root_hash), s.completing_block_height)).collect();
        let (ours, theirs) = (served.len(), truth.len());
        sync_ensure!(ours == theirs, "{pool} subtrees: zaino {ours}, zebra {theirs}");
        if let Some((index, (s, t))) =
            served.iter().zip(&truth).enumerate().find(|(_, (s, t))| s != t)
        {
            return violated(
                t.1 as u32,
                format!("{pool} subtree {index}\n  zaino: {s:?}\n  zebra: {t:?}"),
            );
        }
    }
    Verdict::Satisfied
}

/// One transaction as both sides can spell it (counts per shielded pool: byte orders differ)
#[derive(Debug, PartialEq)]
struct TxShape {
    index: u64,
    txid: String,
    prevouts: Vec<(String, u32)>,
    output_values: Vec<u64>,
    sapling_spends: usize,
    sapling_outputs: usize,
    orchard_actions: usize,
    ironwood_actions: usize,
}

/// `GetBlock` (all pools) vs `getblock(h, 2)`: header link, then every tx in order
///
/// - Coinbase `vin` = omitted on the compact side (lightwalletd), so dropped from zebra's too
async fn blocks_agree(zaino: &ZainoIndexer, zebra: &ZebraValidator, heights: &[u32]) -> Verdict {
    for &height in heights {
        let served = match zaino.get_block(BlockHeight::from_u32(height)).await {
            Ok(b) => b,
            Err(e) => return violated(height, format!("zaino GetBlock({height}): {e}")),
        };
        let truth = match rpc(zebra, "getblock", json!([height.to_string(), 2])).await {
            Ok(v) => v,
            Err(v) => return v,
        };
        let header = [
            ("hash", display_hex(&served.hash), truth["hash"].as_str().unwrap_or("").to_owned()),
            (
                "prev_hash",
                display_hex(&served.prev_hash),
                truth["previousblockhash"].as_str().unwrap_or("").to_owned(),
            ),
            ("time", served.time.to_string(), truth["time"].to_string()),
        ];
        if let Some((field, zaino, zebra)) = header.into_iter().find(|(_, a, b)| a != b) {
            return violated(
                height,
                format!("block {field} at {height}\n  zaino: {zaino}\n  zebra: {zebra}"),
            );
        }

        let served: Vec<TxShape> = served
            .vtx
            .iter()
            .map(|tx| TxShape {
                index: tx.index,
                txid: display_hex(&tx.txid),
                prevouts: tx
                    .vin
                    .iter()
                    .map(|i| (display_hex(&i.prevout_txid), i.prevout_index))
                    .collect(),
                output_values: tx.vout.iter().map(|o| o.value).collect(),
                sapling_spends: tx.spends.len(),
                sapling_outputs: tx.outputs.len(),
                orchard_actions: tx.actions.len(),
                ironwood_actions: tx.ironwood_actions.len(),
            })
            .collect();
        let count = |v: &Value| v.as_array().map_or(0, Vec::len);
        let truth: Vec<TxShape> = truth["tx"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, tx)| TxShape {
                index: index as u64,
                txid: tx["txid"].as_str().unwrap_or("").to_owned(),
                prevouts: tx["vin"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|i| i.get("coinbase").is_none())
                    .map(|i| {
                        let index = i["vout"].as_u64().map_or(u32::MAX, |v| v as u32);
                        (i["txid"].as_str().unwrap_or("").to_owned(), index)
                    })
                    .collect(),
                output_values: tx["vout"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|o| o["valueZat"].as_u64().unwrap_or(u64::MAX))
                    .collect(),
                sapling_spends: count(&tx["vShieldedSpend"]),
                sapling_outputs: count(&tx["vShieldedOutput"]),
                orchard_actions: count(&tx["orchard"]["actions"]),
                ironwood_actions: count(&tx["ironwood"]["actions"]),
            })
            .collect();
        let (ours, theirs) = (served.len(), truth.len());
        sync_ensure!(ours == theirs, "block {height}: zaino {ours} txs, zebra {theirs}");
        if let Some((s, t)) = served.iter().zip(&truth).find(|(s, t)| s != t) {
            return violated(
                height,
                format!("block {height} tx {}\n  zaino: {s:?}\n  zebra: {t:?}", t.index),
            );
        }
    }
    Verdict::Satisfied
}

/// Coinbase recipients of the ladder blocks (real, funded addresses spanning every era)
async fn coinbase_addresses(
    zebra: &ZebraValidator,
    heights: &[u32],
) -> Result<Vec<String>, Verdict> {
    let mut addresses = Vec::new();
    for &height in heights {
        let block = rpc(zebra, "getblock", json!([height.to_string(), 2])).await?;
        addresses.extend(block["tx"][0]["vout"].as_array().into_iter().flatten().filter_map(|o| {
            let key = &o["scriptPubKey"];
            key["addresses"][0].as_str().or(key["address"].as_str()).map(str::to_owned)
        }));
    }
    addresses.sort_unstable();
    addresses.dedup();
    Ok(addresses)
}

/// Balance + whole UTXO set per address == `getaddressbalance` / `getaddressutxos` (frozen
/// chain → no still-window needed)
async fn transparent_agrees(
    zaino: &ZainoIndexer,
    zebra: &ZebraValidator,
    addresses: &[String],
) -> Verdict {
    if addresses.is_empty() {
        return Verdict::ProbeError("no coinbase addresses to compare".into());
    }
    for address in addresses {
        let selector = json!([{ "addresses": [address] }]);
        let (served_balance, truth_balance, served_utxos, truth_utxos) = match (
            zaino.get_taddress_balance(vec![address.clone()]).await,
            rpc(zebra, "getaddressbalance", selector.clone()).await,
            zaino.get_address_utxos(vec![address.clone()], BlockHeight::from_u32(0), 0).await,
            rpc(zebra, "getaddressutxos", selector).await,
        ) {
            (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
            (Err(e), ..) | (_, _, Err(e), _) => {
                return violated(0, format!("zaino refused {address}: {e}"))
            }
            (_, Err(v), ..) | (.., Err(v)) => return v,
        };
        let mut served: Vec<(String, u64, i64, u64)> = served_utxos
            .iter()
            .map(|u| (display_hex(&u.txid), u.index as u64, u.value_zat, u.height))
            .collect();
        let mut truth: Vec<(String, u64, i64, u64)> = truth_utxos
            .as_array()
            .into_iter()
            .flatten()
            .map(|u| {
                (
                    u["txid"].as_str().unwrap_or("").to_owned(),
                    u["outputIndex"].as_u64().unwrap_or(u64::MAX),
                    u["satoshis"].as_i64().unwrap_or(-1),
                    u["height"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        served.sort_unstable();
        truth.sort_unstable();
        let (served_zat, truth_zat) =
            (i64::from(served_balance), truth_balance["balance"].as_i64().unwrap_or(-1));
        if served_zat != truth_zat || served != truth {
            let differing = served
                .iter()
                .zip(&truth)
                .find(|(s, t)| s != t)
                .map(|(s, t)| format!("; first differing utxo zaino {s:?} zebra {t:?}"))
                .unwrap_or_default();
            return violated(
                0,
                format!(
                    "{address}: balance zaino {served_zat} zebra {truth_zat}; utxos zaino {} zebra {}{differing}",
                    served.len(),
                    truth.len()
                ),
            );
        }
    }
    Verdict::Satisfied
}

// ── plumbing ──────────────────────────────────────────────────────────────────────────────────

fn violated(height: u32, detail: String) -> Verdict {
    Verdict::Violated(Violation { probe: String::new(), height: Some(height), detail })
}

async fn rpc(
    zebra: &ZebraValidator,
    method: &'static str,
    params: Value,
) -> Result<Value, Verdict> {
    let client =
        zebra.json_rpc().await.map_err(|e| Verdict::ProbeError(format!("zebra json_rpc: {e}")))?;
    client
        .call_value(method, params)
        .await
        .map_err(|e| Verdict::ProbeError(format!("zebra {method}: {e}")))
}

/// Protocol-order bytes → display hex (zebra's JSON spelling of hashes and txids)
fn display_hex(bytes: &[u8]) -> String {
    bytes.iter().rev().map(|b| format!("{b:02x}")).collect()
}

fn plain_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
