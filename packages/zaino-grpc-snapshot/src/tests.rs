//! The run loop over real watches, the `/metrics` golden, `check()`'s fire drills

use std::collections::HashSet;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chainview::{ChainView, ChainViewSnapshot, TrustedValidatorSet};
use zaino_header_chain::testing::{insert, insert_unwitnessed, HeaderViews};
use zaino_header_chain::HeaderChainSnapshot;
use zaino_nfs::{ChainParams, NfsPublication};
use zaino_persistence::{fs::SimFs, DiskEngine, DiskView, IndexKind, PersistenceEngine, Store};
use zaino_primitives::testing::{h, MockChain};
use zaino_primitives::types::{Block, BlockRef, FinalityDepth};
use zaino_source::testing::{within, ValidatorViews};
use zaino_sync::SyncProgress;
use zaino_traffic::{Health, Limits, TrafficBalancer, TrustedValidator};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::consensus::NetworkType;
use zcash_protocol::TxId;

use crate::compose::check;
use crate::feed::Feed;
use crate::publisher::Core;
use crate::{
    describe_metrics, emit_gauges, notes, GrpcSnapshot, GrpcSnapshotError, GrpcSnapshots,
    Publisher, ServingStatus, Tips,
};

const DEPTH: FinalityDepth = FinalityDepth::new(NonZeroU32::new(3).expect("nonzero"));

/// - Seq 0 stored at `new` (nothing verified, nothing served)
/// - A chain view publish → seq 1 under the new chain
/// - Three NFS publishes before the publisher runs → one publish, from the last (coalesced)
/// - Cancel → `Ok`
/// - NFS's watch dropped → `IndexedGone`
#[tokio::test]
async fn the_publisher_follows_both_watches_coalesces_and_stops_on_cancel_or_a_gone_nfs() {
    let mut builder = MockChain::regtest();
    let a3 = builder.mine_empty(3);
    let path = builder.blocks(a3);
    let params = ChainParams::of(&builder);
    let source = Arc::new(builder.validator(a3));
    let limits = Limits::new(8).expect("8 ≥ MIN_CONNECTIONS");
    let address = "10.0.0.1:8232".to_owned();
    let trusted = TrustedValidator { address, source, priority: 0, limits };
    let (balancer, _never_driven) = TrafficBalancer::new(vec![trusted], None);
    let schedule = builder.schedule().upgrades;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let view = ChainView::new(balancer, DEPTH, schedule, shutdown);
    let (nfs, indexed) = watch::channel(None);
    let publisher = Publisher::<DiskView>::new(indexed, view.subscriber(), DEPTH);
    let mut grpc_snapshots = publisher.handle();
    assert_eq!((grpc_snapshots.load().seq(), grpc_snapshots.load().tips()), (0, Tips::default()));

    let cancel = CancellationToken::new();
    let running = tokio::spawn(publisher.run(cancel.clone()));
    let chain = builder.snapshot(a3);
    view.set_chain(Some(chain.clone()));
    within("a publish", grpc_snapshots.changed()).await.expect("publisher running");
    let grpc_snapshot = grpc_snapshots.load();
    assert_eq!(
        (grpc_snapshot.seq(), grpc_snapshot.tips().best, grpc_snapshot.tips().served),
        (1, Some(a3), None)
    );

    let none: [(IndexKind, DiskView); 0] = [];
    for block in &path[1..] {
        let indexed =
            NfsPublication::fixed(Arc::new(chain.clone()), block.block_ref(), params, none.clone());
        nfs.send_replace(Some(Arc::new(indexed)));
    }
    within("a publish", grpc_snapshots.changed()).await.expect("publisher running");
    let grpc_snapshot = grpc_snapshots.load();
    let tips = grpc_snapshot.tips();
    assert_eq!(
        (grpc_snapshot.seq(), tips.served, tips.synced),
        (2, Some(a3), true),
        "coalesced burst"
    );

    cancel.cancel();
    let ended = within("the publisher stopped", running).await;
    assert!(matches!(ended.expect("task"), Ok(())), "cancel = clean stop");
    let gone = Publisher::<DiskView>::new(nfs.subscribe(), view.subscriber(), DEPTH);
    drop(nfs);
    let stopped = within("the publisher stopped", gone.run(CancellationToken::new())).await;
    assert!(matches!(stopped, Err(GrpcSnapshotError::IndexedGone)), "{stopped:?}");
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
    let mut headers = builder.header_chain(DEPTH);
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
        describe_metrics();
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
        assert!(rendered.lines().any(|at| at == line), "{line:?} not in:\n{rendered}");
    }
}

