//! The run loop over real watches, the `/statusz` + `/metrics` goldens, `check()`'s fire drills

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chainview::{ChainView, ChainViewSnapshot, EndpointSet};
use zaino_header_chain::testing::{insert, HeaderViews};
use zaino_header_chain::VerifiedChain;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_nfs::{ChainParams, Indexed};
use zaino_persistence::{
    fs::SimFs, DiskEngine, DiskView, IndexKind, PersistenceEngine, Schema, Store,
};
use zaino_primitives::testing::{h, MockChain};
use zaino_primitives::types::{Block, BlockRef, Height, ReorgDepth, TransactionId};
use zaino_source::testing::{within, ValidatorViews};
use zaino_sync::SyncProgress;
use zaino_traffic::{
    Health, Limits, MemberId, MemberRow, MemberTable, TrafficBalancer, Trusted, ValidatorId,
};
use zcash_protocol::consensus::NetworkType;

use crate::compose::check;
use crate::feed::Feed;
use crate::publisher::Core;
use crate::{
    describe_metrics, emit_gauges, notes, Down, Publisher, Report, Snapshot, SnapshotError,
    Snapshots, Standing, Tips,
};

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));

fn at(block: &Block) -> BlockRef {
    BlockRef { hash: block.header().hash, height: block.header().height }
}

/// - Seq 0 stored at `new` (nothing verified, nothing served)
/// - A chain view publish → seq 1 under the new chain
/// - Three NFS publishes before the publisher runs → one publish, from the last (coalesced)
/// - Cancel → `Ok`; the NFS's watch dropped → `IndexedGone`
#[tokio::test]
async fn the_publisher_follows_both_watches_coalesces_and_stops_on_cancel_or_a_gone_nfs() {
    let mut builder = MockChain::regtest();
    let a3 = builder.mine_empty(3);
    let path = builder.blocks(a3);
    let params = ChainParams::of(&builder);
    let source = Arc::new(builder.validator(a3));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let (balancer, _never_driven) =
        TrafficBalancer::new(vec![Trusted { source, priority: 0, limits }], None);
    let address = vec!["10.0.0.1:8232".to_owned()];
    let schedule = builder.schedule().upgrades;
    let view = ChainView::new(address, balancer, DEPTH, schedule).expect("one validator");
    let (nfs, indexed) = watch::channel(None);
    let publisher = Publisher::<DiskView>::new(indexed, view.subscriber(), DEPTH);
    let mut snapshots = publisher.handle();
    assert_eq!((snapshots.load().seq(), snapshots.load().tips()), (0, Tips::default()));

    let cancel = CancellationToken::new();
    let running = tokio::spawn(publisher.run(cancel.clone()));
    let chain = builder.verified(a3);
    view.set_verified(Some(chain.clone()));
    within("a publish", snapshots.changed()).await.expect("publisher running");
    let snap = snapshots.load();
    assert_eq!((snap.seq(), snap.tips().best, snap.tips().served), (1, Some(a3), None));

    let none: [(IndexKind, DiskView); 0] = [];
    for block in &path[1..] {
        let indexed = Indexed::fixed(Arc::new(chain.clone()), at(block), params, none.clone());
        nfs.send_replace(Some(Arc::new(indexed)));
    }
    within("a publish", snapshots.changed()).await.expect("publisher running");
    let snap = snapshots.load();
    let tips = snap.tips();
    assert_eq!((snap.seq(), tips.served, tips.synced), (2, Some(a3), true), "coalesced burst");

    cancel.cancel();
    let ended = within("the publisher stopped", running).await;
    assert!(matches!(ended.expect("task"), Ok(())), "cancel = clean stop");
    let gone = Publisher::<DiskView>::new(nfs.subscribe(), view.subscriber(), DEPTH);
    drop(nfs);
    let stopped = within("the publisher stopped", gone.run(CancellationToken::new())).await;
    assert!(matches!(stopped, Err(SnapshotError::IndexedGone)), "{stopped:?}");
}

