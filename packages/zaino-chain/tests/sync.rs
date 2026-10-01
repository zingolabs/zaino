//! Feeding the finalised store from the chain head's freeze stream.
//!
//! The composer's only write path. Every test here drives it the way a
//! deployment does — spawn the loop, emit blocks from the chain head, look at
//! what the store ended up holding — rather than calling the internals, because
//! the thing worth testing is that a gap reported by one tier is repaired
//! against the other without anyone outside noticing.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zaino_chain::testing::{Chain, FakeHead, FakeSource, FakeStore};
use zaino_chain::{ChainViewComposer, ChainViewConfig};
use zaino_component::Lifecycle;

fn chain() -> Chain {
    Chain::of_length(1201)
}

/// A composer over these three, in an `Arc` because the sync loop holds one.
fn composed(
    store: FakeStore,
    head: FakeHead,
    chain: &Chain,
) -> Arc<ChainViewComposer<FakeStore, FakeHead, FakeSource>> {
    Arc::new(ChainViewComposer::new(
        store,
        head,
        Arc::new(FakeSource::over(chain)),
        ChainViewConfig::default(),
    ))
}

/// Waits for `condition`, rather than sleeping a fixed time and hoping.
///
/// The loop is a spawned task, so every assertion here is about something that
/// becomes true rather than something that is true now. Polling with a deadline
/// fails in bounded time when the behaviour is absent and returns immediately
/// when it is present, which a fixed sleep does neither of.
async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// The ordinary case: the store is behind, so the first frozen block gaps, the
/// loop builds to close it, and the block lands.
///
/// This is the whole mechanism in one test. There is no separate catch-up
/// phase — the gap error *is* the trigger — so a cold start and a chain head
/// that ran ahead take exactly this path.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_is_closed_and_the_block_lands() {
    let chain = chain();
    let store = FakeStore::empty().buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());
    head.freeze(&chain, 1000);

    assert!(
        eventually(|| store.heights().last() == Some(&1000)).await,
        "the store should have built to 999 and frozen 1000, holding {:?}",
        store.heights().last(),
    );
    assert_eq!(
        store.heights().len(),
        1001,
        "genesis through the frozen block, with no hole",
    );
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
}

/// Contiguous blocks arriving together are written together, and all of them
/// land.
///
/// Batching is an optimisation — one write transaction rather than several —
/// so the property that matters is that it changes nothing observable.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_of_blocks_all_land() {
    let chain = chain();
    let store = FakeStore::covering(&chain, 999).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let _sync = composer.spawn_sync(CancellationToken::new());
    for height in 1000..1005 {
        head.freeze(&chain, height);
    }

    assert!(
        eventually(|| store.heights().last() == Some(&1004)).await,
        "every frozen block should land; got {:?}",
        store.heights().last(),
    );
}

/// A block the store already holds is skipped, not refused.
///
/// The freeze stream's retention window overlaps what a store built for itself,
/// so being handed blocks it already has is the ordinary case rather than a
/// fault — and a loop that treated it as one would stall on every handover.
#[tokio::test(flavor = "multi_thread")]
async fn a_block_already_held_is_skipped() {
    let chain = chain();
    let store = FakeStore::covering(&chain, 1010).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());
    head.freeze(&chain, 1005);
    head.freeze(&chain, 1011);

    assert!(
        eventually(|| store.heights().last() == Some(&1011)).await,
        "the below-tip block is skipped and the next one appends",
    );
    assert_eq!(sync.status().lifecycle, Lifecycle::Ready);
    assert_eq!(store.heights().len(), 1012, "nothing was written twice");
}

/// A store that cannot close its gap reports it and keeps running.
///
/// The failure a deployment actually hits: the store's validator cannot reach
/// the height the chain head is at. The loop must not write into a hole and
/// must not treat the store as ready — but it must also stay up, because the
/// condition is usually temporary.
#[tokio::test(flavor = "multi_thread")]
async fn a_gap_that_cannot_be_closed_leaves_the_store_untouched() {
    let chain = chain();
    // Buildable only to 500, while the chain head freezes from 1000.
    let store = FakeStore::empty().buildable_from(&chain, 500);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let sync = composer.spawn_sync(CancellationToken::new());
    head.freeze(&chain, 1000);

    assert!(
        !eventually(|| !store.heights().is_empty()).await,
        "nothing may be written above a hole",
    );
    assert_eq!(
        sync.status().lifecycle,
        Lifecycle::Syncing,
        "an unclosed gap is not readiness",
    );
    assert_eq!(
        sync.status().health,
        zaino_component::Health::Recoverable,
        "a gap it cannot close is degraded, not healthy — which one axis could not say",
    );
}

/// Cancelling the token stops the loop, and the stop is observable.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_stops_the_loop() {
    let chain = chain();
    let store = FakeStore::covering(&chain, 999).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    let cancel = CancellationToken::new();
    let sync = composer.spawn_sync(cancel.clone());
    cancel.cancel();

    assert!(
        eventually(|| sync.status().lifecycle == Lifecycle::Offline).await,
        "a cancelled loop should report itself offline, not {:?}",
        sync.status().lifecycle,
    );

    head.freeze(&chain, 1000);
    assert!(
        !eventually(|| store.heights().last() == Some(&1000)).await,
        "a stopped loop must not still be writing",
    );
}

/// Dropping the handle stops the loop too.
///
/// The same mechanism as cancelling, exposed as a lifetime: a caller that lets
/// the handle go has stopped caring, and a task still writing to a store nobody
/// holds is a leak that looks like working software.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_handle_stops_the_loop() {
    let chain = chain();
    let store = FakeStore::covering(&chain, 999).buildable_from(&chain, 1200);
    let head = FakeHead::covering(&chain, 1000, 1200);
    let composer = composed(store.clone(), head.clone(), &chain);

    drop(composer.spawn_sync(CancellationToken::new()));

    head.freeze(&chain, 1000);
    assert!(
        !eventually(|| store.heights().last() == Some(&1000)).await,
        "a dropped handle must not leave a task writing",
    );
}
