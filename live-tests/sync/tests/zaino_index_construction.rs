//! Light-wallet index construction on mainnet, following the live tip, then a funded wallet
//! synced through it.
//!
//! - zebra = the lazy-point-decompression fork, restored at `IRONWOOD_MAINNET`, following the
//!   live network; zaino builds every index from empty over JSON-RPC
//! - Phase `index`: durable extents never shrink, watermarks ordered, a syncing index refuses,
//!   zebra leaves its snapshot, committed trees = zebra's every 5 s; at completion every index
//!   held to zebra + `zainod verify` in-pod
//! - Phase `follow`: [`FOLLOW_BLOCKS`] live blocks through pre-commit; served tip tracks zebra,
//!   committed trees = zebra's every 5 s, top-of-chain answers
//!   held to zebra again
//! - Phase `serve`: simulated light wallets ramped against the built index ([`Plan::mainnet`]);
//!   every served block byte-consistent, a sample of every answer held to zebra, capacity per
//!   stage in the report
//! - Phase `wallet`: pepper-sync scans [`WALLET`] from its birthday to zaino's tip; tree roots vs
//!   zebra's, balances + notes vs the declared truth, every block scanned
//!
//! `ztest sync start zaino_index_construction`

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use ztest::backends::zainod::{family, ServeCaps};
use ztest::loadtest::load::{LoadRun, LoadSubject, Plan, PodCgroup, Target};
use ztest::loadtest::reference::{block_diff, tree_state_diff, Fees, Zebra};
use ztest::prelude::*;
use ztest::snapshots::IRONWOOD_MAINNET;
use ztest::sync::{
    commitment_tree_root, hours, mins, secs, Op, OpSet, Severity, Snapshot, SyncCtx, SyncOutcome,
    SyncRunner, TreeRoots, Verdict, Violation,
};
use ztest::{sync_ensure, AccountId, ZingolibWallet};

/// Zingo team's frozen mainnet test wallet (view key only; never sent to or from again)
///
/// - truth = zingo-cli clearnet sync: nothing spendable, 3 zero-value unspent Orchard notes
///   (2,225,924; 2,319,745; 2,319,748)
const WALLET: FundedWallet = FundedWallet {
    ufvk: "uview1ta2tvwhnfgafrjcl97yz26ynpktfllr7x6uvy6srmzepy5w6uz7l26k72jfhhyy4ulv2kw5nuhtfwemgahudtxl9q3ve5xtjrakpmt5re96qs72qplgk6ecgxn4mgrzs6gevpws3wx65gxymaf3u657pypn5yj9r35zqjpsdnfyqv0vqnf2fkty2789hn7kmssqj3pqg6z24gsrqdux65m2p6dgkthc8ssymvxhte40gc5ypqfqtcsxg9g3fvs8gjzwxj7wzfp8x42ycfkpc9mlu86prpz507kff2s35nkspc2pwzc00ffak0j5j79jgea98j34txmmkp2gpv4ufg0wswt40hauc2vja6sqft5f5jlknlxjq3c4s7apz0h478xrpk5hp0qm4rz9vhrj9y3ege23lkp8yr2a5j896th33q67fyl7paczs4cdpy0w8xynen62gdntw74wj6fnmtjd69fqnewtt63p9nzayhva5th2295ydp950",
    birthday: 2_208_514,
    balances: PoolBalances {
        orchard: 0,
        ironwood: 0,
        sapling: 0,
        transparent: 0,
    },
    notes: [0, 3, 0, 0],
};

/// `notes` = unspent sapling, orchard, ironwood, transparent
struct FundedWallet {
    ufvk: &'static str,
    birthday: u32,
    balances: PoolBalances,
    notes: [usize; 4],
}

