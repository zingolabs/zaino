//! The run loop over real watches, the `/metrics` golden, serving status and the mempool gate,
//! `check()`'s fire drills

use std::collections::HashSet;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chainview::{ChainView, ChainViewSnapshot, TrustedValidatorSet};
use zaino_header_chain::testing::{insert, insert_unwitnessed};
use zaino_header_chain::{HeaderChain, HeaderChainSnapshot};
use zaino_nfs::{ChainParams, NfsPublication};
use zaino_persistence::{fs::SimFs, DiskEngine, DiskView, IndexKind, PersistenceEngine, Store};
use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::{h, within, MockChain};
use zaino_sync::SyncProgress;
use zaino_traffic::{Health, Limits, TrafficBalancer, TrustedValidator};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::TxId;

use crate::invariants::check;
use crate::mempool_feed::Feed;
use crate::publisher::Core;
use crate::{
    emit_gauges, notes, GrpcSnapshot, GrpcSnapshotError, GrpcSnapshots, Publisher, ServingStatus,
    Tips, Unavailable, METRICS,
};

const DEPTH: FinalityDepth = FinalityDepth::new(NonZeroU32::new(3).expect("nonzero"));

/// - Seq 0 stored at `new` (nothing verified, nothing served)
/// - A chain view publish → seq 1 under the new chain
/// - Three NFS publishes (A1, A2, A3) before the publisher wakes → one publish, from A3
#[tokio::test]
async fn the_publisher_stores_seq_0_then_one_publish_per_change_coalescing_bursts() {
    let mut builder = MockChain::regtest();
    let a3 = builder.mine_empty(3);
    let path = builder.blocks(a3);
    let params = ChainParams::of(&builder);
    let source = Arc::new(builder.validator(a3).client());
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let address = "10.0.0.1:8232".to_owned();
    let trusted = TrustedValidator { address, source, priority: 0, limits };
    let (balancer, _never_driven) = TrafficBalancer::new(vec![trusted], None);
    let schedule = builder.schedule().upgrades;
    let view = ChainView::new(balancer, DEPTH, schedule, CancellationToken::new());
    let (nfs, indexed) = watch::channel(None);
    let publisher = Publisher::<DiskView>::new(indexed, view.subscriber(), DEPTH);
    let mut grpc_snapshots = publisher.handle();
    let first = grpc_snapshots.load();
    assert_eq!((first.seq(), first.tips()), (0, Tips::default()), "(seq, tips) at new");
    tokio::spawn(publisher.run(CancellationToken::new()));

    let chain = HeaderChainSnapshot::of(&builder, a3);
    view.set_chain(Some(chain.clone()));
    within("a publish", grpc_snapshots.changed()).await.expect("publisher running");
    let grpc_snapshot = grpc_snapshots.load();
    let tips = grpc_snapshot.tips();
    let published = (grpc_snapshot.seq(), tips.best, tips.served);
    assert_eq!(published, (1, Some(a3), None), "chain view published: (seq, best, served)");

    let none: [(IndexKind, DiskView); 0] = [];
    for block in &path[1..] {
        let indexed =
            NfsPublication::fixed(Arc::new(chain.clone()), block.block_ref(), params, none.clone());
        nfs.send_replace(Some(Arc::new(indexed)));
    }
    within("a publish", grpc_snapshots.changed()).await.expect("publisher running");
    let grpc_snapshot = grpc_snapshots.load();
    let tips = grpc_snapshot.tips();
    let published = (grpc_snapshot.seq(), tips.served, tips.synced);
    assert_eq!(published, (2, Some(a3), true), "A1..=A3 coalesced: (seq, served, synced)");
}

