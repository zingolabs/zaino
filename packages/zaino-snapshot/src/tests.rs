//! The run loop over real watches, the `/statusz` + `/metrics` goldens, `check()`'s fire drills

use std::num::NonZeroU32;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zaino_chainview::{ChainView, ChainViewSnapshot, EndpointSet};
use zaino_header_chain::{HeaderChain, VerifiedChain};
use zaino_index_tree_state::PoolActivations;
use zaino_internal_block_hash_to_height as block_hash;
use zaino_nfs::{ChainParams, Snapshot as Indexed};
use zaino_persistence::{
    fs::SimFs, DiskEngine, DiskView, IndexKind, PersistenceEngine, Schema, Store,
};
use zaino_primitives::testing::Chain;
use zaino_primitives::types::{Block, BlockRef, Height, ReorgDepth, TransactionId};
use zaino_source::mock::MockChain;
use zaino_traffic::{Limits, TrafficBalancer, Trusted};
use zcash_protocol::consensus::NetworkType;

use crate::compose::check;
use crate::feed::Feed;
use crate::publisher::Core;
use crate::{describe_metrics, emit_gauges, Publisher, Report, Snapshot, SnapshotError, Tips};

const DEPTH: ReorgDepth = ReorgDepth::new(NonZeroU32::new(3).expect("nonzero"));
const PARAMS: ChainParams = ChainParams {
    network: NetworkType::Regtest,
    activations: PoolActivations { sapling: Height::GENESIS, orchard: None, ironwood: None },
};

fn at(block: &Block) -> BlockRef {
    BlockRef { hash: block.header().hash, height: block.header().height }
}

/// - Seq 0 stored at `new` (nothing verified, nothing served)
/// - A chain view publish → seq 1 under the new chain
/// - Three NFS publishes before the publisher runs → one publish, from the last (coalesced)
/// - Cancel → `Ok`; the NFS's watch dropped → `IndexedGone`
#[tokio::test]
async fn the_publisher_follows_both_watches_coalesces_and_stops_on_cancel_or_a_gone_nfs() {
    let mut builder = Chain::new();
    let a3 = builder.extend(builder.genesis().hash, 3);
    let path = builder.path(a3.hash);
    let source = Arc::new(MockChain::serving(path.clone()));
    let limits = Limits::new(8, None).expect("8 ≥ MIN_CONNECTIONS");
    let (balancer, _never_driven) =
        TrafficBalancer::new(vec![Trusted { source, priority: 0, limits }], None);
    let address = vec!["10.0.0.1:8232".to_owned()];
    let view = ChainView::new(address, balancer, DEPTH).expect("one validator");
    let (nfs, indexed) = watch::channel(None);
    let publisher = Publisher::<DiskView>::new(indexed, view.subscriber(), DEPTH);
    let mut snapshots = publisher.handle();
    assert_eq!((snapshots.load().seq(), snapshots.load().tips()), (0, Tips::default()));

    let cancel = CancellationToken::new();
    let running = tokio::spawn(publisher.run(cancel.clone()));
    let chain = VerifiedChain::regtest(&path);
    view.set_verified(Some(chain.clone()));
    snapshots.changed().await.expect("publisher running");
    let snap = snapshots.load();
    assert_eq!((snap.seq(), snap.tips().best, snap.tips().served), (1, Some(a3), None));

    let none: [(IndexKind, DiskView); 0] = [];
    for block in &path[1..] {
        let indexed = Indexed::fixed(Arc::new(chain.clone()), at(block), PARAMS, none.clone());
        nfs.send_replace(Some(Arc::new(indexed)));
    }
    snapshots.changed().await.expect("publisher running");
    let snap = snapshots.load();
    let tips = snap.tips();
    assert_eq!((snap.seq(), tips.served, tips.synced), (2, Some(a3), true), "coalesced burst");

    cancel.cancel();
    assert!(matches!(running.await.expect("task"), Ok(())), "cancel = clean stop");
    let gone = Publisher::<DiskView>::new(nfs.subscribe(), view.subscriber(), DEPTH);
    drop(nfs);
    let stopped = gone.run(CancellationToken::new()).await;
    assert!(matches!(stopped, Err(SnapshotError::IndexedGone)), "{stopped:?}");
}