/// A 0..=5 final through 2, side S4..=S5 off A3; one of two validators holds A5 (the balancer:
/// one live at 12 ms, one degraded after 3 failures); block-hash durable through A5, tree-state
/// configured but absent from the NFS (enabled, no durable), served A5, handed 5; one relayed tx:
/// the `/statusz` body field by field, then the gauges a scrape renders from the same snapshot
#[test]
fn one_snapshot_renders_the_status_report_and_every_gauge() {
    let mut builder = MockChain::regtest();
    let a5 = builder.mine_empty(5);
    let a = builder.blocks(a5);
    let s5 = builder.fork(h(3)).mine_empty(2).tip();
    let s = builder.blocks(s5);
    let mut headers = builder.header_chain(DEPTH);
    insert(&mut headers, &a).expect("A verifies");
    insert(&mut headers, &s[4..]).expect("S verifies");
    headers.finalize(at(&a[2]));
    let chain = Arc::new(headers.verified().expect("verified"));

    let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
    let schema = Schema::new(
        IndexKind::BlockHash,
        block_hash::FORMAT,
        NetworkType::Regtest,
        block_hash::TABLES,
    );
    let mut store =
        engine.open(Path::new("block_hash"), &schema, NonZeroUsize::MAX).expect("fresh store");
    for block in &a {
        let mut out = store.changes(block.at());
        block_hash::fold(&block_hash::BlockHashReader::new(store.staged()), block, &mut out);
        store.apply(out);
    }
    store.commit().expect("SimFs commit");
    let durable = [(IndexKind::BlockHash, store.committed())];
    let indexed = Indexed::fixed(Arc::clone(&chain), a5, ChainParams::of(&builder), durable);

    let relayed = TransactionId::from([1; 32]);
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let view = ChainViewSnapshot::fixed(
        Some(Arc::clone(&chain)),
        EndpointSet::at([0]),
        &addresses,
        &[(relayed, Bytes::from_static(b"relayed"))],
    );
    let snap = Snapshots::fixed(Some(Arc::new(indexed)), Arc::new(view)).load();
    let progress = SyncProgress::fixed(Some(Height::try_from(5u32).expect("h")));
    let member = |at: usize, health, failures, latency| MemberRow {
        id: MemberId::Trusted(ValidatorId::new(at).expect("small")),
        health,
        failures,
        benched_until: None,
        latency: Duration::from_millis(latency),
        in_flight: 0,
    };
    let members = MemberTable {
        rows: vec![member(0, Health::Live, 0, 12), member(1, Health::Degraded, 3, 40)],
    };

    let block = |block: &Block| {
        let at = at(block);
        serde_json::json!({ "height": u32::from(at.height), "hash": at.hash.to_string() })
    };
    let work = chain.forks()[0].cumulative_work.to_string();
    let disabled =
        |name: &str| serde_json::json!({ "name": name, "enabled": false, "durable": null });
    let validator =
        |address: &str, state: &str, latency_ms: u64, failures: u32, at: Option<u32>| {
            serde_json::json!({
                "address": address, "state": state, "height": at,
                "agreement": if at.is_some() { "agreed" } else { "unknown" },
                "stale_blocks": at.map(|_| 0), "latency_ms": latency_ms, "failures": failures,
                "observed_s_ago": null, "streaming": false, "release": null, "peers": [],
            })
        };
    let expected = serde_json::json!({
        "seq": 0,
        "tips": {
            "best": block(&a[5]), "final": block(&a[2]), "served": block(&a[5]),
            "held_by": 1, "configured": 2, "synced": true,
        },
        "handed": 5,
        "indexes": [
            disabled("value_balance"),
            disabled("compact_block"),
            { "name": "block_hash", "enabled": true, "durable": 5 },
            { "name": "tree_state", "enabled": true, "durable": null },
            disabled("transparent_address"),
        ],
        "validators": [
            validator(addresses[0], "live", 12, 0, Some(5)),
            validator(addresses[1], "degraded", 40, 3, None),
        ],
        "alarms": {
            "partitioned": false, "eclipsed": false, "finality_paused": false,
            "stale": [], "ending": [],
        },
        "mempool": {
            "transactions": 1, "verified": 0, "quorum": 0, "ours_unverified": 1, "fully_spread": 0,
            "trusted_readers": 1,
        },
        "forks": [
            { "from": block(&a[3]), "tip": block(&s[5]), "cumulative_work": work, "folded": null },
        ],
    });
    let enabled = [IndexKind::BlockHash, IndexKind::TreeState];
    let report = Report::of(&snap, &progress, &members, &enabled);
    let report = serde_json::to_value(report).expect("serializes");
    assert_eq!(report, expected);

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let render = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        describe_metrics();
        emit_gauges(&snap, &progress);
    });
    let rendered = render.render();
    let first = addresses[0];
    for line in [
        "zaino_best_tip 5".to_owned(),
        "zaino_fetch_height 5".to_owned(),
        "zaino_index_synced{index=\"block_hash\"} 1".to_owned(),
        "zaino_chainview_best_height 5".to_owned(),
        "zaino_chainview_tip_holders 1".to_owned(),
        "zaino_chainview_finality_paused 0".to_owned(),
        "zaino_chainview_mempool_transactions{state=\"verified\"} 0".to_owned(),
        "zaino_chainview_mempool_transactions{state=\"quorum\"} 0".to_owned(),
        "zaino_chainview_mempool_transactions{state=\"ours_unverified\"} 1".to_owned(),
        format!("zaino_chainview_endpoint_state{{endpoint=\"{first}\",state=\"live\"}} 1"),
        format!("zaino_chainview_agreement{{endpoint=\"{first}\",agreement=\"agreed\"}} 1"),
        format!("zaino_chainview_push_stream{{endpoint=\"{first}\"}} 0"),
        format!("zaino_chainview_tip_height{{endpoint=\"{first}\"}} 5"),
    ] {
        assert!(rendered.lines().any(|at| at == line), "{line:?} not in:\n{rendered}");
    }
}