#[tokio::test]
async fn the_publisher_stops_ok_on_cancel_and_as_indexed_gone_once_the_nfs_drops() {
    let mut builder = MockChain::regtest();
    let a3 = builder.mine_empty(3);
    let source = Arc::new(builder.validator(a3).client());
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let address = "10.0.0.1:8232".to_owned();
    let trusted = TrustedValidator { address, source, priority: 0, limits };
    let (balancer, _never_driven) = TrafficBalancer::new(vec![trusted], None);
    let schedule = builder.schedule().upgrades;
    let view = ChainView::new(balancer, DEPTH, schedule, CancellationToken::new());
    let (nfs, indexed) = watch::channel(None);

    let cancel = CancellationToken::new();
    let cancelled = Publisher::<DiskView>::new(indexed, view.subscriber(), DEPTH);
    let running = tokio::spawn(cancelled.run(cancel.clone()));
    cancel.cancel();
    let ended = within("the publisher stopped", running).await.expect("publisher task");
    assert!(ended.is_ok(), "cancel → Ok, got {ended:?}");

    let orphaned = Publisher::<DiskView>::new(nfs.subscribe(), view.subscriber(), DEPTH);
    drop(nfs);
    let stopped = within("the publisher stopped", orphaned.run(CancellationToken::new())).await;
    let gone = matches!(stopped, Err(GrpcSnapshotError::IndexedGone));
    assert!(gone, "NFS watch dropped → Err(IndexedGone), got {stopped:?}");
}

/// Gauges a scrape renders, given:
///
/// - A 0..=5 finalized through 2, one of two validators holding A5, the other degraded
/// - block-hash durable through A5, served A5, handed 5, one relayed tx
#[test]
fn one_snapshot_renders_every_gauge() {
    let mut builder = MockChain::regtest();
    let a5 = builder.mine_empty(5);
    let a = builder.blocks(a5);
    let mut headers = HeaderChain::anchored(&builder, DEPTH);
    insert(&mut headers, &a).expect("A verifies");
    headers.finalize(a[2].block_ref());
    let chain = Arc::new(headers.snapshot().expect("verified"));

    let engine = DiskEngine::new(SimFs::new(), zaino_persistence::LsmConfig::default());
    let schema = zaino_internal_block_hash_to_height::schema(NetworkType::Regtest);
    let mut store =
        engine.open(Path::new("block_hash"), &schema, NonZeroUsize::MAX).expect("fresh store");
    for block in &a {
        let mut out = store.changes(block.block_ref());
        let parent = zaino_internal_block_hash_to_height::BlockHashReader::new(store.applied());
        zaino_internal_block_hash_to_height::fold(&parent, block, &mut out);
        store.apply(out);
    }
    store.commit().expect("SimFs commit");
    let durable = [(IndexKind::BlockHash, store.durable())];
    let indexed = NfsPublication::fixed(Arc::clone(&chain), a5, ChainParams::of(&builder), durable);

    let relayed = TxId::from_bytes([1; 32]);
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let view = ChainViewSnapshot::fixed(
        Some(Arc::clone(&chain)),
        TrustedValidatorSet::at([0]),
        &addresses,
        &[(relayed, Bytes::from_static(b"relayed"))],
    );
    let view = view.trusted_validator(1, Health::Degraded, 0);
    let grpc_snapshot = GrpcSnapshots::fixed(Some(Arc::new(indexed)), Arc::new(view)).load();
    let progress = SyncProgress::fixed(Some(BlockHeight::from(5)));

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let render = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        METRICS.iter().for_each(|metric| metric.describe());
        emit_gauges(&grpc_snapshot, &progress);
    });
    let rendered = render.render();
    let first = addresses[0];
    for line in [
        "zaino_best_tip 5".to_owned(),
        "zaino_fetch_height 5".to_owned(),
        "zaino_index_synced{index=\"block_hash\"} 1".to_owned(),
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
        let rendered_line = rendered.lines().any(|at| at == line);
        assert!(rendered_line, "gauge from one snapshot (G9): {line:?} not in:\n{rendered}");
    }
}