const TICK: Duration = secs(15);
const INDEX_CAP: Duration = hours(48);
/// ~75 s blocks → ~30 min of live tip
const FOLLOW_BLOCKS: u32 = 24;
const FOLLOW_CAP: Duration = hours(2);
/// UNMEASURED; pepper-sync = 2 scan workers, trial-decrypts every output from the birthday
const WALLET_CAP: Duration = hours(12);
/// Plan ≈ 35 min of stages + calibration + up to 20 min settling tip audits
const SERVE_CAP: Duration = hours(3);
/// Above any mainnet address (pool payouts hold millions of receives): the oracle holds whole
/// histories to zebra; the budget's own refusal = unit-tested in zaino-index-transparent-address
const MAX_ADDRESS_ROWS: NonZeroU32 = NonZeroU32::new(1_000_000_000).expect("non-zero");
/// One driver pod = one client address; the plan's 5k wallets hold a connection + a mempool
/// stream each
const SERVE_CAPS: ServeCaps = ServeCaps {
    max_connections: NonZeroU32::new(16_384).expect("non-zero"),
    max_connections_per_ip: NonZeroU32::new(16_384).expect("non-zero"),
    max_streams: NonZeroU32::new(16_384).expect("non-zero"),
    max_subscriptions: NonZeroU32::new(16_384).expect("non-zero"),
};
/// First minutes = zebrad opening a 277 GiB state, not indexing
const STALL_WINDOW: Duration = mins(15);
const READY_WINDOW: Duration = mins(20);
/// Seeding = DNS + handshake; never latching = a peerless zebrad serving its pin
const PEERING_WINDOW: Duration = mins(10);
/// Peers but still on the pin = handshakes that never turn into block download
const SNAPSHOT_EXIT_WINDOW: Duration = mins(30);
const TERMINAL_WINDOW: Duration = mins(30);
/// Completion fires on sync; zebrad still closing the snapshot's gap to the network (3,434,171 vs
/// ~3.5M = ~62k blocks). Sized for public peers (~7.5 blk/s); UNMEASURED fed by golden-mainnet
const NETWORK_CATCHUP_WINDOW: Duration = hours(3);
/// Our reference mainnet zebra (zingo-infra `golden-mainnet`, `zebra-p2p` NodePort on tekau's own
/// tailscaled: direct path, not DERP); public peers already hold the office egress IP's one slot
/// per peer, so a zebra dialing from there gets none
const GOLDEN_MAINNET_P2P: &str = "tekau.vaquita-altair.ts.net:30233";
const TERMINAL_POLL: Duration = secs(10);
/// Sequential read of every committed byte against its page checksums; UNMEASURED
const VERIFY_TIMEOUT: Duration = hours(3);
const SCRAPE: Duration = secs(10);