/// Validator back from an outage, catching up 1,200 blocks behind the network, its tip moving in
/// jumps the NFS catches between: every publish serves best yet stays `Syncing` toward the
/// validator's estimate (0.10.1 flapped ready ↔ syncing here); then each standing the holder's
/// health and the endpoints' give, with the notes their alarms raise; with no trusted validator
/// vouching for best (all down, or none holding it): degraded while best's time is within 30 min
/// of the clock, down past that
#[test]
fn a_tip_held_only_by_a_catching_up_validator_is_syncing_never_serving() {
    let mut builder = MockChain::regtest();
    let a12 = builder.mine_empty(12);
    let a = builder.blocks(a12);
    let now = i64::from(a[12].header().time);
    let stale = now + 30 * 60;
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let none: [(IndexKind, DiskView); 0] = [];
    let served_at = |tip: &Block| {
        let chain = Arc::new(builder.verified(at(tip)));
        let params = ChainParams::of(&builder);
        let indexed = Indexed::fixed(Arc::clone(&chain), at(tip), params, none.clone());
        let view = ChainViewSnapshot::fixed(Some(chain), EndpointSet::at([0]), &addresses, &[]);
        (Arc::new(indexed), view)
    };

    let (indexed, view) = served_at(&a[4]);
    let core =
        Core::new(Some(indexed), Arc::new(view.endpoint(0, Health::CatchingUp, 1_200)), DEPTH);
    let snapshots = core.handle();
    for tip in [&a[4], &a[8], &a[12]] {
        let (indexed, view) = served_at(tip);
        core.publish(Some(indexed), Arc::new(view.endpoint(0, Health::CatchingUp, 1_200)));
        let snap = snapshots.load();
        let height = u32::from(tip.header().height);
        assert_eq!(snap.tips().served, snap.tips().best, "at {height}: served = best");
        let target = snap.sync_target().map(u32::from);
        assert_eq!(
            (snap.standing(now), target),
            (Standing::Syncing, Some(height + 1_200)),
            "{height}"
        );
    }

    let cases = [
        ("caught up", Health::Live, 0, Health::Pending, Standing::Serving, vec![]),
        ("one failed poll", Health::Degraded, 0, Health::Pending, Standing::Serving, vec![]),
        (
            "holder stale",
            Health::Live,
            30,
            Health::Pending,
            Standing::Serving,
            vec!["1 validator on a stale tip"],
        ),
        (
            "every validator down",
            Health::Down,
            0,
            Health::Down,
            Standing::Degraded(Down::NoValidatorReachable),
            vec![],
        ),
    ];
    for (case, holder, behind, other, standing, expected_notes) in cases {
        let (indexed, view) = served_at(&a[12]);
        let view = view.endpoint(0, holder, behind).endpoint(1, other, 0);
        let snap = Snapshots::fixed(Some(indexed), Arc::new(view)).load();
        assert_eq!(snap.standing(now), standing, "{case}");
        assert_eq!(notes(&snap), expected_notes, "{case}");
    }
    let (indexed, view) = served_at(&a[12]);
    let all_down = view.endpoint(0, Health::Down, 0).endpoint(1, Health::Down, 0);
    let snap = Snapshots::fixed(Some(indexed), Arc::new(all_down)).load();
    let down = Standing::Down(Down::NoValidatorReachable);
    assert_eq!(snap.standing(stale), down, "every validator down, best stale");

    let (indexed, _) = served_at(&a[12]);
    let unheld = ChainViewSnapshot::fixed(
        Some(Arc::new(builder.verified(at(&a[12])))),
        EndpointSet::default(),
        &addresses,
        &[],
    );
    let snap = Snapshots::fixed(Some(indexed), Arc::new(unheld)).load();
    let unheld = (snap.standing(now), snap.standing(stale - 1), snap.standing(stale));
    let degraded = Standing::Degraded(Down::TipNotHeld);
    let expected = (degraded, degraded, Standing::Down(Down::TipNotHeld));
    assert_eq!(unheld, expected, "no holder of best: fresh through 30 min − 1 s, stale at 30 min");
}

