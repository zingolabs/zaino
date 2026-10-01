//! Mainnet light-wallet index: built from empty, then synced through by a real wallet.
//!
//! - zebra = lazy-point-decompression fork, restored at `IRONWOOD_MAINNET`, following mainnet
//! - `index`: every index built over JSON-RPC; at completion each held to zebra + `zainod verify`
//! - `wallet`: pepper-sync scans [`WALLET`] birthday → tip under live blocks; roots, balances, notes
//!   vs the truth
//! - Both phases: every 5 s the durable tip's block + trees vs zebra; durable extents never shrink
//!
//! `ztest sync start zaino_index_construction`

use std::cell::Cell;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use serde_json::{json, Value};
use ztest::backends::zingolib::PerformanceLevel;
use ztest::loadtest::reference::{block_diff, tree_state_diff, Fees, Zebra};
use ztest::prelude::*;
use ztest::snapshots::IRONWOOD_MAINNET;
use ztest::sync::Severity::Fatal;
use ztest::sync::{commitment_tree_root, hours, mins, secs, Op, OpSet, SyncOutcome, SyncRunner};
use ztest::{sync_ensure, sync_fail};

/// Zingo team's frozen mainnet test wallet (view key only; never sent to or from again)
///
/// - truth = zingo-cli clearnet sync: nothing spendable, 3 zero-value unspent Orchard notes
///   (2,225,924; 2,319,745; 2,319,748)
const WALLET: FundedWallet = FundedWallet {
    ufvk: "uview1ta2tvwhnfgafrjcl97yz26ynpktfllr7x6uvy6srmzepy5w6uz7l26k72jfhhyy4ulv2kw5nuhtfwemgahudtxl9q3ve5xtjrakpmt5re96qs72qplgk6ecgxn4mgrzs6gevpws3wx65gxymaf3u657pypn5yj9r35zqjpsdnfyqv0vqnf2fkty2789hn7kmssqj3pqg6z24gsrqdux65m2p6dgkthc8ssymvxhte40gc5ypqfqtcsxg9g3fvs8gjzwxj7wzfp8x42ycfkpc9mlu86prpz507kff2s35nkspc2pwzc00ffak0j5j79jgea98j34txmmkp2gpv4ufg0wswt40hauc2vja6sqft5f5jlknlxjq3c4s7apz0h478xrpk5hp0qm4rz9vhrj9y3ege23lkp8yr2a5j896th33q67fyl7paczs4cdpy0w8xynen62gdntw74wj6fnmtjd69fqnewtt63p9nzayhva5th2295ydp950",
    birthday: 2_208_514,
    balances: PoolBalances { orchard: 0, ironwood: 0, sapling: 0, transparent: 0 },
    notes: NoteCounts { sapling: 0, orchard: 3, ironwood: 0, transparent: 0 },
};

struct FundedWallet {
    ufvk: &'static str,
    birthday: u32,
    balances: PoolBalances,
    notes: NoteCounts,
}

// ── topology ──────────────────────────────────────────────────────────────────────────────────

/// zingo-infra `golden-mainnet` via tekau NodePort (direct tailscale; office IP = no public peers)
const GOLDEN_MAINNET_P2P: &str = "tekau.vaquita-altair.ts.net:30233";
/// Snapshot restores 277 GiB; zebrad appends past the pin
const CHAIN_DISK_GIB: u64 = 320;
/// UNMEASURED: compact-block + tree-state + transparent files for the whole chain
const INDEX_DISK_GIB: u64 = 160;
/// Set on zaino, not mirrored from its default
const FINALISED_DEPTH: u32 = 1_000;
/// Ordered stream → throughput = in-flight / tail latency
/// - ≤ 100: zebra's jsonrpsee server refuses connection 101+ with HTTP 429 (not configurable)
const FETCH_CONCURRENCY: NonZeroU32 = NonZeroU32::new(64).expect("non-zero");
/// Above any mainnet address (pool payouts hold millions of receives): the oracle holds whole
/// histories to zebra; the budget's own refusal = unit-tested in zaino-index-transparent-address
const MAX_ADDRESS_ROWS: NonZeroU32 = NonZeroU32::new(1_000_000_000).expect("non-zero");

// ── phase windows ─────────────────────────────────────────────────────────────────────────────