/// Set on zaino, not mirrored from its default
const FINALISED_DEPTH: u32 = 1_000;
/// Ordered stream → throughput = in-flight / tail latency
/// - ≤ 100: zebra's jsonrpsee server refuses connection 101+ with HTTP 429 (not configurable)
const FETCH_CONCURRENCY: NonZeroU32 = NonZeroU32::new(64).expect("non-zero");
/// Blocks the chain may gain between two reads (~1 per 75 s; 3 absorbs a burst)
const CHAIN_MOTION_SLACK: u32 = 3;
/// Below both tips for zebra comparisons (a live tip can reorg under one)
const REORG_MARGIN: u32 = 10;
/// `estimatedheight` extrapolates from block times → tens short at the tip
const NETWORK_ESTIMATE_SLACK: u32 = 24;
/// Snapshot restores 277 GiB; zebrad appends past the pin
const CHAIN_DISK_GIB: u64 = 320;
/// UNMEASURED: compact-block + tree-state + transparent files for the whole chain
const INDEX_DISK_GIB: u64 = 160;
/// p2pkh of hash160 `00…00` (never funded) = a cheap transparent request for the gate probe
const UNFUNDED_T_ADDR: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
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
    description = "light-wallet indexes built from empty to the live mainnet tip, followed through pre-commit, then a funded zingolib wallet synced through them",
    subject = indexer,
    timeout = "60h",
    qos = sync,
    footprint = "16c/24Gi",
    tags = ["mainnet", "zaino", "index", "light-wallet", "pepper-sync", "ironwood", "live-tip"],
)]
async fn zaino_index_construction(mut run: SyncRunner) -> SyncOutcome {
    let (zebra, zaino, wallet) = match run
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
                .follow_from(IRONWOOD_MAINNET, [GOLDEN_MAINNET_P2P])
                .disk(Disk::gib(CHAIN_DISK_GIB))
                .resources(Cpu::cores(6), Mem::gib(10)),
            );
            let zaino = t.add_indexer(
                dev!(
                    Indexer::Zainod,
                    "../../Dockerfile",
                    context = "../..",
                    features = ["prometheus"]
                )
                .snapshot(IRONWOOD_MAINNET)
                .finalised_depth(FINALISED_DEPTH)
                .fetch_concurrency(FETCH_CONCURRENCY)
                .serve_caps(SERVE_CAPS)
                .max_address_rows(MAX_ADDRESS_ROWS)
                .disk(Disk::gib(INDEX_DISK_GIB))
                .resources(Cpu::cores(8), Mem::gib(10)),
            );
            let wallet = t.add_wallet(
                Wallet::zingolib()
                    .performance(ztest::backends::zingolib::PerformanceLevel::Maximum),
            );
            (zebra, zaino, wallet)
        })
        .await
    {
        Ok(handles) => handles,
        Err(e) => return e.into(),
    };
    let chain = run.chain();

    run.sync(zaino.clone());
    run.named("index").tick(TICK).timeout(INDEX_CAP);
    run.requires_work(OpSet::of(&FETCHED_OPS));

    let durable = Arc::new(ZainoIndex::ALL.map(|_| AtomicU32::new(0)));
    let fetch_max = Arc::new(AtomicU32::new(0));
    let refused_while_syncing = Arc::new(AtomicBool::new(false));

    {
        let (zaino, durable) = (zaino.clone(), durable.clone());
        run.always(Severity::Fatal)
            .named("durable_extents_never_shrink")
            .every(secs(30))
            .check_rpc(move |_s, _cx| {
                let (zaino, durable) = (zaino.clone(), durable.clone());
                Box::pin(async move { durable_extents_never_shrink(&zaino, &durable).await })
            });
    }
    {
        let (zaino, zebra, fetch_max) = (zaino.clone(), zebra.clone(), fetch_max.clone());
        run.always(Severity::Fatal).named("watermarks_ordered").every(secs(30)).check_rpc(
            move |_s, _cx| {
                let (zaino, zebra, fetch_max) = (zaino.clone(), zebra.clone(), fetch_max.clone());
                Box::pin(async move { watermarks_ordered(&zaino, &zebra, &fetch_max).await })
            },
        );
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        run.always(Severity::Fatal)
            .named("committed_tree_states_are_zebras")
            .every(secs(5))
            .check_rpc(move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { committed_tree_states_are_zebras(&zaino, &zebra).await })
            });
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
    {
        let zebra = zebra.clone();
        run.eventually(Severity::Fatal)
            .named("validator_found_peers")
            .window(PEERING_WINDOW)
            .check_rpc(move |_s, _cx| {
                let zebra = zebra.clone();
                Box::pin(async move {
                    match zebra.has_peers().await {
                        Ok(Health::Ok) => Verdict::Satisfied,
                        Ok(_) => Verdict::Pending,
                        Err(e) => Verdict::ProbeError(format!("zebra health: {e}")),
                    }
                })
            });
    }
    {
        let zebra = zebra.clone();
        run.eventually(Severity::Fatal)
            .named("validator_left_the_snapshot")
            .window(SNAPSHOT_EXIT_WINDOW)
            .check_rpc(move |_s, _cx| {
                let zebra = zebra.clone();
                Box::pin(async move {
                    match zebra.chain_height().await {
                        Ok(h) if u32::from(h) > chain.tip_height => Verdict::Satisfied,
                        Ok(_) => Verdict::Pending,
                        Err(e) => Verdict::ProbeError(format!("zebra chain_height: {e}")),
                    }
                })
            });
    }

    run.sometimes().named("observed_a_partial_index").check(|s: &Snapshot| match s.target() {
        Some(target) if s.height() < target => Verdict::Satisfied,
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
    {
        let zebra = zebra.clone();
        run.sometimes().named("fetched_past_the_newest_activation").check_rpc(move |s, _cx| {
            let zebra = zebra.clone();
            let fetched = s.height();
            Box::pin(async move { past_newest_activation(&zebra, fetched).await })
        });
    }

    run.at_completion(Severity::Fatal).named("no_restart").check(no_restart);
    {
        let zaino = zaino.clone();
        run.at_completion(Severity::Fatal).named("every_index_serves").check_rpc(move |s, _cx| {
            let zaino = zaino.clone();
            let at = s.height();
            Box::pin(async move { every_index_serves(&zaino, at).await })
        });
    }
    {
        let zebra = zebra.clone();
        run.at_completion(Severity::Fatal).named("validator_at_network_tip").check_rpc(
            move |s, _cx| {
                let zebra = zebra.clone();
                let at = s.height();
                Box::pin(async move { validator_at_network_tip(&zebra, at).await })
            },
        );
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        run.at_completion(Severity::Fatal).named("served_tip_is_the_validator_tip").check_rpc(
            move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { served_tip_is_the_validator_tip(&zaino, &zebra).await })
            },
        );
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        run.at_completion(Severity::Fatal).named("indexes_agree_with_zebra").check_rpc(
            move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { indexes_agree_with_zebra(&zaino, &zebra).await })
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

    let follow = {
        let zaino = zaino.clone();
        run.then("follow", move |_cx: SyncCtx| async move { Ok(zaino) })
    };
    follow.tick(TICK).timeout(FOLLOW_CAP).for_blocks(FOLLOW_BLOCKS);

    {
        let (zaino, durable) = (zaino.clone(), durable.clone());
        follow
            .always(Severity::Fatal)
            .named("durable_extents_never_shrink")
            .every(secs(30))
            .check_rpc(move |_s, _cx| {
                let (zaino, durable) = (zaino.clone(), durable.clone());
                Box::pin(async move { durable_extents_never_shrink(&zaino, &durable).await })
            });
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        follow
            .always(Severity::Fatal)
            .named("committed_tree_states_are_zebras")
            .every(secs(5))
            .check_rpc(move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { committed_tree_states_are_zebras(&zaino, &zebra).await })
            });
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        follow.always(Severity::Fatal).named("served_tip_tracks_zebra").every(secs(30)).check_rpc(
            move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { served_tip_tracks_zebra(&zaino, &zebra).await })
            },
        );
    }
    follow.eventually(Severity::Fatal).named("follows_new_blocks").window(STALL_WINDOW).check(
        |s: &Snapshot| {
            if s.progressed_within(STALL_WINDOW) {
                Verdict::Satisfied
            } else {
                Verdict::Pending
            }
        },
    );
    follow.at_completion(Severity::Fatal).named("no_restart").check(no_restart);
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        follow.at_completion(Severity::Fatal).named("served_tip_is_the_validator_tip").check_rpc(
            move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { served_tip_is_the_validator_tip(&zaino, &zebra).await })
            },
        );
    }
    {
        let (zaino, zebra) = (zaino.clone(), zebra.clone());
        follow.at_completion(Severity::Fatal).named("indexes_agree_with_zebra").check_rpc(
            move |_s, _cx| {
                let (zaino, zebra) = (zaino.clone(), zebra.clone());
                Box::pin(async move { indexes_agree_with_zebra(&zaino, &zebra).await })
            },
        );
    }

    let load: Arc<OnceLock<Arc<LoadRun>>> = Arc::new(OnceLock::new());
    let serve = {
        let (zebra, zaino, load) = (zebra.clone(), zaino.clone(), load.clone());
        run.then("serve", move |cx: SyncCtx| async move {
            let pod = cx.indexer_pod().ok_or("serve: no indexer pod bound")?.clone();
            let target = Target {
                uri: zaino.grpc_uri().await?,
                zebra: zebra.json_rpc().await?,
                server: Arc::new(PodCgroup::new(pod)),
                chain_name: "main".to_owned(),
            };
            let subject = LoadSubject::new(target, Plan::mainnet());
            let _ = load.set(subject.state());
            Ok(subject)
        })
    };
    serve.tick(TICK).timeout(SERVE_CAP);
    {
        let load = load.clone();
        serve.always(Severity::Fatal).named("served_answers_hold_to_zebra").each_tick().check(
            move |_s: &Snapshot| {
                let Some(state) = load.get() else {
                    return Verdict::Pending;
                };
                match state.violations().into_iter().next() {
                    None => Verdict::Satisfied,
                    Some(first) => violated(
                        first.height as u32,
                        format!("{} violations; first: {first}", state.violation_count()),
                    ),
                }
            },
        );
    }
    serve.at_completion(Severity::Fatal).named("no_restart").check(no_restart);
    {
        let load = load.clone();
        serve.at_completion(Severity::Fatal).named("load_ran_to_its_report").check(
            move |_s: &Snapshot| match load.get().and_then(|state| state.report()) {
                Some(Ok(_)) => Verdict::Satisfied,
                Some(Err(error)) => violated(0, format!("load run failed: {error}")),
                None => Verdict::ProbeError("serve completed without a load report".into()),
            },
        );
    }

    let account: Arc<OnceLock<AccountId>> = Arc::new(OnceLock::new());
    let phase = {
        let (zebra, zaino, wallet, account) =
            (zebra.clone(), zaino.clone(), wallet.clone(), account.clone());
        run.then("wallet", move |_cx: SyncCtx| async move {
            let birthday = BlockHeight::from_u32(WALLET.birthday);
            let opened = wallet.viewing_account(&zebra, &zaino, WALLET.ufvk, birthday).await?;
            let _ = account.set(opened.id());
            opened.wallet().sync_subject(opened.id())
        })
    };
    phase.tick(secs(10)).timeout(WALLET_CAP);

    phase.always(Severity::Fatal).named("scan_never_regresses").each_tick().check(
        |s: &Snapshot| {
            let (was, now) = (s.prev_height(), s.height());
            sync_ensure!(now >= was, "fully scanned height fell {was} -> {now}");
            Verdict::Satisfied
        },
    );
    phase.eventually(Severity::Fatal).named("wallet_advances").window(mins(20)).check(
        |s: &Snapshot| {
            if s.progressed_within(mins(20)) {
                Verdict::Satisfied
            } else {
                Verdict::Pending
            }
        },
    );
    {
        let zaino = zaino.clone();
        phase.at_completion(Severity::Fatal).named("wallet_reached_the_served_tip").check_rpc(
            move |s, _cx| {
                let zaino = zaino.clone();
                let scanned = s.height();
                Box::pin(async move { wallet_reached_the_served_tip(&zaino, scanned).await })
            },
        );
    }
    {
        let zebra = zebra.clone();
        phase.at_completion(Severity::Fatal).named("wallet_tree_roots_are_zebras").check_rpc(
            move |s, _cx| {
                let zebra = zebra.clone();
                let (at, roots) = (s.height(), s.tree_roots());
                Box::pin(async move { wallet_tree_roots_are_zebras(&zebra, at, roots).await })
            },
        );
    }
    phase.at_completion(Severity::Fatal).named("wallet_balances_are_the_truth").check(
        |s: &Snapshot| {
            let (got, want) = (s.balances(), WALLET.balances);
            sync_ensure!(got == want, "wallet balances {got:?}, expected {want:?}");
            Verdict::Satisfied
        },
    );
    {
        let (wallet, account) = (wallet.clone(), account.clone());
        phase.at_completion(Severity::Fatal).named("wallet_notes_are_the_truth").check_rpc(
            move |_s, _cx| {
                let (wallet, account) = (wallet.clone(), account.clone());
                Box::pin(async move { wallet_notes_are_the_truth(&wallet, &account).await })
            },
        );
    }
    {
        let (wallet, account) = (wallet.clone(), account.clone());
        phase.at_completion(Severity::Fatal).named("wallet_scanned_every_block").check_rpc(
            move |s, _cx| {
                let (wallet, account) = (wallet.clone(), account.clone());
                let at = s.height();
                Box::pin(async move { wallet_scanned_every_block(&wallet, &account, at) })
            },
        );
    }

    run.run().await
}