/// `check()` fire drills: A 0..=6 (A6 mines tx M) + S5 off A4, both validators hold A6
struct Drill {
    a: Vec<Arc<Block>>,
    s5: BlockRef,
    mined: TransactionId,
    chain: Arc<VerifiedChain>,
    params: ChainParams,
    prev: Arc<Snapshot<DiskView>>,
    next: Arc<Snapshot<DiskView>>,
}

impl Drill {
    /// `prev` = seq 0 serving A3 (not synced), `next` = seq 1 serving A6 (synced, epoch rotated)
    fn new() -> Self {
        let mut builder = MockChain::regtest();
        builder.mine_empty(5);
        let a6 = builder.mine(|block| block.tx(|tx| tx.txid([0x66; 32])));
        let a = builder.blocks(a6);
        let s5 = builder.fork(h(4)).mine_empty(1).tip();
        let chain = Arc::new(builder.verified(a6));
        let params = ChainParams::of(&builder);
        let view = Arc::new(view(&chain, &[]));
        let core = Core::new(served(&chain, params, at(&a[3])), Arc::clone(&view), DEPTH);
        let prev = core.handle().load();
        core.publish(served(&chain, params, at(&a[6])), view);
        let next = core.handle().load();
        Self { a, s5, mined: TransactionId::from([0x66; 32]), chain, params, prev, next }
    }

    /// `next`, serving `block`
    fn serving(&self, block: BlockRef, synced: bool) -> Snapshot<DiskView> {
        let mut planted = copy(&self.next);
        planted.indexed = served(&self.chain, self.params, block);
        planted.tips = Tips { served: Some(block), synced, ..self.next.tips };
        planted
    }

    fn synced_prev(&self) -> Snapshot<DiskView> {
        Snapshot { tips: Tips { synced: true, ..self.prev.tips }, ..copy(&self.prev) }
    }
}

/// Both validators holding `chain`'s best, `mempool` listed
fn view(chain: &Arc<VerifiedChain>, mempool: &[(TransactionId, Bytes)]) -> ChainViewSnapshot {
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    ChainViewSnapshot::fixed(Some(Arc::clone(chain)), EndpointSet::at([0, 1]), &addresses, mempool)
}

/// Index views serving `block` (no tables)
fn served(
    chain: &Arc<VerifiedChain>,
    params: ChainParams,
    block: BlockRef,
) -> Option<Arc<Indexed<DiskView>>> {
    let none: [(IndexKind, DiskView); 0] = [];
    Some(Arc::new(Indexed::fixed(Arc::clone(chain), block, params, none)))
}

fn copy(s: &Snapshot<DiskView>) -> Snapshot<DiskView> {
    Snapshot {
        seq: s.seq,
        tips: s.tips,
        indexed: s.indexed.clone(),
        view: Arc::clone(&s.view),
        feed: s.feed.clone(),
    }
}

fn fresh(key: BlockRef) -> Feed {
    Feed::open(Some(key), Vec::new(), HashSet::new(), watch::Sender::new(()).subscribe())
}