/// Validator back from an outage, catching up 1,200 blocks behind the network, its tip moving in
/// jumps the NFS catches between (A4, A8, A12): every publish serves best yet stays `Syncing`
/// toward the validator's estimate
#[test]
fn a_tip_held_only_by_a_catching_up_validator_is_syncing_never_serving() {
    let mut builder = MockChain::regtest();
    let a12 = builder.mine_empty(12);
    let a = builder.blocks(a12);
    let now = i64::from(a[12].header().time);
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let none: [(IndexKind, DiskView); 0] = [];
    let catching_up_holding = |tip: &Block| {
        let chain = Arc::new(HeaderChainSnapshot::of(&builder, tip.block_ref()));
        let params = ChainParams::of(&builder);
        let indexed =
            NfsPublication::fixed(Arc::clone(&chain), tip.block_ref(), params, none.clone());
        let view =
            ChainViewSnapshot::fixed(Some(chain), TrustedValidatorSet::at([0]), &addresses, &[]);
        let view = view.trusted_validator(0, Health::CatchingUp, 1_200);
        (Some(Arc::new(indexed)), Arc::new(view))
    };

    let (indexed, view) = catching_up_holding(&a[4]);
    let core = Core::new(indexed, view, DEPTH);
    let grpc_snapshots = core.handle();
    for tip in [&a[4], &a[8], &a[12]] {
        let (indexed, view) = catching_up_holding(tip);
        core.publish(indexed, view);

        let grpc_snapshot = grpc_snapshots.load();
        let height = u32::from(tip.header().height);
        let tips = grpc_snapshot.tips();
        assert_eq!(tips.served, tips.best, "at A{height}: served (left) = best (right)");
        let target = grpc_snapshot.sync_target().map(u32::from);
        assert_eq!(
            (grpc_snapshot.serving_status(now), target),
            (ServingStatus::Syncing, Some(height + 1_200)),
            "at A{height}: (status, sync target) vs (Syncing, the validator's estimate)"
        );
    }
}

/// Best A12 (a 12-block chain), validator 0 holding it, a trusted run vouched through `vouched`,
/// peers' headers above it, served `served`, read at A12's clock, 1 s before stale (30 min), stale
///
/// - one block past the last vouch = the race a poll closes: serving
/// - two = no trusted validator vouches for the tip: degraded while fresh, down once stale
/// - every validator down (`[p2p]` off: the tip frozen) = serving while fresh, down once stale
#[test]
fn serving_status_by_vouch_holder_health_and_the_clock() {
    use ServingStatus::{Degraded, Down, Serving};
    let mut builder = MockChain::regtest();
    let a12 = builder.mine_empty(12);
    let a = builder.blocks(a12);
    let now = i64::from(a[12].header().time);
    let stale = now + 30 * 60;
    let none: [(IndexKind, DiskView); 0] = [];
    let stale_tip: &[&str] = &["1 validator on a stale tip"];
    #[rustfmt::skip]
    let cases = [
        // (case,                       vouched, served, (holder, behind),         other,           [now,     stale − 1, stale],   notes)
        ("caught up",                   12,      12,     (Health::Live, 0),        Health::Pending, [Serving,  Serving,   Serving], &[][..]),
        ("one failed poll",             12,      12,     (Health::Degraded, 0),    Health::Pending, [Serving,  Serving,   Serving], &[]),
        ("holder 30 blocks stale",      12,      12,     (Health::Live, 30),       Health::Pending, [Serving,  Serving,   Serving], stale_tip),
        ("every validator down",        12,      12,     (Health::Down, 0),        Health::Down,    [Serving,  Serving,   Down],    &[]),
        ("one past the last vouch",     11,      12,     (Health::Live, 0),        Health::Pending, [Serving,  Serving,   Serving], &[]),
        ("two past the last vouch",     10,      12,     (Health::Live, 0),        Health::Pending, [Degraded, Degraded,  Down],    &[]),
        ("unvouched, not synced (A4)",  10,      4,      (Health::Live, 0),        Health::Pending, [Down,     Down,      Down],    &[]),
    ];
    for (case, vouched, served, (holder, behind), other, expected, expected_notes) in cases {
        let mut headers = HeaderChain::anchored(&builder, DEPTH);
        insert(&mut headers, &a[1..=vouched]).expect("trusted run links");
        insert_unwitnessed(&mut headers, &a[vouched + 1..]).expect("peers' headers link");
        let chain = Arc::new(headers.snapshot().expect("anchored"));
        let params = ChainParams::of(&builder);
        let indexed =
            NfsPublication::fixed(Arc::clone(&chain), a[served].block_ref(), params, none.clone());
        let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
        let view =
            ChainViewSnapshot::fixed(Some(chain), TrustedValidatorSet::at([0]), &addresses, &[]);
        let view = view.trusted_validator(0, holder, behind).trusted_validator(1, other, 0);
        let grpc_snapshot = GrpcSnapshots::fixed(Some(Arc::new(indexed)), Arc::new(view)).load();

        let statuses = [now, stale - 1, stale].map(|clock| grpc_snapshot.serving_status(clock));

        let case = format!(
            "{case}: vouched through {vouched}, served {served}, holder {holder:?} {behind} \
             behind, other {other:?}"
        );
        assert_eq!(statuses, expected, "{case}: status at [now, stale − 1, stale]");
        assert_eq!(notes(&grpc_snapshot), expected_notes, "{case}: notes");
    }
}

