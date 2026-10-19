//! Mainnet light-wallet index: built from empty to the live tip, then held to zebra.
//!
//! - zebra = optimized fork (`elicbarbieri/zebra` `perf/indexer-block-range`: lazy point
//!   decompression, stored-bytes `GetBlockRange`), restored at `IRONWOOD_MAINNET`, following mainnet
//! - `index`: blocks over zebrad's indexer gRPC (`validator_client = "grpc"`), the rest over
//!   JSON-RPC; at completion each index held to zebra + `zainod verify`
//! - Throughout: every 5 s the durable tip's block + trees vs zebra; durable extents never shrink
//!
//! `ztest sync start zaino_index_construction`

use std::cell::Cell;
use std::collections::BTreeSet;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use serde_json::{json, Value};
use ztest::loadtest::reference::{block_diff, shielded, tree_state_diff, Fees, Zebra};
use ztest::prelude::*;
use ztest::snapshots::IRONWOOD_MAINNET;
use ztest::sync::Severity::Fatal;
use ztest::sync::{hours, mins, secs, Op, OpSet, SyncOutcome, SyncRunner};
use ztest::zebrad_sync::SyncBudget;
use ztest::Empty;
use ztest::{sync_ensure, sync_fail};

// ── topology ──────────────────────────────────────────────────────────────────────────────────

/// Over tailscale (office IP = no public peers)
/// - zingo-infra `golden-mainnet` via tekau NodePort
/// - `elicb-home-server` zebrad's reserved tailnet listener (public 8233 inbound full)
const TRUSTED_MAINNET_P2P: [&str; 2] =
    ["tekau.vaquita-altair.ts.net:30233", "elicb-home-server-zebra.vaquita-altair.ts.net:8234"];
/// Snapshot restores 277 GiB; zebrad appends past the pin
const CHAIN_DISK_GIB: u64 = 320;
/// UNMEASURED: compact-block + tree-state + transparent files for the whole chain
const INDEX_DISK_GIB: u64 = 160;
/// Set on zaino, not mirrored from its default
const FINALISED_DEPTH: u32 = 1_000;
/// Ordered stream → throughput = blocks in flight / tail latency
/// - below the header window: 8 per `GetBlockRange` → 32 streams
const FETCH_CONCURRENCY: NonZeroU32 = NonZeroU32::new(256).expect("non-zero");
/// Merges' read + written MiB/s (zainod default 200: the sync pool's NVMe takes more)
const MERGE_MIB_PER_SEC: NonZeroU32 = NonZeroU32::new(300).expect("non-zero");
/// Block streams carried <= this − the poll's 1 − 2 other lanes' reserves (69 >= 32)
/// - gRPC: ceil(72 / 20) = 4 HTTP/2 connections (zebrad admits 20 streams per connection)
/// - ≤ 100: zebra's jsonrpsee server refuses connection 101+ with HTTP 429 (JSON-RPC: the rest)
const VALIDATOR_CONNECTIONS: NonZeroU32 = NonZeroU32::new(72).expect("non-zero");
/// zebrad's catch-up past the pin: 4 verify threads of its 6 cores (rest = zaino's block reads +
/// RocksDB); verify + download depth = zebra's defaults
const ZEBRA_SYNC_BUDGET: SyncBudget = SyncBudget {
    verify_threads: NonZeroU32::new(4),
    full_verify_blocks: NonZeroU32::new(20).expect("non-zero"),
    download_blocks: NonZeroU32::new(50).expect("non-zero"),
};
/// Above any mainnet address (pool payouts hold millions of receives): the oracle holds whole
/// histories to zebra; the budget's own refusal = unit-tested in zaino-index-transparent-address
const MAX_ADDRESS_ROWS: NonZeroU32 = NonZeroU32::new(1_000_000_000).expect("non-zero");

// ── phase windows ─────────────────────────────────────────────────────────────────────────────