/// Validator back from an outage, catching up 1,200 blocks behind the network, its tip moving in
/// jumps the NFS catches between: every publish serves best yet stays `Syncing` toward the
/// validator's estimate
#[test]
fn a_tip_held_only_by_a_catching_up_validator_is_syncing_never_serving() {
    let mut builder = MockChain::regtest();
    let a12 = builder.mine_empty(12);
    let a = builder.blocks(a12);
    let now = i64::from(a[12].header().time);
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let none: [(IndexKind, DiskView); 0] = [];
    let served_at = |tip: &Block| {
        let chain = Arc::new(builder.snapshot(tip.block_ref()));
        let params = ChainParams::of(&builder);
        let indexed =
            NfsPublication::fixed(Arc::clone(&chain), tip.block_ref(), params, none.clone());
        let view =
            ChainViewSnapshot::fixed(Some(chain), TrustedValidatorSet::at([0]), &addresses, &[]);
        (Arc::new(indexed), view)
    };

    let (indexed, view) = served_at(&a[4]);
    let core = Core::new(
        Some(indexed),
        Arc::new(view.trusted_validator(0, Health::CatchingUp, 1_200)),
        DEPTH,
    );
    let grpc_snapshots = core.handle();
    for tip in [&a[4], &a[8], &a[12]] {
        let (indexed, view) = served_at(tip);
        core.publish(Some(indexed), Arc::new(view.trusted_validator(0, Health::CatchingUp, 1_200)));
        let grpc_snapshot = grpc_snapshots.load();
        let height = u32::from(tip.header().height);
        assert_eq!(
            grpc_snapshot.tips().served,
            grpc_snapshot.tips().best,
            "at {height}: served = best"
        );
        let target = grpc_snapshot.sync_target().map(u32::from);
        assert_eq!(
            (grpc_snapshot.serving_status(now), target),
            (ServingStatus::Syncing, Some(height + 1_200)),
            "{height}"
        );
    }
}

/// Synced to a fresh best held by validator 0: each (holder, other) health → serving status + notes
///
/// - every validator down = still `Serving` (best vouched: degraded once it runs unvouched)
#[test]
fn each_holder_health_gives_its_standing_and_notes() {
    let mut builder = MockChain::regtest();
    let a12 = builder.mine_empty(12);
    let now = i64::from(builder.blocks(a12)[12].header().time);
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let none: [(IndexKind, DiskView); 0] = [];
    let chain = Arc::new(builder.snapshot(a12));
    let indexed =
        Arc::new(NfsPublication::fixed(Arc::clone(&chain), a12, ChainParams::of(&builder), none));
    let cases = [
        ("caught up", Health::Live, 0, Health::Pending, ServingStatus::Serving, vec![]),
        ("one failed poll", Health::Degraded, 0, Health::Pending, ServingStatus::Serving, vec![]),
        (
            "holder stale",
            Health::Live,
            30,
            Health::Pending,
            ServingStatus::Serving,
            vec!["1 validator on a stale tip".to_owned()],
        ),
        ("every validator down", Health::Down, 0, Health::Down, ServingStatus::Serving, vec![]),
    ];
    for (case, holder, behind, other, serving_status, expected_notes) in cases {
        let view = ChainViewSnapshot::fixed(
            Some(Arc::clone(&chain)),
            TrustedValidatorSet::at([0]),
            &addresses,
            &[],
        );
        let view = view.trusted_validator(0, holder, behind).trusted_validator(1, other, 0);
        let grpc_snapshot = GrpcSnapshots::fixed(Some(Arc::clone(&indexed)), Arc::new(view)).load();
        assert_eq!(
            (grpc_snapshot.serving_status(now), notes(&grpc_snapshot)),
            (serving_status, expected_notes),
            "{case}"
        );
    }
}