// ── phase `index`: safety ─────────────────────────────────────────────────────────────────────

/// Durable = survived an fsync → no reorg, reset or restart can take it back
async fn durable_extents_never_shrink(
    zaino: &ZainoIndexer,
    seen: &[AtomicU32; ZainoIndex::ALL.len()],
) -> Verdict {
    for (index, last) in ZainoIndex::ALL.into_iter().zip(seen) {
        let now = match zaino.finalized_height(index).await {
            Ok(Some(h)) => h,
            Ok(None) => continue,
            Err(e) => return Verdict::ProbeError(format!("{index:?} finalized: {e}")),
        };
        // +1: 0 = never published, distinct from a durable genesis
        let prev = last.fetch_max(now + 1, Ordering::Relaxed);
        let was = prev.saturating_sub(1);
        sync_ensure!(now + 1 >= prev, "{index:?} durable extent shrank {was} -> {now}");
    }
    Verdict::Satisfied
}

/// finalized(ix) ≤ max fetched ≤ best tip ≤ zebra's tip (+ motion slack per read)
///
/// - Max fetched, not current: a reorg rewinds the fetch cursor below a faster index's extent
async fn watermarks_ordered(
    zaino: &ZainoIndexer,
    zebra: &ZebraValidator,
    fetch_max: &AtomicU32,
) -> Verdict {
    let scrape = match zaino.read(SCRAPE).await {
        Ok(s) => s,
        Err(e) => return Verdict::ProbeError(format!("zaino /metrics: {e}")),
    };
    let (Some(fetched), Some(best)) =
        (scrape.level(family::FETCH_HEIGHT), scrape.level(family::BEST_TIP))
    else {
        return Verdict::Pending;
    };
    let (fetched, best) = (fetched as u32, best as u32);
    let fetched_max = fetch_max.fetch_max(fetched, Ordering::Relaxed).max(fetched);
    let tip = match zebra.chain_height().await {
        Ok(h) => u32::from(h),
        Err(e) => return Verdict::ProbeError(format!("zebra chain_height: {e}")),
    };

    for index in ZainoIndex::ALL {
        if let Some(finalized) = scrape.level(family::index_finalized_height(index)) {
            let durable = finalized as u32;
            sync_ensure!(durable <= fetched_max, "{index:?} durable {durable} > {fetched_max}");
        }
    }
    sync_ensure!(fetched <= best + CHAIN_MOTION_SLACK, "fetched {fetched} > best tip {best}");
    sync_ensure!(best <= tip + CHAIN_MOTION_SLACK, "best tip {best} ahead of zebra {tip}");
    Verdict::Satisfied
}

