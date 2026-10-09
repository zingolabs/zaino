//! Crash consistency: zaino SIGKILLed mid-build, three times, over a frozen mainnet chain.
//!
//! - Durable = fsynced before published → a kill may lose the non-finalized state, never a
//!   durable height
//! - Each restart resumes from its durable extent and replays; the files must stay valid
//!   (`zainod verify`) and the served answers must stay zebra's (every 5 s + at rest)
//! - Frozen chain (peerless zebra at `ORCHARD_MAINNET`) → at rest the durable extent is exact:
//!   `pin - FINALISED_DEPTH` (bulk→tip transition writes its partial batch)
//! - Restarts beyond the scheduled kills = zaino crashed on its own
//!
//! `ztest sync start zaino_index_crash_consistency`

use std::cell::Cell;
use std::time::Duration;

use anyhow::Context as _;
use serde_json::{json, Value};
use ztest::backends::zainod::family;
use ztest::loadtest::reference::{block_diff, tree_state_diff, Fees, Zebra};
use ztest::prelude::*;
use ztest::snapshots::ORCHARD_MAINNET;
use ztest::sync::Severity::Fatal;
use ztest::sync::{hours, mins, secs, Op, OpSet, SyncOutcome, SyncRunner};
use ztest::{sync_ensure, sync_fail};

// ── topology ──────────────────────────────────────────────────────────────────────────────────

const ZAINO: &str = "zaino";
/// Set on zaino, not mirrored from its default
const FINALISED_DEPTH: u32 = 1_000;
/// UNMEASURED: index files for 1.69M blocks
const INDEX_DISK_GIB: u64 = 64;

// ── phase windows ─────────────────────────────────────────────────────────────────────────────

const TICK: Duration = secs(15);
/// UNMEASURED; 1.69M pre-sandblast blocks + three restarts and their replays
const RUN_CAP: Duration = hours(24);
/// Kills land well inside the build (first minutes = pod start + zebrad opening its state)
const KILLS: [Duration; 3] = [mins(10), mins(25), mins(45)];
const READY_WINDOW: Duration = mins(20);
/// Kubelet crash backoff doubles per kill (10 s → 20 s → 40 s) + replay of the non-finalized state
const STALL_WINDOW: Duration = mins(15);
const TERMINAL_WINDOW: Duration = mins(30);
/// Full walk of every index file (CRC + decode per record); UNMEASURED
const VERIFY_TIMEOUT: Duration = hours(1);
const SCRAPE: Duration = secs(10);