const TICK: Duration = secs(15);
const INDEX_CAP: Duration = hours(48);
/// UNMEASURED; pepper-sync = 2 scan workers, trial-decrypts every output from the birthday
const WALLET_CAP: Duration = hours(12);
const READY_WINDOW: Duration = mins(20);
/// First minutes = zebrad opening a 277 GiB state, not indexing
const STALL_WINDOW: Duration = mins(15);
/// Seeding = DNS + handshake; never latching = a peerless zebrad serving its pin
const PEERING_WINDOW: Duration = mins(10);
/// Peers but still on the pin = handshakes that never turn into block download
const SNAPSHOT_EXIT_WINDOW: Duration = mins(30);
/// zaino trails a moving zebra by one poll batch
const TERMINAL_WINDOW: Duration = mins(30);
/// Zebrad may still be closing the pin → tip gap at completion
const NETWORK_CATCHUP_WINDOW: Duration = hours(3);
/// Sequential read of every committed byte against its page checksums; UNMEASURED
const VERIFY_TIMEOUT: Duration = hours(3);

// ── live-chain tolerances ─────────────────────────────────────────────────────────────────────

/// No block for 15 min at 75 s spacing ≈ e^-12 → an older tip = zebra still catching up
const LIVE_TIP_AGE: Duration = mins(15);
/// Blocks the chain may gain between two reads (~1 per 75 s; 3 absorbs a burst)
const CHAIN_MOTION_SLACK: u32 = 3;
/// Below both tips for zebra comparisons (a live tip can reorg under one)
const REORG_MARGIN: u32 = 10;
/// Tries for a transparent comparison over a window where both tips held still
const STILL_WINDOW_ATTEMPTS: u32 = 12;

const FETCHED_OPS: [Op; 6] = [
    Op::TransparentIn,
    Op::TransparentOut,
    Op::SaplingSpend,
    Op::SaplingOutput,
    Op::OrchardAction,
    Op::IronwoodAction,
];