/// Tree-state's durable tip = final (below any reorg), so zaino's trees there == zebra's
async fn committed_tree_states_are_zebras(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    match zaino.finalized_height(ZainoIndex::TreeState).await {
        Ok(Some(tip)) => tree_states_agree(zaino, zebra, &[tip]).await,
        Ok(None) => Verdict::Satisfied,
        Err(e) => Verdict::ProbeError(format!("TreeState finalized: {e}")),
    }
}

/// synced = false ⇒ the index answers UNAVAILABLE, never a partial answer
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

/// Past the newest upgrade zebra reports active → its blocks went through every index
async fn past_newest_activation(zebra: &ZebraValidator, fetched: u32) -> Verdict {
    let info = match rpc(zebra, "getblockchaininfo", json!([])).await {
        Ok(i) => i,
        Err(v) => return v,
    };
    let newest = info["upgrades"]
        .as_object()
        .into_iter()
        .flat_map(|u| u.values())
        .filter(|u| u["status"].as_str() == Some("active"))
        .filter_map(|u| u["activationheight"].as_u64())
        .max()
        .unwrap_or(u64::MAX);
    if u64::from(fetched) > newest {
        Verdict::Satisfied
    } else {
        Verdict::Pending
    }
}

// ── phase `index`: terminal ───────────────────────────────────────────────────────────────────