/// Best A6, two validators configured, validator 0 the only candidate holder
#[test]
fn the_mempool_is_served_only_while_a_live_validator_holds_best_or_peers_are_quorate() {
    let mut builder = MockChain::regtest();
    let a6 = builder.mine_empty(6);
    let chain = Arc::new(HeaderChainSnapshot::of(&builder, a6));
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let not_held = Err(Unavailable::NotHeld { height: 6, configured: 2 });
    let unanchored = Err(Unavailable::NoChain);
    let holding = |health: Health| {
        let view = ChainViewSnapshot::fixed(
            Some(Arc::clone(&chain)),
            TrustedValidatorSet::at([0]),
            &addresses,
            &[],
        );
        view.trusted_validator(0, health, 0)
    };
    let no_holder = ChainViewSnapshot::fixed(
        Some(Arc::clone(&chain)),
        TrustedValidatorSet::at([]),
        &addresses,
        &[],
    );
    let no_chain = ChainViewSnapshot::fixed(None, TrustedValidatorSet::at([0]), &addresses, &[]);
    // one peer per /16 netgroup
    let peers: Vec<SocketAddr> = (1..=6).map(|group| ([10, group, 0, 1], 8233).into()).collect();
    let peers_in = |netgroups: usize| no_holder.clone().peers_listed(&peers[..netgroups]);
    #[rustfmt::skip]
    let cases = [
        // (case,                          view,                                     mempool())
        ("Live holder",                    holding(Health::Live),                    Ok(())),
        ("Live, ahead, hash at best read", holding(Health::Live).ahead(0, 1, true),  Ok(())),
        ("Live, ahead, unread: no holder", holding(Health::Live).ahead(0, 1, false), not_held),
        ("catching up: its mempool off",   holding(Health::CatchingUp),              not_held),
        ("no holder, 6 peer netgroups",    peers_in(6),                              Ok(())),
        ("no holder, 5 peer netgroups",    peers_in(5),                              not_held),
        ("no holder",                      no_holder,                                not_held),
        ("no verified chain",              no_chain,                                 unanchored),
    ];
    for (case, view, expected) in cases {
        let grpc_snapshot = GrpcSnapshots::<DiskView>::fixed(None, Arc::new(view)).load();

        let served = grpc_snapshot.mempool().map(|_| ());

        assert_eq!(served, expected, "{case}: mempool()");
        let stream = grpc_snapshot.mempool_stream().map(|_| ()).err();
        assert_eq!(stream, expected.err(), "{case}: mempool_stream() refusal = mempool()'s");
    }
}

/// `check()` fire drills: A 0..=6 (A6 mines tx M) + S5 off A4, both validators hold A6
struct Drill {
    a: Vec<Arc<Block>>,
    s5: BlockRef,
    mined: TxId,
    chain: Arc<HeaderChainSnapshot>,
    params: ChainParams,
    prev: Arc<GrpcSnapshot<DiskView>>,
    next: Arc<GrpcSnapshot<DiskView>>,
}