const TICK: Duration = secs(15);
const INDEX_CAP: Duration = hours(48);
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
/// Each side of the durable extent (files → non-finalized state seam)
const SEAM_BLOCKS: u32 = 50;
/// Opening batch + arrivals; < 75 s spacing → most reads see no new block
const MEMPOOL_READ: Duration = secs(30);

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
    description = "light-wallet indexes built from empty to the live mainnet tip, then held to zebra",
    subject = indexer,
    timeout = "60h",
    qos = sync,
    footprint = "16c/20Gi",
    tags = ["mainnet", "zaino", "index", "light-wallet", "ironwood", "live-tip"],
)]
async fn zaino_index_construction(run: SyncRunner) -> SyncOutcome {
    let topology = run.topology(|t| {
        t.set_ready_timeout(READY_WINDOW);
        let zebra = t.add_validator(
            dev!(
                Validator::Zebrad,
                git = "https://github.com/elicbarbieri/zebra.git",
                rev = "e7c50dad3fa906695a6d5da66b24f2ac37797d4f",
                dockerfile = "docker/Dockerfile",
                version = "6.3.0"
            )
            .sync_budget(ZEBRA_SYNC_BUDGET)
            .indexer()
            .follow_from(IRONWOOD_MAINNET, TRUSTED_MAINNET_P2P)
            .disk(Disk::gib(CHAIN_DISK_GIB))
            .resources(Cpu::cores(6), Mem::gib(10)),
        );
        let zaino = t.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile", context = "../..")
                .snapshot(IRONWOOD_MAINNET)
                .finalised_depth(FINALISED_DEPTH)
                .fetch_concurrency(FETCH_CONCURRENCY)
                .lsm_merge_mib_per_sec(MERGE_MIB_PER_SEC)
                .validator_max_connections(VALIDATOR_CONNECTIONS)
                .validator_client(ValidatorClient::Grpc)
                .max_address_rows(MAX_ADDRESS_ROWS)
                .disk(Disk::gib(INDEX_DISK_GIB))
                .resources(Cpu::cores(10), Mem::gib(10)),
        );
        (zebra, zaino)
    });
    let mut run = match topology.await {
        Ok(run) => run,
        Err(e) => return e.into(),
    };
    let pinned_tip = run.chain().tip_height;

    // ── throughout ────────────────────────────────────────────────────────────────────────────

    // Durable = survived an fsync → no reorg, reset or restart takes it back
    let durable = Cell::new([None::<u32>; ZainoIndex::ALL.len()]);
    run.throughout().always("durable_extents_never_shrink", Fatal).every(secs(30)).check(
        async move |_, (_, zaino)| {
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
    run.throughout().always("committed_blocks_are_zebras", Fatal).every(secs(5)).check(
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
    run.throughout().at_completion("no_restart", Fatal).check(async |s, _| {
        sync_ensure!(s.restarts() == 0, "{} restarts (nothing here kills a pod)", s.restarts());
        Ok(())
    });

    // ── index ─────────────────────────────────────────────────────────────────────────────────

    let index = run.phase("index", async |(_, zaino)| Ok(zaino.clone()));
    index.tick(TICK).timeout(INDEX_CAP).requires_work(OpSet::of(&FETCHED_OPS));

    index
        .eventually("index_advances", Fatal)
        .window(STALL_WINDOW)
        .check(async |s, _| Ok(s.progressed_within(STALL_WINDOW)));
    index
        .eventually("validator_found_peers", Fatal)
        .window(PEERING_WINDOW)
        .check(async |_, (zebra, _)| Ok(zebra.has_peers().await? == Health::Ok));
    index
        .eventually("validator_left_the_snapshot", Fatal)
        .window(SNAPSHOT_EXIT_WINDOW)
        .check(async move |_, (zebra, _)| Ok(u32::from(zebra.chain_height().await?) > pinned_tip));
    index.sometimes("fetched_past_the_newest_activation").check(async |s, (zebra, _)| {
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
        async |_, (_, zaino)| {
            for ix in ZainoIndex::ALL {
                let synced = zaino.synced(ix).await?;
                sync_ensure!(synced == Some(true), "{ix:?} not serving: synced = {synced:?}");
            }
            Ok(())
        },
    );
    // zaino's target = zebra's tip → without this, "zaino at the tip" = "wherever zebra stalled"
    index.at_completion("validator_at_network_tip", Fatal).within(NETWORK_CATCHUP_WINDOW).check(
        async |_, (zebra, _)| {
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
        async |_, (zebra, zaino)| {
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
    index.at_completion("lightd_info_is_zebras", Fatal).check(async |_, (zebra, zaino)| {
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
    // Ladder = every activation ±1 (zebra's own schedule), both ends, geometric back from the top
    // - top < `FINALISED_DEPTH` below the tip → the non-finalized state answers, not only files
    index.at_completion("ladder_blocks_and_trees_are_zebras", Fatal).check(
        async |_, (zebra, zaino)| {
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
    // Fees = the ladder's (`Fees::Checked` there)
    index.at_completion("block_range_across_the_seam_is_zebras", Fatal).check(
        async |_, (zebra, zaino)| {
            let reference = Zebra::new(zebra.json_rpc().await?);
            let durable = zaino.finalized_height(ZainoIndex::CompactBlock).await?;
            let durable = durable.context("compact-block index published no durable extent")?;
            let served = u32::from(zaino.latest_block_height().await?);
            let top = served.min(reference.tip().await?) - REORG_MARGIN;
            let (start, end) = (durable - SEAM_BLOCKS, (durable + SEAM_BLOCKS).min(top));
            let blocks = match zaino
                .get_block_range(BlockHeight::from_u32(start), BlockHeight::from_u32(end))
                .await
            {
                Ok(blocks) => blocks,
                Err(e) => sync_fail!(at = start, "zaino GetBlockRange({start}, {end}): {e}"),
            };
            let heights: Vec<u64> = blocks.iter().map(|block| block.height).collect();
            let expected: Vec<u64> = (u64::from(start)..=u64::from(end)).collect();
            sync_ensure!(
                at = start,
                heights == expected,
                "GetBlockRange({start}, {end}) streamed heights {heights:?}"
            );
            for served in &blocks {
                let at = served.height as u32;
                let truth = shielded(reference.compact_block(at, Fees::AsServed(served)).await?);
                if let Some(diff) = block_diff(&truth, served) {
                    sync_fail!(at = at, "block: {diff}");
                }
            }
            Ok(())
        },
    );
    // Judged across a read both tips held still (a new block ends the stream, empties the pool)
    // - held by zebra before + after the read = streamed; streamed = held by zebra before or after
    index.at_completion("mempool_stream_is_zebras", Fatal).within(TERMINAL_WINDOW).check(
        async |_, (zebra, zaino)| {
            let reference = Zebra::new(zebra.json_rpc().await?);
            let tips = async || {
                let served = u32::from(zaino.latest_block_height().await?);
                anyhow::Ok((served, reference.tip().await?))
            };
            let before_tips = tips().await?;
            let before: BTreeSet<String> = match reference.mempool().await {
                Ok(listed) => listed.into_iter().collect(),
                Err(e) => sync_fail!("zebra getrawmempool (mempool still off?): {e}"),
            };
            let mut stream =
                zaino.grpc_client().await?.raw().get_mempool_stream(Empty {}).await?.into_inner();
            let until = tokio::time::Instant::now() + MEMPOOL_READ;
            let mut streamed = BTreeSet::new();
            let ended = loop {
                match tokio::time::timeout_at(until, stream.message()).await {
                    Err(_) => break false,
                    Ok(Ok(Some(tx))) => {
                        streamed.insert(tx.data);
                    }
                    Ok(Ok(None)) => break true,
                    Ok(Err(status)) => sync_fail!("zaino GetMempoolStream: {status}"),
                }
            };
            let after: BTreeSet<String> = match reference.mempool().await {
                Ok(listed) => listed.into_iter().collect(),
                Err(e) => sync_fail!("zebra getrawmempool (mempool still off?): {e}"),
            };
            let after_tips = tips().await?;
            if after_tips != before_tips {
                sync_fail!("tips moved during the read: {before_tips:?} -> {after_tips:?}");
            }
            sync_ensure!(!ended, "GetMempoolStream ended with no new block at {after_tips:?}");

            let mut held = BTreeSet::new();
            for txid in before.intersection(&after) {
                match reference.raw_transaction(txid).await {
                    Ok(raw) => held.insert(raw),
                    Err(e) => sync_fail!("zebra getrawtransaction {txid} (evicted?): {e}"),
                };
            }
            let missing = held.difference(&streamed).count();
            sync_ensure!(
                missing == 0,
                "{missing} of {} txs zebra held throughout never streamed",
                held.len()
            );
            let mut known = held;
            for txid in before.symmetric_difference(&after) {
                if let Ok(raw) = reference.raw_transaction(txid).await {
                    known.insert(raw);
                }
            }
            let phantom = streamed.difference(&known).count();
            sync_ensure!(
                phantom == 0,
                "{phantom} of {} streamed txs zebra never held",
                streamed.len()
            );
            Ok(())
        },
    );
    // Coinbase recipients at every activation + near the tip = funded addresses spanning every era
    // - Judged only across a window both tips held still (a live address can gain an output)
    index.at_completion("coinbase_balances_are_zebras", Fatal).check(async |_, (zebra, zaino)| {
        let client = zebra.json_rpc().await?;
        let served = u32::from(zaino.latest_block_height().await?);
        let top = served.min(u32::from(zebra.chain_height().await?)) - REORG_MARGIN;
        let info = client.call_value("getblockchaininfo", json!([])).await?;
        let upgrades = info["upgrades"].as_object().context("getblockchaininfo: no upgrades")?;
        let activations = upgrades
            .values()
            .filter(|upgrade| upgrade["status"] == "active")
            .filter_map(|upgrade| upgrade["activationheight"].as_u64());
        let mut addresses = Vec::new();
        for at in activations.chain([u64::from(top)]) {
            let block = client.call_value("getblock", json!([at.to_string(), 2])).await?;
            let coinbase = block["tx"][0]["vout"].as_array().context("getblock: no coinbase")?;
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
    });
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