/// A 0..=5 final through 2, side S4..=S5 off A3; one of two validators holds A5; block-hash
/// durable through A5 (the only index), served A5; one relayed tx, one unlisted: the `/statusz`
/// body field by field, then the gauges a scrape renders from the same snapshot
#[test]
fn one_snapshot_renders_the_status_report_and_every_gauge() {
    let mut builder = Chain::new();
    let a5 = builder.extend(builder.genesis().hash, 5);
    let a = builder.path(a5.hash);
    let s5 = builder.extend(a[3].header().hash, 2);
    let s = builder.path(s5.hash);
    let mut headers = HeaderChain::regtest_in_memory(builder.genesis().hash, DEPTH);
    headers.insert_blocks(&a).expect("A verifies");
    headers.insert_blocks(&s[4..]).expect("S verifies");
    headers.finalize(at(&a[2])).expect("in-memory store");
    let chain = Arc::new(headers.verified().expect("verified"));

    let engine = DiskEngine::new(SimFs::new());
    let schema = Schema::new(
        IndexKind::BlockHash,
        block_hash::FORMAT,
        NetworkType::Regtest,
        block_hash::TABLES,
    );
    let mut store = engine.open(Path::new("block_hash"), &schema).expect("fresh store");
    for block in &a {
        let mut out = store.changes(block.at());
        block_hash::fold(&block_hash::BlockHashReader::new(store.staged()), block, &mut out);
        store.apply(out);
    }
    store.commit().expect("SimFs commit");
    let durable = [(IndexKind::BlockHash, store.view())];
    let indexed = Indexed::fixed(Arc::clone(&chain), at(&a[5]), PARAMS, durable);

    let (relayed, unlisted) = (TransactionId::from([1; 32]), TransactionId::from([2; 32]));
    let addresses = ["10.0.0.1:8232", "10.0.0.2:8232"];
    let view = ChainViewSnapshot::fixed(
        Some(Arc::clone(&chain)),
        EndpointSet::at([0]),
        &addresses,
        &[(relayed, Bytes::from_static(b"relayed"))],
        &[(unlisted, Bytes::from_static(b"unlisted"))],
    );
    let core = Core::new(Some(Arc::new(indexed)), Arc::new(view), DEPTH);
    let snap = core.handle().load();
    let handed = Some(Height::try_from(5u32).expect("h"));

    let block = |block: &Block| {
        let at = at(block);
        serde_json::json!({ "height": u32::from(at.height), "hash": at.hash.to_string() })
    };
    let work = chain.forks()[0].cumulative_work.to_string();
    let disabled =
        |name: &str| serde_json::json!({ "name": name, "enabled": false, "durable": null });
    let validator = |address: &str| {
        serde_json::json!({
            "address": address, "agreement": "unknown", "height": null, "stale_blocks": null,
            "streaming": false, "release": null, "peers": [],
        })
    };
    let expected = serde_json::json!({
        "seq": 0,
        "tips": {
            "best": block(&a[5]), "final": block(&a[2]), "served": block(&a[5]),
            "held_by": 1, "configured": 2, "synced": true,
        },
        "unready": [],
        "handed": 5,
        "indexes": [
            disabled("value_balance"),
            disabled("compact_block"),
            { "name": "block_hash", "enabled": true, "durable": 5 },
            disabled("tree_state"),
            disabled("transparent_address"),
        ],
        "validators": [validator(addresses[0]), validator(addresses[1])],
        "alarms": {
            "partitioned": false, "eclipsed": false, "finality_paused": false,
            "stale": [], "ending": [],
        },
        "mempool": {
            "transactions": 2, "verified": 0, "ours_unverified": 1, "fully_spread": 0,
            "trusted_readers": 0,
        },
        "forks": [
            { "from": block(&a[3]), "tip": block(&s[5]), "cumulative_work": work, "folded": null },
        ],
    });
    let report = serde_json::to_value(Report::of(&snap, handed)).expect("serializes");
    assert_eq!(report, expected);

    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let render = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        describe_metrics();
        emit_gauges(&snap, handed);
    });
    let rendered = render.render();
    for line in [
        "zaino_best_tip 5",
        "zaino_fetch_height 5",
        "zaino_index_finalized_height{index=\"block_hash\"} 5",
        "zaino_index_synced{index=\"block_hash\"} 1",
    ] {
        assert!(rendered.lines().any(|at| at == line), "{line:?} not in:\n{rendered}");
    }
}

/// Panic message of `run`, `None` = it returned
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = catch_unwind(AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
}

