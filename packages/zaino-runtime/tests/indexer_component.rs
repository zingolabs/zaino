//! The indexer as an owned component: boots `Syncing`, reaches `Ready` when
//! caught up, and gates readiness while it syncs.

use std::sync::Arc;
use std::time::Duration;

use zaino_component::{
    CancellationToken, ComponentName, Lifecycle, Managed, ReadySignal, StatusWatch,
};
use zaino_runtime::{IndexerComponent, OrchestraBuilder, SyncDriver};

/// A stub sync driver. `catch_up` decides whether it reaches the tip.
struct StubSync {
    catch_up: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("sync failed")]
struct SyncError;

impl SyncDriver for StubSync {
    type Error = SyncError;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        caught_up: ReadySignal,
    ) -> Result<(), SyncError> {
        if self.catch_up {
            caught_up.notify();
        }
        cancel.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn an_indexer_reaches_ready_when_caught_up() {
    let indexer = IndexerComponent::new(ComponentName("indexer"), StubSync { catch_up: true });
    indexer.spawn().await.expect("spawn");

    let mut status = indexer.subscribe();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if status.borrow_and_update().lifecycle == Lifecycle::Ready {
                return;
            }
            status.changed().await.expect("status stream open");
        }
    })
    .await
    .expect("caught up to Ready");

    indexer.stop().await.expect("stop");
}

#[tokio::test]
async fn a_syncing_indexer_boots_but_gates_readiness() {
    // Never catches up: stays Syncing.
    let indexer = IndexerComponent::new(ComponentName("indexer"), StubSync { catch_up: false });
    let orchestra = OrchestraBuilder::new()
        .boot(indexer)
        .await
        .expect("boot indexer")
        .build();

    // Boot proceeded once it was *running* (Syncing) — not blocked on caught-up.
    assert_eq!(orchestra.statuses()[0].lifecycle, Lifecycle::Syncing);

    // Under full mode a syncing indexer keeps the runtime not ready / not started.
    let signals = *orchestra.signals().borrow();
    assert!(!signals.ready, "a syncing indexer gates readiness");
    assert!(!signals.started, "still booting while syncing");

    orchestra.shutdown();
}