/// Nothing here kills zaino → any restart = it crashed
fn no_restart(s: &Snapshot) -> Verdict {
    sync_ensure!(s.restarts() == 0, "zaino restarted {} times on its own", s.restarts());
    Verdict::Satisfied
}

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

/// zaino's target = zebra's tip → without this, "zaino at the tip" = "wherever zebra stalled"
async fn validator_at_network_tip(zebra: &ZebraValidator, at: u32) -> Verdict {
    let deadline = Instant::now() + NETWORK_CATCHUP_WINDOW;
    loop {
        let last = match zebra.blockchain_info().await {
            Ok(info) => match info.estimated_height.map(u32::from) {
                Some(estimate) if u32::from(info.blocks) + NETWORK_ESTIMATE_SLACK >= estimate => {
                    return Verdict::Satisfied
                }
                estimate => {
                    format!("zebra at {}, network estimate {estimate:?}", u32::from(info.blocks))
                }
            },
            Err(e) => format!("zebra getblockchaininfo: {e}"),
        };
        if Instant::now() >= deadline {
            return violated(at, format!("{NETWORK_CATCHUP_WINDOW:?} after completion, {last}"));
        }
        tokio::time::sleep(TERMINAL_POLL).await;
    }
}

/// Served tip within motion slack of zebra's; durable extent within the finality depth of it
async fn served_tip_is_the_validator_tip(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    let (served, finalized, tip) = match (
        zaino.latest_block_height().await,
        zaino.finalized_height(ZainoIndex::CompactBlock).await,
        zebra.chain_height().await,
    ) {
        (Ok(s), Ok(Some(f)), Ok(t)) => (u32::from(s), f, u32::from(t)),
        (s, f, t) => {
            return Verdict::ProbeError(format!(
                "served {:?}, finalized {:?}, zebra {:?}",
                s.map(u32::from).map_err(|e| e.to_string()),
                f.map_err(|e| e.to_string()),
                t.map(u32::from).map_err(|e| e.to_string())
            ))
        }
    };
    sync_ensure!(served <= tip, "zaino serves {served}, above zebra's tip {tip}");
    sync_ensure!(tip - served <= CHAIN_MOTION_SLACK, "zaino serves {served}, zebra at {tip}");
    let lag = tip - finalized;
    sync_ensure!(lag <= FINALISED_DEPTH + CHAIN_MOTION_SLACK, "durable {lag} behind tip {tip}");
    Verdict::Satisfied
}

/// Every served index vs zebra over one ladder (activations ±1, both ends, dense near the top)
///
/// - Top of the ladder < `FINALISED_DEPTH` below the tip → pre-commit answers, not only files
async fn indexes_agree_with_zebra(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    let top = match (zaino.latest_block_height().await, zebra.chain_height().await) {
        (Ok(s), Ok(t)) => u32::from(s).min(u32::from(t)).saturating_sub(REORG_MARGIN),
        (s, t) => return Verdict::ProbeError(format!("tips: zaino {s:?}, zebra {t:?}")),
    };
    let heights = match ladder(zebra, top).await {
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
        Ok(addresses) => transparent_agrees(zaino, zebra, &addresses, 12).await,
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
            format!("zainod verify: corruption\n{report:#}\nstderr: {}", output.stderr),
        );
    }
    for (index, floor) in floors {
        let heights = report[index.label()]["heights"].as_u64();
        if heights.is_none_or(|heights| heights <= u64::from(floor)) {
            return violated(
                floor,
                format!("verify saw {index:?} commit {heights:?} heights, durable reached {floor}"),
            );
        }
    }
    Verdict::Satisfied
}