/// A 0..=6 + S5 off A4, both validators hold A6: seq 0 serves A3 (not synced), seq 1 serves
/// A6 (synced, epoch rotated): `check` passes; then one planted bug per assertion, each firing
/// its own (G6 = the same view as G3's `held_by`: no separate plant)
#[test]
fn every_check_fires_on_its_planted_bug() {
    let mut builder = Chain::new();
    let a6 = builder.extend(builder.genesis().hash, 6);
    let a = builder.path(a6.hash);
    let s5 = builder.mine(a[4].header().hash);
    let chain = Arc::new(VerifiedChain::regtest(&a));
    let served = |block: BlockRef| {
        let none: [(IndexKind, DiskView); 0] = [];
        Some(Arc::new(Indexed::fixed(Arc::clone(&chain), block, PARAMS, none)))
    };
    let view = Arc::new(ChainViewSnapshot::fixed(
        Some(Arc::clone(&chain)),
        EndpointSet::at([0, 1]),
        &["10.0.0.1:8232", "10.0.0.2:8232"],
        &[],
        &[],
    ));
    let core = Core::new(served(at(&a[3])), Arc::clone(&view), DEPTH);
    let prev = core.handle().load();
    core.publish(served(at(&a[6])), Arc::clone(&view));
    let next = core.handle().load();
    assert_eq!(fired(|| check(&prev, &next, DEPTH)), None, "the unplanted pair passes");

    let copy = |s: &Snapshot<DiskView>| Snapshot {
        seq: s.seq,
        tips: s.tips,
        indexed: s.indexed.clone(),
        view: Arc::clone(&s.view),
        feed: s.feed.clone(),
    };
    let fresh =
        |key: BlockRef| Feed::open(Some(key), Vec::new(), watch::Sender::new(()).subscribe());
    let serving = |block: BlockRef, synced: bool| {
        let mut planted = copy(&next);
        planted.indexed = served(block);
        planted.tips = Tips { served: Some(block), synced, ..next.tips };
        planted
    };
    let synced_prev = || Snapshot { tips: Tips { synced: true, ..prev.tips }, ..copy(&prev) };
    type Plant<'a> = Box<dyn Fn() -> (Snapshot<DiskView>, Snapshot<DiskView>) + 'a>;
    let drills: Vec<(&str, Plant)> = vec![
        ("G2: seq + 1", Box::new(|| (copy(&prev), Snapshot { seq: 5, ..copy(&next) }))),
        (
            "G3: best",
            Box::new(|| {
                (copy(&prev), Snapshot { tips: Tips { best: None, ..next.tips }, ..copy(&next) })
            }),
        ),
        (
            "G3: final",
            Box::new(|| {
                let tips = Tips { final_tip: Some(at(&a[1])), ..next.tips };
                (copy(&prev), Snapshot { tips, ..copy(&next) })
            }),
        ),
        (
            "G3: held_by",
            Box::new(|| {
                let tips = Tips { held_by: EndpointSet::default(), ..next.tips };
                (copy(&prev), Snapshot { tips, ..copy(&next) })
            }),
        ),
        (
            "G3: served",
            Box::new(|| {
                let tips = Tips { served: Some(at(&a[5])), ..next.tips };
                (copy(&prev), Snapshot { tips, ..copy(&next) })
            }),
        ),
        (
            "G4: synced opens only at served = best",
            Box::new(|| (copy(&prev), serving(at(&a[5]), true))),
        ),
        ("G4: synced stays on best", Box::new(|| (synced_prev(), serving(s5, true)))),
        ("G4: synced stays within", Box::new(|| (synced_prev(), serving(at(&a[2]), true)))),
        (
            "G5: epoch key",
            Box::new(|| (copy(&prev), Snapshot { feed: fresh(at(&a[3])), ..copy(&next) })),
        ),
        (
            "G5: epoch rotates iff",
            Box::new(|| {
                let again = Snapshot { seq: 2, feed: fresh(at(&a[6])), ..copy(&next) };
                (copy(&next), again)
            }),
        ),
        (
            "G5: the old epoch sealed iff rotated",
            Box::new(|| (Snapshot { feed: fresh(at(&a[3])), ..copy(&prev) }, copy(&next))),
        ),
        (
            "G5: the stored epoch open",
            Box::new(|| {
                let feed = fresh(at(&a[6]));
                feed.seal();
                (copy(&prev), Snapshot { feed, ..copy(&next) })
            }),
        ),
    ];
    for (expected, plant) in drills {
        let (prev, next) = plant();
        let message = fired(|| check(&prev, &next, DEPTH)).unwrap_or_default();
        assert!(message.contains(expected), "planted {expected:?}, fired {message:?}");
    }
}