#[ztest::needs(IRONWOOD_MAINNET)]
#[ztest::sync_test(
    name = "zaino_index_construction",
    description = "light-wallet indexes built from empty to the live mainnet tip, then a funded zingolib wallet synced through them under live blocks",
    subject = indexer,
    timeout = "60h",
    qos = sync,
    footprint = "14c/20Gi",
    tags = ["mainnet", "zaino", "index", "light-wallet", "pepper-sync", "ironwood", "live-tip"],
)]
async fn zaino_index_construction(run: SyncRunner) -> SyncOutcome {
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
            .follow_from(IRONWOOD_MAINNET, [GOLDEN_MAINNET_P2P])
            .disk(Disk::gib(CHAIN_DISK_GIB))
            .resources(Cpu::cores(4), Mem::gib(10)),
        );
        let zaino = t.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile", context = "../..")
                .snapshot(IRONWOOD_MAINNET)
                .finalised_depth(FINALISED_DEPTH)
                .fetch_concurrency(FETCH_CONCURRENCY)
                .max_address_rows(MAX_ADDRESS_ROWS)
                .disk(Disk::gib(INDEX_DISK_GIB))
                .resources(Cpu::cores(10), Mem::gib(10)),
        );
        let wallet = t.add_wallet(Wallet::zingolib().performance(PerformanceLevel::Maximum));
        (zebra, zaino, wallet)
    });
    let mut run = match topology.await {
        Ok(run) => run,
        Err(e) => return e.into(),
    };
    let pinned_tip = run.chain().tip_height;

    // ── both phases ───────────────────────────────────────────────────────────────────────────

    // Durable = survived an fsync → no reorg, reset or restart takes it back
    let durable = Cell::new([None::<u32>; ZainoIndex::ALL.len()]);
    run.throughout().always("durable_extents_never_shrink", Fatal).every(secs(30)).check(
        async move |_, (_, zaino, _)| {
            let mut seen = durable.get();
            for (ix, last) in ZainoIndex::ALL.into_iter().zip(&mut seen) {
                let Some(now) = zaino.finalized_height(ix).await? else { continue };
                sync_ensure!(last.is_none_or(|was| now >= was), "{ix:?} shrank {last:?} -> {now}");
                *last = Some(now);
            }
            durable.set(seen);
            Ok(())
        },
    );
    // Durable = final → answered mid-build too: ~1 height per 5 s sampled across the whole chain
    run.throughout().always("committed_tree_states_are_zebras", Fatal).every(secs(5)).check(
        async |_, (zebra, zaino, _)| {
            let Some(at) = zaino.finalized_height(ZainoIndex::TreeState).await? else {
                return Ok(());
            };
            let served = match zaino.get_tree_state(BlockHeight::from_u32(at)).await {
                Ok(served) => served,
                Err(e) => sync_fail!(at = at, "zaino GetTreeState: {e}"),
            };
            let truth = Zebra::new(zebra.json_rpc().await?).tree_state(at).await?;
            if let Some(diff) = tree_state_diff(&truth, &served) {
                sync_fail!(at = at, "tree state: {diff}");
            }
            Ok(())
        },
    );
    run.throughout().always("committed_blocks_are_zebras", Fatal).every(secs(5)).check(
        async |_, (zebra, zaino, _)| {
            let Some(at) = zaino.finalized_height(ZainoIndex::CompactBlock).await? else {
                return Ok(());
            };
            let served = match zaino.get_block(BlockHeight::from_u32(at)).await {
                Ok(served) => served,
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
    run.throughout().at_completion("no_restart", Fatal).check(async |s, _| {
        sync_ensure!(s.restarts() == 0, "{} restarts (nothing here kills a pod)", s.restarts());
        Ok(())
    });

    // ── index ─────────────────────────────────────────────────────────────────────────────────

    let index = run.phase("index", async |(_, zaino, _)| Ok(zaino.clone()));
    index.tick(TICK).timeout(INDEX_CAP).requires_work(OpSet::of(&FETCHED_OPS));

    index
        .eventually("index_advances", Fatal)
        .window(STALL_WINDOW)
        .check(async |s, _| Ok(s.progressed_within(STALL_WINDOW)));
    index
        .eventually("validator_found_peers", Fatal)
        .window(PEERING_WINDOW)
        .check(async |_, (zebra, _, _)| Ok(zebra.has_peers().await? == Health::Ok));
    index.eventually("validator_left_the_snapshot", Fatal).window(SNAPSHOT_EXIT_WINDOW).check(
        async move |_, (zebra, _, _)| Ok(u32::from(zebra.chain_height().await?) > pinned_tip),
    );
    index.sometimes("fetched_past_the_newest_activation").check(async |s, (zebra, _, _)| {
        let info = zebra.json_rpc().await?.call_value("getblockchaininfo", json!([])).await?;
        let upgrades = info["upgrades"].as_object().context("getblockchaininfo: no upgrades")?;
        let newest = upgrades
            .values()
            .filter(|upgrade| upgrade["status"] == "active")
            .filter_map(|upgrade| upgrade["activationheight"].as_u64())
            .max()
            .context("getblockchaininfo: no active upgrade")?;
        Ok(u64::from(s.height()) > newest)
    });

    index.at_completion("every_index_serves", Fatal).within(TERMINAL_WINDOW).check(
        async |_, (_, zaino, _)| {
            for ix in ZainoIndex::ALL {
                let synced = zaino.synced(ix).await?;
                sync_ensure!(synced == Some(true), "{ix:?} not serving: synced = {synced:?}");
            }
            Ok(())
        },
    );
    // zaino's target = zebra's tip → without this, "zaino at the tip" = "wherever zebra stalled"
    index.at_completion("validator_at_network_tip", Fatal).within(NETWORK_CATCHUP_WINDOW).check(
        async |_, (zebra, _, _)| {
            let client = zebra.json_rpc().await?;
            let info = client.call_value("getblockchaininfo", json!([])).await?;
            let tip = info["bestblockhash"].as_str().context("getblockchaininfo: no tip hash")?;
            let header = client.call_value("getblockheader", json!([tip])).await?;
            let mined = header["time"].as_u64().with_context(|| format!("tip {tip}: no time"))?;
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
            let age = secs(now.saturating_sub(mined));
            sync_ensure!(age <= LIVE_TIP_AGE, "zebra's tip {tip} mined {age:?} ago");
            Ok(())
        },
    );
    index.at_completion("served_tip_is_the_validator_tip", Fatal).within(TERMINAL_WINDOW).check(
        async |_, (zebra, zaino, _)| {
            let served = u32::from(zaino.latest_block_height().await?);
            let durable = zaino.finalized_height(ZainoIndex::CompactBlock).await?;
            let durable = durable.context("compact-block index published no durable extent")?;
            let tip = u32::from(zebra.chain_height().await?);
            sync_ensure!(served <= tip, "zaino serves {served}, above zebra's tip {tip}");
            sync_ensure!(tip - served <= CHAIN_MOTION_SLACK, "zaino serves {served}, zebra {tip}");
            let (lag, bound) = (tip - durable, FINALISED_DEPTH + CHAIN_MOTION_SLACK);
            sync_ensure!(lag <= bound, "durable {lag} behind tip {tip} (bound {bound})");
            Ok(())
        },
    );
    // Height = zaino's own served tip (bracketed: the live tip moves); chain from zaino's config;
    // activation + branch from zebra
    index.at_completion("lightd_info_is_zebras", Fatal).check(async |_, (zebra, zaino, _)| {
        let before = u64::from(u32::from(zaino.latest_block_height().await?));
        let info = zaino.indexer_info().await?;
        let after = u64::from(u32::from(zaino.latest_block_height().await?));
        let height = info.block_height;
        sync_ensure!(
            (before..=after).contains(&height),
            "height {height}, served {before}..={after}"
        );

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
        );
        let from_zaino = (
            info.chain_name.as_str(),
            info.sapling_activation_height,
            info.consensus_branch_id.as_str(),
        );
        sync_ensure!(
            from_zaino == from_zebra,
            "(chain, sapling activation, branch)\n  zaino: {from_zaino:?}\n  zebra: {from_zebra:?}"
        );
        Ok(())
    });
    // Whole list per pool; one side answering while the other refuses = disagreement
    index.at_completion("subtree_roots_are_zebras", Fatal).check(async |_, (zebra, zaino, _)| {
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
    // Ladder = every activation ±1 (zebra's own schedule), both ends, geometric back from the top
    // - top < `FINALISED_DEPTH` below the tip → the non-finalized state answers, not only files
    index.at_completion("ladder_blocks_and_trees_are_zebras", Fatal).check(
        async |_, (zebra, zaino, _)| {
            let client = zebra.json_rpc().await?;
            let reference = Zebra::new(client.clone());
            let served = u32::from(zaino.latest_block_height().await?);
            let top = served.min(u32::from(zebra.chain_height().await?)) - REORG_MARGIN;
            let info = client.call_value("getblockchaininfo", json!([])).await?;
            let upgrades =
                info["upgrades"].as_object().context("getblockchaininfo: no upgrades")?;
            let mut ladder = vec![1, top];
            for at in upgrades.values().filter_map(|upgrade| upgrade["activationheight"].as_u64()) {
                let at = at as u32;
                ladder.extend([at.saturating_sub(1), at, at + 1]);
            }
            let mut back = 1;
            while back < top {
                ladder.push(top - back);
                back = back.saturating_mul(4);
            }
            ladder.retain(|height| (1..=top).contains(height));
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
    // Coinbase recipients at every activation + near the tip = funded addresses spanning every era
    // - Judged only across a window both tips held still (a live address can gain an output)
    index.at_completion("coinbase_balances_are_zebras", Fatal).check(
        async |_, (zebra, zaino, _)| {
            let client = zebra.json_rpc().await?;
            let served = u32::from(zaino.latest_block_height().await?);
            let top = served.min(u32::from(zebra.chain_height().await?)) - REORG_MARGIN;
            let info = client.call_value("getblockchaininfo", json!([])).await?;
            let upgrades =
                info["upgrades"].as_object().context("getblockchaininfo: no upgrades")?;
            let activations = upgrades
                .values()
                .filter(|upgrade| upgrade["status"] == "active")
                .filter_map(|upgrade| upgrade["activationheight"].as_u64());
            let mut addresses = Vec::new();
            for at in activations.chain([u64::from(top)]) {
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

            let tips = async || {
                let served = zaino.latest_block_height().await.ok().map(u32::from);
                (served, zebra.chain_height().await.ok().map(u32::from))
            };
            for _ in 0..STILL_WINDOW_ATTEMPTS {
                let before = tips().await;
                if before.0.is_none() || before.0 != before.1 {
                    tokio::time::sleep(secs(5)).await;
                    continue;
                }
                let mut first_difference = None;
                for address in &addresses {
                    let balance = match zaino.get_taddress_balance(vec![address.clone()]).await {
                        Ok(balance) => i64::from(balance),
                        Err(e) => sync_fail!("zaino GetTaddressBalance({address}): {e}"),
                    };
                    let from = BlockHeight::from_u32(0);
                    let utxos = match zaino.get_address_utxos(vec![address.clone()], from, 0).await
                    {
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

                    if from_zaino != from_zebra && first_difference.is_none() {
                        let utxo = from_zaino.1.iter().zip(&from_zebra.1).find(|(s, t)| s != t);
                        first_difference = Some(format!(
                            "{address}: balance zaino {} zebra {}; utxos zaino {} zebra {}; \
                             first differing (zaino, zebra) {utxo:?}",
                            from_zaino.0,
                            from_zebra.0,
                            from_zaino.1.len(),
                            from_zebra.1.len()
                        ));
                    }
                }
                if tips().await != before {
                    continue;
                }
                if let Some(detail) = first_difference {
                    sync_fail!(at = before.1.unwrap_or_default(), "{detail}");
                }
                return Ok(());
            }
            anyhow::bail!("no still window in {STILL_WINDOW_ATTEMPTS} tries")
        },
    );
    // Durable extents read first → the walk (files only grow) must reach at least them
    index.at_completion("index_files_verify_clean", Fatal).check(async |_, (_, zaino, _)| {
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

    // ── wallet ────────────────────────────────────────────────────────────────────────────────

    let wallet = run.phase("wallet", async |(zebra, zaino, wallet)| {
        let birthday = BlockHeight::from_u32(WALLET.birthday);
        let opened = wallet.viewing_account(zebra, zaino, WALLET.ufvk, birthday).await?;
        opened.wallet().sync_subject(opened.id()).map_err(anyhow::Error::from_boxed)
    });
    wallet.tick(secs(10)).timeout(WALLET_CAP);

    wallet.always("scan_never_regresses", Fatal).each_tick().check(async |s, _| {
        let (was, now) = (s.prev_height(), s.height());
        sync_ensure!(now >= was, "fully scanned height fell {was} -> {now}");
        Ok(())
    });
    // Non-finalized state serves each live block within motion slack of zebra, either side
    wallet.always("served_tip_tracks_zebra", Fatal).every(secs(30)).check(
        async |_, (zebra, zaino, _)| {
            let served = u32::from(zaino.latest_block_height().await?);
            let tip = u32::from(zebra.chain_height().await?);
            let gap = served.abs_diff(tip);
            sync_ensure!(gap <= CHAIN_MOTION_SLACK, "zaino serves {served}, zebra at {tip}");
            Ok(())
        },
    );
    wallet
        .eventually("wallet_advances", Fatal)
        .window(mins(20))
        .check(async |s, _| Ok(s.progressed_within(mins(20))));

    wallet.at_completion("wallet_reached_the_served_tip", Fatal).check(async |s, (_, zaino, _)| {
        let (scanned, tip) = (s.height(), u32::from(zaino.latest_block_height().await?));
        sync_ensure!(scanned + CHAIN_MOTION_SLACK >= tip, "scanned {scanned}, serves {tip}");
        Ok(())
    });
    // Any output missing, extra or misordered in a served compact block moves a root
    wallet.at_completion("wallet_tree_roots_are_zebras", Fatal).check(async |s, (zebra, _, _)| {
        let at = s.height();
        let truth = zebra.json_rpc().await?;
        let truth = truth.call_value("z_gettreestate", json!([at.to_string()])).await?;
        for (pool, key) in
            [(Pool::Sapling, "sapling"), (Pool::Orchard, "orchard"), (Pool::Ironwood, "ironwood")]
        {
            // absent = empty tree (pool not yet active)
            let frontier = truth[key]["commitments"]["finalState"].as_str().unwrap_or_default();
            let expected = commitment_tree_root(pool, frontier)
                .with_context(|| format!("zebra {key} tree at {at}"))?;
            let wallet = s.tree_roots().require(pool);
            sync_ensure!(
                at = at,
                wallet == expected,
                "{key}: wallet {wallet:?}, zebra {expected:?}"
            );
        }
        Ok(())
    });
    wallet.at_completion("wallet_holds_the_truth", Fatal).check(async |s, _| {
        let got = (s.balances(), s.notes());
        let want = (WALLET.balances, WALLET.notes);
        sync_ensure!(got == want, "(balances, unspent notes)\n  got:  {got:?}\n  want: {want:?}");
        Ok(())
    });
    // Anti-vacuity: a "complete" sync that skipped blocks never trial-decrypted them
    wallet.at_completion("wallet_scanned_every_block", Fatal).check(async |s, _| {
        let scan = s.scan().context("pepper-sync finished no scan session")?;
        let (start, end, scanned) = (scan.start_height, scan.end_height, scan.blocks_scanned);
        let (birthday, at) = (WALLET.birthday, s.height());
        sync_ensure!(start <= birthday + 1, "scan started at {start}, birthday {birthday}");
        sync_ensure!(end >= at, "scan ended at {end}, wallet reports {at}");
        sync_ensure!(scanned == end - start + 1, "scanned {scanned} blocks over [{start}, {end}]");
        Ok(())
    });

    run.run().await
}