// ── phase `follow` ────────────────────────────────────────────────────────────────────────────

/// Pre-commit serves each new block within motion slack of zebra, never ahead of it
async fn served_tip_tracks_zebra(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    let (served, tip) = match (zaino.latest_block_height().await, zebra.chain_height().await) {
        (Ok(s), Ok(t)) => (u32::from(s), u32::from(t)),
        (s, t) => return Verdict::ProbeError(format!("tips: zaino {s:?}, zebra {t:?}")),
    };
    sync_ensure!(served <= tip + CHAIN_MOTION_SLACK, "zaino serves {served}, zebra at {tip}");
    sync_ensure!(served + CHAIN_MOTION_SLACK >= tip, "zaino serves {served} < zebra {tip}");
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

/// Zebra's wire expectations (`ztest::loadtest::reference`, the serve phase's oracle too)
async fn reference(zebra: &ZebraValidator) -> Result<Zebra, Verdict> {
    zebra
        .json_rpc()
        .await
        .map(Zebra::new)
        .map_err(|e| Verdict::ProbeError(format!("zebra json_rpc: {e}")))
}

/// `GetTreeState` byte-identical to `z_gettreestate`: height, hash, time, all three trees
async fn tree_states_agree(
    zaino: &ZainoIndexer,
    zebra: &ZebraValidator,
    heights: &[u32],
) -> Verdict {
    let reference = match reference(zebra).await {
        Ok(r) => r,
        Err(v) => return v,
    };
    for &height in heights {
        let served = match zaino.get_tree_state(BlockHeight::from_u32(height)).await {
            Ok(t) => t,
            Err(e) => return violated(height, format!("zaino GetTreeState({height}): {e}")),
        };
        let truth = match reference.tree_state(height).await {
            Ok(t) => t,
            Err(e) => return Verdict::ProbeError(format!("zebra z_gettreestate({height}): {e}")),
        };
        if let Some(diff) = tree_state_diff(&truth, &served) {
            return violated(height, format!("tree state at {height}: {diff}"));
        }
    }
    Verdict::Satisfied
}

/// Whole `GetSubtreeRoots` list per pool == `z_getsubtreesbyindex` (root, completing height and
/// hash)
///
/// - One side answering while the other refuses = violation; both refusing = no disagreement
async fn subtree_roots_agree(zaino: &ZainoIndexer, zebra: &ZebraValidator) -> Verdict {
    let reference = match reference(zebra).await {
        Ok(r) => r,
        Err(v) => return v,
    };
    for (protocol, pool) in [
        (ShieldedProtocol::Sapling, "sapling"),
        (ShieldedProtocol::Orchard, "orchard"),
        (ShieldedProtocol::Ironwood, "ironwood"),
    ] {
        let (served, truth) = match (
            zaino.get_subtree_roots(0, protocol, 0).await,
            reference.subtree_roots(pool).await,
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
                return violated(
                    0,
                    format!("{pool} subtrees: zebra has {}, zaino refused: {e}", t.len()),
                )
            }
        };
        let (ours, theirs) = (served.len(), truth.len());
        sync_ensure!(ours == theirs, "{pool} subtrees: zaino {ours}, zebra {theirs}");
        if let Some((index, (s, t))) =
            served.iter().zip(&truth).enumerate().find(|(_, (s, t))| s != t)
        {
            return violated(
                t.completing_block_height as u32,
                format!("{pool} subtree {index}\n  zaino: {s:?}\n  zebra: {t:?}"),
            );
        }
    }
    Verdict::Satisfied
}