/// G6 = the same view as G3's `held_by`: no separate plant
#[test]
fn the_unplanted_pair_passes_every_check() {
    let d = Drill::new();
    check(&d.prev, &d.next, DEPTH);
}

#[test]
#[should_panic(expected = "G2: seq + 1")]
fn check_catches_a_skipped_seq() {
    let d = Drill::new();
    check(&d.prev, &Snapshot { seq: 5, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: best")]
fn check_catches_a_best_off_the_chain() {
    let d = Drill::new();
    let tips = Tips { best: None, ..d.next.tips };
    check(&d.prev, &Snapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: final")]
fn check_catches_a_final_tip_off_the_chain() {
    let d = Drill::new();
    let tips = Tips { final_tip: Some(at(&d.a[1])), ..d.next.tips };
    check(&d.prev, &Snapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: held_by")]
fn check_catches_holders_off_the_view() {
    let d = Drill::new();
    let tips = Tips { held_by: EndpointSet::default(), ..d.next.tips };
    check(&d.prev, &Snapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: served")]
fn check_catches_a_served_tip_off_the_indexes() {
    let d = Drill::new();
    let tips = Tips { served: Some(at(&d.a[5])), ..d.next.tips };
    check(&d.prev, &Snapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G4: synced opens only at served = best")]
fn check_catches_synced_opening_below_best() {
    let d = Drill::new();
    check(&d.prev, &d.serving(at(&d.a[5]), true), DEPTH);
}

/// Boot race: validator 0's claim past best, its hash at best not polled yet
#[test]
#[should_panic(expected = "no validator claiming past it")]
fn check_catches_synced_opening_under_a_validator_claiming_past_best() {
    let d = Drill::new();
    let mut planted = d.serving(at(&d.a[6]), true);
    let view = view(&d.chain, &[]).ahead(0, 1, false);
    planted.tips.held_by = view.held_by();
    planted.view = Arc::new(view);
    check(&d.prev, &planted, DEPTH);
}

#[test]
#[should_panic(expected = "G4: synced stays on best")]
fn check_catches_synced_serving_a_side_branch() {
    let d = Drill::new();
    check(&d.synced_prev(), &d.serving(d.s5, true), DEPTH);
}

#[test]
#[should_panic(expected = "G4: synced stays within")]
fn check_catches_synced_falling_behind_its_window() {
    let d = Drill::new();
    check(&d.synced_prev(), &d.serving(at(&d.a[2]), true), DEPTH);
}

#[test]
#[should_panic(expected = "G5: epoch key")]
fn check_catches_an_epoch_keyed_off_the_served_tip() {
    let d = Drill::new();
    check(&d.prev, &Snapshot { feed: fresh(at(&d.a[3])), ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G5: epoch rotates iff")]
fn check_catches_an_epoch_rotating_without_a_move() {
    let d = Drill::new();
    let again = Snapshot { seq: 2, feed: fresh(at(&d.a[6])), ..copy(&d.next) };
    check(&d.next, &again, DEPTH);
}

#[test]
#[should_panic(expected = "G5: the old epoch sealed iff rotated")]
fn check_catches_an_old_epoch_left_open() {
    let d = Drill::new();
    check(&Snapshot { feed: fresh(at(&d.a[3])), ..copy(&d.prev) }, &copy(&d.next), DEPTH);
}

#[test]
#[should_panic(expected = "G5: the stored epoch open")]
fn check_catches_a_sealed_stored_epoch() {
    let d = Drill::new();
    let feed = fresh(at(&d.a[6]));
    feed.seal();
    check(&d.prev, &Snapshot { feed, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G10: the epoch carries a tx of the served tip's own chain")]
fn check_catches_an_epoch_carrying_a_tx_its_served_tip_mined() {
    let d = Drill::new();
    let none: [(IndexKind, DiskView); 0] = [];
    let indexed =
        Indexed::fixed_above(Arc::clone(&d.chain), at(&d.a[5]), &d.a[6..], d.params, none);
    let stale = view(&d.chain, &[(d.mined, Bytes::from_static(&[0x66]))]);
    let wake = watch::Sender::new(()).subscribe();
    let feed = Feed::open(Some(at(&d.a[6])), stale.arrivals(None), HashSet::new(), wake);
    let planted = Snapshot { indexed: Some(Arc::new(indexed)), feed, ..copy(&d.next) };
    check(&d.prev, &planted, DEPTH);
}