/// Frozen chain → no motion; one block absorbs a scrape straddling a step
const ORDER_SLACK: u32 = 1;

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
async fn zaino_index_crash_consistency(run: SyncRunner) -> SyncOutcome {
    let topology = run.topology(|t| {
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
            dev!(Indexer::Zainod, "../../Dockerfile", context = "../..")
                .named(ZAINO)
                .restartable()
                .snapshot(ORCHARD_MAINNET)
                .finalised_depth(FINALISED_DEPTH)
                .disk(Disk::gib(INDEX_DISK_GIB))
                .resources(Cpu::cores(4), Mem::gib(16)),
        );
        (zebra, zaino)
    });
    let mut run = match topology.await {
        Ok(run) => run,
        Err(e) => return e.into(),
    };
    let pin = run.chain().tip_height;
    for (n, at) in KILLS.into_iter().enumerate() {
        run.nemesis().named(format!("kill-{n}")).at(at).kill(ZAINO);
    }

    let index = run.phase("index", async |(_, zaino)| Ok(zaino.clone()));
    index.tick(TICK).timeout(RUN_CAP).requires_work(OpSet::of(&FETCHED_OPS));

    // ── across every kill ─────────────────────────────────────────────────────────────────────

    let durable = Cell::new([None::<u32>; ZainoIndex::ALL.len()]);
    index.always("durable_extents_survive_every_kill", Fatal).every(secs(30)).check(
        async move |_, (_, zaino)| {
            let mut seen = durable.get();
            for (ix, last) in ZainoIndex::ALL.into_iter().zip(&mut seen) {
                // Unpublished = the restarted process has not scraped in yet, not a shrink
                let Some(now) = zaino.finalized_height(ix).await? else { continue };
                sync_ensure!(last.is_none_or(|was| now >= was), "{ix:?} shrank {last:?} -> {now}");
                *last = Some(now);
            }
            durable.set(seen);
            Ok(())
        },
    );
    // Frozen chain → nothing names a height past the pin, nothing durable inside the reorg window
    index.always("watermarks_within_the_pin", Fatal).every(secs(30)).check(
        async move |_, (_, zaino)| {
            let scrape = zaino.read(SCRAPE).await?;
            if let Some(best) = scrape.level(family::BEST_TIP) {
                sync_ensure!(best as u32 == pin, "best tip {best} on a chain frozen at {pin}");
            }
            if let Some(fetched) = scrape.level(family::FETCH_HEIGHT) {
                let fetched = fetched as u32;
                sync_ensure!(fetched <= pin + ORDER_SLACK, "fetched {fetched} past pin {pin}");
            }
            let floor = pin - FINALISED_DEPTH;
            for ix in ZainoIndex::ALL {
                if let Some(durable) = scrape.level(family::index_finalized_height(ix)) {
                    sync_ensure!(durable as u32 <= floor, "{ix:?} durable {durable} > {floor}");
                }
            }
            Ok(())
        },
    );
    // Durable = final → answered mid-build too: a torn write across a kill reads back here
    index.always("committed_tree_states_are_zebras", Fatal).every(secs(5)).check(
        async |_, (zebra, zaino)| {
            let Some(at) = zaino.finalized_height(ZainoIndex::TreeState).await? else {
                return Ok(());
            };
            let served = match zaino.get_tree_state(BlockHeight::from_u32(at)).await {
                Ok(served) => served,
                // nothing served yet (an index uncommitted): retry, not a wrong answer
                Err(e) if e.grpc_code() == Some(tonic::Code::Unavailable) => return Ok(()),
                Err(e) => sync_fail!(at = at, "zaino GetTreeState: {e}"),
            };
            let truth = Zebra::new(zebra.json_rpc().await?).tree_state(at).await?;
            if let Some(diff) = tree_state_diff(&truth, &served) {
                sync_fail!(at = at, "tree state: {diff}");
            }
            Ok(())
        },
    );
    index.always("committed_blocks_are_zebras", Fatal).every(secs(5)).check(
        async |_, (zebra, zaino)| {
            let Some(at) = zaino.finalized_height(ZainoIndex::CompactBlock).await? else {
                return Ok(());
            };
            let served = match zaino.get_block(BlockHeight::from_u32(at)).await {
                Ok(served) => served,
                // nothing served yet (an index uncommitted): retry, not a wrong answer
                Err(e) if e.grpc_code() == Some(tonic::Code::Unavailable) => return Ok(()),
                Err(e) => sync_fail!(at = at, "zaino GetBlock: {e}"),
            };
            let reference = Zebra::new(zebra.json_rpc().await?);
            if let Some(diff) =
                block_diff(&reference.compact_block(at, Fees::Checked).await?, &served)
            {
                sync_fail!(at = at, "block: {diff}");
            }
            Ok(())
        },
    );
    index
        .eventually("index_advances", Fatal)
        .window(STALL_WINDOW)
        .check(async |s, _| Ok(s.progressed_within(STALL_WINDOW)));
    index.sometimes("killed_mid_build").check(async |s, _| {
        Ok(s.observed_restart() && s.target().is_some_and(|target| s.height() < target))
    });

    // ── at rest on the pin ────────────────────────────────────────────────────────────────────

    index.at_completion("no_unplanned_restart", Fatal).check(async |s, _| {
        let (restarts, kills) = (s.restarts() as usize, KILLS.len());
        sync_ensure!(restarts <= kills, "{restarts} restarts, {kills} kills: crashed on its own");
        Ok(())
    });
    index.at_completion("every_index_serves", Fatal).within(TERMINAL_WINDOW).check(
        async |_, (_, zaino)| {
            for ix in ZainoIndex::ALL {
                let synced = zaino.synced(ix).await?;
                sync_ensure!(synced == Some(true), "{ix:?} not serving: synced = {synced:?}");
            }
            Ok(())
        },
    );
    // Lower than `pin - FINALISED_DEPTH` = a partial batch left unwritten at the bulk→tip transition
    index.at_completion("at_rest_on_the_pin", Fatal).check(async move |_, (_, zaino)| {
        let served = u32::from(zaino.latest_block_height().await?);
        let mut durable = Vec::new();
        for ix in ZainoIndex::ALL {
            durable.push((ix, zaino.finalized_height(ix).await?));
        }
        let floor = Some(pin - FINALISED_DEPTH);
        let want: Vec<_> = ZainoIndex::ALL.into_iter().map(|ix| (ix, floor)).collect();
        sync_ensure!(served == pin, "serves {served}, pin {pin}");
        sync_ensure!(durable == want, "durable extents\n  got:  {durable:?}\n  want: {want:?}");
        Ok(())
    });
    // Chain from zaino's config, height = its own served tip; activation + branch from zebra
    index.at_completion("lightd_info_is_zebras", Fatal).check(async move |_, (zebra, zaino)| {
        let info = zaino.indexer_info().await?;
        let truth = zebra.json_rpc().await?.call_value("getblockchaininfo", json!([])).await?;
        let sapling = truth["upgrades"]
            .as_object()
            .and_then(|upgrades| upgrades.values().find(|upgrade| upgrade["name"] == "Sapling"))
            .and_then(|sapling| sapling["activationheight"].as_u64())
            .context("getblockchaininfo: no Sapling activation")?;
        let from_zebra = (
            truth["chain"].as_str().context("getblockchaininfo: no chain")?,
            sapling,
            truth["consensus"]["chaintip"].as_str().context("getblockchaininfo: no branch")?,
            u64::from(pin),
        );
        let from_zaino = (
            info.chain_name.as_str(),
            info.sapling_activation_height,
            info.consensus_branch_id.as_str(),
            info.block_height,
        );
        sync_ensure!(
            from_zaino == from_zebra,
            "(chain, sapling activation, branch, height)\n  zaino: {from_zaino:?}\n  zebra: \
             {from_zebra:?}"
        );
        Ok(())
    });
    // Whole list per pool; one side answering while the other refuses = disagreement
    index.at_completion("subtree_roots_are_zebras", Fatal).check(async |_, (zebra, zaino)| {
        let reference = Zebra::new(zebra.json_rpc().await?);
        for (protocol, pool) in [
            (ShieldedProtocol::Sapling, "sapling"),
            (ShieldedProtocol::Orchard, "orchard"),
            (ShieldedProtocol::Ironwood, "ironwood"),
        ] {
            let (served, truth) = match (
                zaino.get_subtree_roots(0, protocol, 0).await,
                reference.subtree_roots(pool).await,
            ) {
                (Ok(served), Ok(truth)) => (served, truth),
                (Err(_), Err(_)) => continue,
                (Ok(s), Err(e)) => sync_fail!("{pool}: zaino has {}, zebra refused: {e}", s.len()),
                (Err(e), Ok(t)) => sync_fail!("{pool}: zebra has {}, zaino refused: {e}", t.len()),
            };
            if let Some((i, (s, t))) =
                served.iter().zip(&truth).enumerate().find(|(_, (s, t))| s != t)
            {
                let at = t.completing_block_height as u32;
                sync_fail!(at = at, "{pool} subtree {i}\n  zaino: {s:?}\n  zebra: {t:?}");
            }
            let (ours, theirs) = (served.len(), truth.len());
            sync_ensure!(ours == theirs, "{pool} subtrees: zaino {ours}, zebra {theirs}");
        }
        Ok(())
    });
    // Ladder = every activation ±1 (zebra's own schedule), both ends, geometric back from the pin
    index.at_completion("ladder_blocks_and_trees_are_zebras", Fatal).check(
        async move |_, (zebra, zaino)| {
            let client = zebra.json_rpc().await?;
            let reference = Zebra::new(client.clone());
            let info = client.call_value("getblockchaininfo", json!([])).await?;
            let upgrades =
                info["upgrades"].as_object().context("getblockchaininfo: no upgrades")?;
            let mut ladder = vec![1, pin];
            for at in upgrades.values().filter_map(|upgrade| upgrade["activationheight"].as_u64()) {
                let at = at as u32;
                ladder.extend([at.saturating_sub(1), at, at + 1]);
            }
            let mut back = 1;
            while back < pin {
                ladder.push(pin - back);
                back = back.saturating_mul(4);
            }
            ladder.retain(|height| (1..=pin).contains(height));
            ladder.sort_unstable();
            ladder.dedup();

            for at in ladder {
                let height = BlockHeight::from_u32(at);
                let served = match zaino.get_tree_state(height).await {
                    Ok(served) => served,
                    Err(e) => sync_fail!(at = at, "zaino GetTreeState: {e}"),
                };
                if let Some(diff) = tree_state_diff(&reference.tree_state(at).await?, &served) {
                    sync_fail!(at = at, "tree state: {diff}");
                }
                let served = match zaino.get_block(height).await {
                    Ok(served) => served,
                    Err(e) => sync_fail!(at = at, "zaino GetBlock: {e}"),
                };
                let truth = reference.compact_block(at, Fees::Checked).await?;
                if let Some(diff) = block_diff(&truth, &served) {
                    sync_fail!(at = at, "block: {diff}");
                }
            }
            Ok(())
        },
    );
    // Coinbase recipients at every activation + the pin = funded addresses spanning every era
    // - Frozen chain → compared directly (no still window needed)
    index.at_completion("coinbase_balances_are_zebras", Fatal).check(
        async move |_, (zebra, zaino)| {
            let client = zebra.json_rpc().await?;
            let info = client.call_value("getblockchaininfo", json!([])).await?;
            let upgrades =
                info["upgrades"].as_object().context("getblockchaininfo: no upgrades")?;
            let activations = upgrades
                .values()
                .filter(|upgrade| upgrade["status"] == "active")
                .filter_map(|upgrade| upgrade["activationheight"].as_u64());
            let mut addresses = Vec::new();
            for at in activations.chain([u64::from(pin)]) {
                let block = client.call_value("getblock", json!([at.to_string(), 2])).await?;
                let coinbase =
                    block["tx"][0]["vout"].as_array().context("getblock: no coinbase")?;
                addresses.extend(coinbase.iter().filter_map(|output| {
                    let key = &output["scriptPubKey"];
                    key["addresses"][0].as_str().or(key["address"].as_str()).map(str::to_owned)
                }));
            }
            addresses.sort_unstable();
            addresses.dedup();
            anyhow::ensure!(!addresses.is_empty(), "no transparent coinbase recipient found");

            for address in &addresses {
                let balance = match zaino.get_taddress_balance(vec![address.clone()]).await {
                    Ok(balance) => i64::from(balance),
                    Err(e) => sync_fail!("zaino GetTaddressBalance({address}): {e}"),
                };
                let from = BlockHeight::from_u32(0);
                let utxos = match zaino.get_address_utxos(vec![address.clone()], from, 0).await {
                    Ok(utxos) => utxos,
                    Err(e) => sync_fail!("zaino GetAddressUtxos({address}): {e}"),
                };
                let mut utxos: Vec<(String, u64, i64, u64)> = utxos
                    .iter()
                    .map(|u| {
                        // protocol order → zebra's display hex
                        let txid = u.txid.iter().rev().map(|b| format!("{b:02x}")).collect();
                        (txid, u.index as u64, u.value_zat, u.height)
                    })
                    .collect();
                utxos.sort_unstable();
                let from_zaino = (balance, utxos);

                let selector = json!([{ "addresses": [address] }]);
                let balance = client.call_value("getaddressbalance", selector.clone()).await?;
                let balance = balance["balance"].as_i64().context("getaddressbalance")?;
                let utxos = client.call_value("getaddressutxos", selector).await?;
                let mut utxos = utxos
                    .as_array()
                    .context("getaddressutxos: no list")?
                    .iter()
                    .map(|u| {
                        anyhow::Ok((
                            u["txid"].as_str().context("utxo: no txid")?.to_owned(),
                            u["outputIndex"].as_u64().context("utxo: no outputIndex")?,
                            u["satoshis"].as_i64().context("utxo: no satoshis")?,
                            u["height"].as_u64().context("utxo: no height")?,
                        ))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                utxos.sort_unstable();
                let from_zebra = (balance, utxos);

                let utxo = from_zaino.1.iter().zip(&from_zebra.1).find(|(s, t)| s != t);
                sync_ensure!(
                    from_zaino == from_zebra,
                    "{address}: balance zaino {} zebra {}; utxos zaino {} zebra {}; first \
                     differing (zaino, zebra) {utxo:?}",
                    from_zaino.0,
                    from_zebra.0,
                    from_zaino.1.len(),
                    from_zebra.1.len()
                );
            }
            Ok(())
        },
    );
    // Durable extents read first → the walk (files only grow) must reach at least them
    index.at_completion("index_files_verify_clean", Fatal).check(async |_, (_, zaino)| {
        let mut floors = Vec::new();
        for ix in ZainoIndex::ALL {
            let Some(floor) = zaino.finalized_height(ix).await? else {
                sync_fail!("{ix:?} never published a durable extent");
            };
            floors.push((ix, floor));
        }
        let output = zaino.exec(&["zainod", "verify"], VERIFY_TIMEOUT).await?;
        let report: Value = serde_json::from_str(&output.stdout).with_context(|| {
            format!("zainod verify (exit {:?}): no JSON\nstderr: {}", output.status, output.stderr)
        })?;
        sync_ensure!(
            output.success() && report["clean"] == json!(true),
            "zainod verify: corruption\n{report:#}\nstderr: {}",
            output.stderr
        );
        for (ix, floor) in floors {
            let heights = report[ix.label()]["heights"].as_u64();
            sync_ensure!(
                at = floor,
                heights.is_some_and(|heights| heights > u64::from(floor)),
                "verify saw {ix:?} commit {heights:?} heights, durable reached {floor}"
            );
        }
        Ok(())
    });

    run.run().await
}