/// `GetBlock` (all pools) field-for-field == the compact block zebra's `getblock 2` implies,
/// every fee recomputed from its prevouts
async fn blocks_agree(zaino: &ZainoIndexer, zebra: &ZebraValidator, heights: &[u32]) -> Verdict {
    let reference = match reference(zebra).await {
        Ok(r) => r,
        Err(v) => return v,
    };
    for &height in heights {
        let served = match zaino.get_block(BlockHeight::from_u32(height)).await {
            Ok(b) => b,
            Err(e) => return violated(height, format!("zaino GetBlock({height}): {e}")),
        };
        let truth = match reference.compact_block(height, Fees::Checked).await {
            Ok(b) => b,
            Err(e) => return Verdict::ProbeError(format!("zebra getblock({height}): {e}")),
        };
        if let Some(diff) = block_diff(&truth, &served) {
            return violated(height, format!("block {height}: {diff}"));
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

/// Balance + whole UTXO set per address == `getaddressbalance` / `getaddressutxos`
///
/// - Judged only across a window where both tips held still (a live address can gain an output
///   between the two reads); `attempts` windows before giving up
async fn transparent_agrees(
    zaino: &ZainoIndexer,
    zebra: &ZebraValidator,
    addresses: &[String],
    attempts: u32,
) -> Verdict {
    if addresses.is_empty() {
        return Verdict::ProbeError("no coinbase addresses to compare".into());
    }
    let tips = || async {
        (
            zaino.latest_block_height().await.ok().map(u32::from),
            zebra.chain_height().await.ok().map(u32::from),
        )
    };
    for _ in 0..attempts {
        let before = tips().await;
        if before.0.is_none() || before.0 != before.1 {
            tokio::time::sleep(secs(5)).await;
            continue;
        }
        let mut first = None;
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
                first.get_or_insert(format!(
                    "{address}: balance zaino {served_zat} zebra {truth_zat}; utxos zaino {} zebra {}{differing}",
                    served.len(),
                    truth.len()
                ));
            }
        }
        if tips().await != before {
            continue;
        }
        return match first {
            None => Verdict::Satisfied,
            Some(detail) => violated(before.1.unwrap_or(0), detail),
        };
    }
    Verdict::ProbeError(format!(
        "no still window for the transparent comparison in {attempts} tries"
    ))
}

// ── phase `wallet` ────────────────────────────────────────────────────────────────────────────

async fn wallet_reached_the_served_tip(zaino: &ZainoIndexer, scanned: u32) -> Verdict {
    match zaino.latest_block_height().await {
        Ok(tip) => {
            let tip = u32::from(tip);
            sync_ensure!(scanned + CHAIN_MOTION_SLACK >= tip, "scanned {scanned}, serves {tip}");
            Verdict::Satisfied
        }
        Err(e) => Verdict::ProbeError(format!("zaino GetLatestBlock: {e}")),
    }
}

/// Wallet's shard-tree roots at its scanned height == roots of zebra's trees there
///
/// - Any output missing, extra or misordered in a served compact block moves a root
async fn wallet_tree_roots_are_zebras(
    zebra: &ZebraValidator,
    at: u32,
    roots: TreeRoots,
) -> Verdict {
    let truth = match rpc(zebra, "z_gettreestate", json!([at.to_string()])).await {
        Ok(t) => t,
        Err(v) => return v,
    };
    for (pool, key) in
        [(Pool::Sapling, "sapling"), (Pool::Orchard, "orchard"), (Pool::Ironwood, "ironwood")]
    {
        let frontier = truth[key]["commitments"]["finalState"].as_str().unwrap_or("");
        let expected = match commitment_tree_root(pool, frontier) {
            Ok(r) => r,
            Err(e) => return Verdict::ProbeError(format!("zebra {key} tree at {at}: {e}")),
        };
        let wallet = roots.require(pool);
        sync_ensure!(wallet == expected, "{key} at {at}: wallet {wallet:?}, zebra {expected:?}");
    }
    Verdict::Satisfied
}

async fn wallet_notes_are_the_truth(
    wallet: &ZingolibWallet,
    account: &OnceLock<AccountId>,
) -> Verdict {
    let Some(&account) = account.get() else {
        return Verdict::ProbeError("phase `wallet` never opened its account".into());
    };
    match wallet.unspent_notes(account).await {
        Ok(n) => {
            let counted = [n.sapling, n.orchard, n.ironwood, n.transparent];
            let want = WALLET.notes;
            sync_ensure!(counted == want, "unspent notes {counted:?}, expected {want:?}");
            Verdict::Satisfied
        }
        Err(e) => Verdict::ProbeError(format!("unspent_notes: {e}")),
    }
}

/// Anti-vacuity: a "complete" sync that skipped blocks never trial-decrypted them
fn wallet_scanned_every_block(
    wallet: &ZingolibWallet,
    account: &OnceLock<AccountId>,
    at: u32,
) -> Verdict {
    let Some(&account) = account.get() else {
        return Verdict::ProbeError("phase `wallet` never opened its account".into());
    };
    let totals = match wallet.last_sync(account) {
        Ok(Some(t)) => t,
        Ok(None) => return Verdict::ProbeError("pepper-sync recorded no scan totals".into()),
        Err(e) => return Verdict::ProbeError(format!("last_sync: {e}")),
    };
    let (start, end, scanned) = (totals.start_height, totals.end_height, totals.blocks_scanned);
    let birthday = WALLET.birthday;
    sync_ensure!(start <= birthday + 1, "scan started at {start}, birthday {birthday}");
    sync_ensure!(end >= at, "scan ended at {end}, wallet reports {at}");
    sync_ensure!(scanned == end - start + 1, "scanned {scanned} blocks over [{start}, {end}]");
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