impl Drill {
    /// `prev` = seq 0 serving A3 (not synced), `next` = seq 1 serving A6 (synced, epoch rotated)
    fn served_a3_then_a6() -> Self {
        let mut builder = MockChain::regtest();
        builder.mine_empty(5);
        let a6 = builder.mine(|block| block.tx(|tx| tx));
        let a = builder.blocks(a6);
        let s5 = builder.fork(h(4)).mine_empty(1).tip();
        let chain = Arc::new(HeaderChainSnapshot::of(&builder, a6));
        let params = ChainParams::of(&builder);
        let view = Arc::new(view(&chain, &[]));
        let core = Core::new(served(&chain, params, a[3].block_ref()), Arc::clone(&view), DEPTH);
        let prev = core.handle().load();
        core.publish(served(&chain, params, a[6].block_ref()), view);
        let next = core.handle().load();
        let mined = builder.tx(a6, 1).txid;
        Self { a, s5, mined, chain, params, prev, next }
    }

    /// `next`, serving `block`
    fn serving(&self, block: BlockRef, synced: bool) -> GrpcSnapshot<DiskView> {
        let mut planted = copy(&self.next);
        planted.indexed = served(&self.chain, self.params, block);
        planted.tips = Tips { served: Some(block), synced, ..self.next.tips };
        planted
    }

    fn synced_prev(&self) -> GrpcSnapshot<DiskView> {
        GrpcSnapshot { tips: Tips { synced: true, ..self.prev.tips }, ..copy(&self.prev) }
    }
}

/// Both validators holding `chain`'s best, `mempool` listed
fn view(chain: &Arc<HeaderChainSnapshot>, mempool: &[(TxId, Bytes)]) -> ChainViewSnapshot {
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    ChainViewSnapshot::fixed(
        Some(Arc::clone(chain)),
        TrustedValidatorSet::at([0, 1]),
        &addresses,
        mempool,
    )
}

/// Index views serving `block` (no tables)
fn served(
    chain: &Arc<HeaderChainSnapshot>,
    params: ChainParams,
    block: BlockRef,
) -> Option<Arc<NfsPublication<DiskView>>> {
    let none: [(IndexKind, DiskView); 0] = [];
    Some(Arc::new(NfsPublication::fixed(Arc::clone(chain), block, params, none)))
}

fn copy(s: &GrpcSnapshot<DiskView>) -> GrpcSnapshot<DiskView> {
    GrpcSnapshot {
        seq: s.seq,
        tips: s.tips,
        indexed: s.indexed.clone(),
        view: Arc::clone(&s.view),
        feed: s.feed.clone(),
    }
}

fn fresh() -> Feed {
    Feed::open(Vec::new(), HashSet::new(), watch::Sender::new(()).subscribe())
}

#[test]
fn the_unplanted_drill_passes_check() {
    let d = Drill::served_a3_then_a6();

    check(&d.prev, &d.next, DEPTH);

    let (a3, a6) = (d.a[3].block_ref(), d.a[6].block_ref());
    let seen = |s: &GrpcSnapshot<DiskView>| (s.seq, s.tips.served, s.tips.synced, s.feed.sealed());
    let expected = [(0, Some(a3), false, true), (1, Some(a6), true, false)];
    assert_eq!([seen(&d.prev), seen(&d.next)], expected, "(seq, served, synced, epoch sealed)");
    assert!(!d.next.feed.same_epoch(&d.prev.feed), "served A3 → A6: epoch rotated");
}