/// Best A12, a trusted validator's run vouched through `vouched`, peers' headers above it, at the
/// clock of A12, 1 s before it goes stale (30 min), then stale
///
/// - one block past the last vouch = the race a poll closes: serving
/// - two = no trusted validator vouches for the tip: degraded while fresh, down once stale
/// - not synced (served A4) under an unvouched tip: down
/// - vouched but the one validator down (`[p2p]` off: the tip frozen): serving while fresh, down
///   once stale
#[test]
fn a_tip_two_blocks_past_the_last_vouch_is_degraded_while_fresh_and_down_once_stale() {
    let mut builder = MockChain::regtest();
    let a12 = builder.mine_empty(12);
    let a = builder.blocks(a12);
    let now = i64::from(a[12].header().time);
    let stale = now + 30 * 60;
    let none: [(IndexKind, DiskView); 0] = [];
    let (serving, degraded, down) =
        (ServingStatus::Serving, ServingStatus::Degraded, ServingStatus::Down);
    #[rustfmt::skip]
    let cases = [
        // (vouched through, served, validator, (now, stale − 1, stale))
        (12, 12, Health::Live, (serving, serving, serving)),
        (11, 12, Health::Live, (serving, serving, serving)),
        (10, 12, Health::Live, (degraded, degraded, down)),
        (10, 4,  Health::Live, (down, down, down)),
        (12, 12, Health::Down, (serving, serving, down)),
    ];
    for (vouched, served, health, expected) in cases {
        let mut headers = builder.header_chain(DEPTH);
        insert(&mut headers, &a[1..=vouched]).expect("trusted run links");
        insert_unwitnessed(&mut headers, &a[vouched + 1..]).expect("peers' headers link");
        let chain = Arc::new(headers.snapshot().expect("anchored"));
        let params = ChainParams::of(&builder);
        let indexed =
            NfsPublication::fixed(Arc::clone(&chain), a[served].block_ref(), params, none.clone());
        let holder = TrustedValidatorSet::at([0]);
        let view = ChainViewSnapshot::fixed(Some(chain), holder, &["10.0.0.1:8232"], &[]);
        let view = view.trusted_validator(0, health, 0);
        let grpc_snapshot = GrpcSnapshots::fixed(Some(Arc::new(indexed)), Arc::new(view)).load();
        let serving_status = (
            grpc_snapshot.serving_status(now),
            grpc_snapshot.serving_status(stale - 1),
            grpc_snapshot.serving_status(stale),
        );
        let case = format!("vouched through {vouched}, served {served}, {health:?}");
        assert_eq!(serving_status, expected, "{case}");
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
    fn new() -> Self {
        let mut builder = MockChain::regtest();
        builder.mine_empty(5);
        let a6 = builder.mine(|block| block.tx(|tx| tx.txid([0x66; 32])));
        let a = builder.blocks(a6);
        let s5 = builder.fork(h(4)).mine_empty(1).tip();
        let chain = Arc::new(builder.snapshot(a6));
        let params = ChainParams::of(&builder);
        let view = Arc::new(view(&chain, &[]));
        let core = Core::new(served(&chain, params, a[3].block_ref()), Arc::clone(&view), DEPTH);
        let prev = core.handle().load();
        core.publish(served(&chain, params, a[6].block_ref()), view);
        let next = core.handle().load();
        Self { a, s5, mined: TxId::from_bytes([0x66; 32]), chain, params, prev, next }
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
#[should_panic(expected = "G2: seq + 1")]
fn check_catches_a_skipped_seq() {
    let d = Drill::new();
    check(&d.prev, &GrpcSnapshot { seq: 5, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: best")]
fn check_catches_a_best_off_the_chain() {
    let d = Drill::new();
    let tips = Tips { best: None, ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: finalized")]
fn check_catches_a_finalized_tip_off_the_chain() {
    let d = Drill::new();
    let tips = Tips { finalized_tip: Some(d.a[1].block_ref()), ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G3: served")]
fn check_catches_a_served_tip_off_the_indexes() {
    let d = Drill::new();
    let tips = Tips { served: Some(d.a[5].block_ref()), ..d.next.tips };
    check(&d.prev, &GrpcSnapshot { tips, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G4: synced opens only at served = best")]
fn check_catches_synced_opening_below_best() {
    let d = Drill::new();
    check(&d.prev, &d.serving(d.a[5].block_ref(), true), DEPTH);
}

/// Boot race: validator 0's claim past best, its hash at best not polled yet
#[test]
#[should_panic(expected = "no validator claiming past it")]
fn check_catches_synced_opening_under_a_validator_claiming_past_best() {
    let d = Drill::new();
    let mut planted = d.serving(d.a[6].block_ref(), true);
    planted.view = Arc::new(view(&d.chain, &[]).ahead(0, 1, false));
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
    check(&d.synced_prev(), &d.serving(d.a[2].block_ref(), true), DEPTH);
}

#[test]
#[should_panic(expected = "G5: epoch rotates iff")]
fn check_catches_an_epoch_rotating_without_a_move() {
    let d = Drill::new();
    let again = GrpcSnapshot { seq: 2, feed: fresh(), ..copy(&d.next) };
    check(&d.next, &again, DEPTH);
}

#[test]
#[should_panic(expected = "G5: the old epoch sealed iff rotated")]
fn check_catches_an_old_epoch_left_open() {
    let d = Drill::new();
    check(&GrpcSnapshot { feed: fresh(), ..copy(&d.prev) }, &copy(&d.next), DEPTH);
}

#[test]
#[should_panic(expected = "G5: the stored epoch open")]
fn check_catches_a_sealed_stored_epoch() {
    let d = Drill::new();
    let feed = fresh();
    feed.seal();
    check(&d.prev, &GrpcSnapshot { feed, ..copy(&d.next) }, DEPTH);
}

#[test]
#[should_panic(expected = "G10: the epoch carries a tx of the served tip's own chain")]
fn check_catches_an_epoch_carrying_a_tx_its_served_tip_mined() {
    let d = Drill::new();
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