#[test]
#[should_panic(expected = "seq + 1 per publish (G2): (prev, next) seq (0, 5)")]
fn check_catches_a_skipped_seq() {
    let d = Drill::served_a3_then_a6();
    check(&d.prev, &GrpcSnapshot { seq: 5, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "tips.best (left) = the view chain's best (right) (G3)")]
fn check_catches_a_best_off_the_chain() {
    let d = Drill::served_a3_then_a6();
    let tips = Tips { best: None, ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "tips.finalized_tip (left) = the view chain's finalized tip (right)")]
fn check_catches_a_finalized_tip_off_the_chain() {
    let d = Drill::served_a3_then_a6();
    let tips = Tips { finalized_tip: Some(d.a[1].block_ref()), ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "tips.served (left) = the NFS's served tip (right) (G3)")]
fn check_catches_a_served_tip_off_the_indexes() {
    let d = Drill::served_a3_then_a6();
    let tips = Tips { served: Some(d.a[5].block_ref()), ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "synced under a chain (G4)")]
fn check_catches_synced_without_a_chain() {
    let d = Drill::served_a3_then_a6();
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let view = ChainViewSnapshot::fixed(None, TrustedValidatorSet::at([0, 1]), &addresses, &[]);
    // G3 kept: best + finalized tip = the chainless view's None
    let tips = Tips { best: None, finalized_tip: None, ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, view: Arc::new(view), ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "synced = something served (G4)")]
fn check_catches_synced_with_nothing_served() {
    let d = Drill::served_a3_then_a6();
    let tips = Tips { served: None, ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, indexed: None, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "synced opens only at served = best, no validator claiming past it \
                           (G4): served BlockHeight(5), best BlockHeight(6), claimed past best \
                           false")]
fn check_catches_synced_opening_below_best() {
    let d = Drill::served_a3_then_a6();
    check(&d.prev, &d.serving(d.a[5].block_ref(), true), DEPTH);
}

/// Boot race: validator 0's claim past best, its hash at best not polled yet
#[test]
#[should_panic(expected = "served BlockHeight(6), best BlockHeight(6), claimed past best true")]
fn check_catches_synced_opening_under_a_validator_claiming_past_best() {
    let d = Drill::served_a3_then_a6();
    let mut planted = d.serving(d.a[6].block_ref(), true);
    planted.view = Arc::new(view(&d.chain, &[]).ahead(0, 1, false));
    check(&d.prev, &planted, DEPTH);
}

#[test]
#[should_panic(expected = "synced stays on best (G4)")]
fn check_catches_synced_serving_a_side_branch() {
    let d = Drill::served_a3_then_a6();
    check(&d.synced_prev(), &d.serving(d.s5, true), DEPTH);
}

#[test]
#[should_panic(expected = "synced stays within depth 3 of best (G4): served BlockHeight(2), best \
                           BlockHeight(6) (4 behind)")]
fn check_catches_synced_falling_behind_its_window() {
    let d = Drill::served_a3_then_a6();
    check(&d.synced_prev(), &d.serving(d.a[2].block_ref(), true), DEPTH);
}

#[test]
#[should_panic(expected = "epoch rotated (left) iff the served tip moved (right) (G5)")]
fn check_catches_an_epoch_rotating_without_a_move() {
    let d = Drill::served_a3_then_a6();
    let again = GrpcSnapshot { seq: 2, feed: fresh(), ..copy(&d.next) };
    check(&d.next, &again, DEPTH);
}

#[test]
#[should_panic(expected = "old epoch sealed (left) iff the served tip moved (right) (G5)")]
fn check_catches_an_old_epoch_left_open() {
    let d = Drill::served_a3_then_a6();
    check(&GrpcSnapshot { feed: fresh(), ..copy(&d.prev) }, &copy(&d.next), DEPTH);
}

#[test]
#[should_panic(expected = "stored epoch open (G5)")]
fn check_catches_a_sealed_stored_epoch() {
    let d = Drill::served_a3_then_a6();
    let feed = fresh();
    feed.seal();
    check(&d.prev, &GrpcSnapshot { feed, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "epoch carries no tx of the served tip's own chain (G10)")]
fn check_catches_an_epoch_carrying_a_tx_its_served_tip_mined() {
    let d = Drill::served_a3_then_a6();
    let none: [(IndexKind, DiskView); 0] = [];
    let indexed = NfsPublication::fixed_above(
        Arc::clone(&d.chain),
        d.a[5].block_ref(),
        &d.a[6..],
        d.params,
        none,
    );
    let stale = view(&d.chain, &[(d.mined, Bytes::from_static(&[0x66]))]);
    let wake = watch::Sender::new(()).subscribe();
    let feed = Feed::open(stale.arrivals(None), HashSet::new(), wake);
    let planted = GrpcSnapshot { indexed: Some(Arc::new(indexed)), feed, ..copy(&d.next) };
    check(&d.prev, &planted, DEPTH);
}
