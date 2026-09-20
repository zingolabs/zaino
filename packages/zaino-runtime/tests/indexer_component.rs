//! The indexer as an owned component: boots `Syncing`, reaches `Ready` when
//! caught up, and gates readiness while it syncs.

use std::sync::Arc;
use std::time::Duration;

use zaino_component::{
    CancellationToken, ComponentName, Health, Lifecycle, Managed, ReadySignal, StatusWatch,
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

/// A sync driver whose run loop *panics* (after yielding so the component is
/// observed `Syncing` first) — a panic outside any joined sub-task, the silent-
/// death path.
struct PanicSync;

impl SyncDriver for PanicSync {
    type Error = SyncError;

    async fn run(
        self: Arc<Self>,
        _cancel: CancellationToken,
        _caught_up: ReadySignal,
    ) -> Result<(), SyncError> {
        tokio::task::yield_now().await;
        panic!("boom in the run loop");
    }
}

#[tokio::test]
async fn a_panicking_run_loop_goes_critical_and_escalates() {
    let indexer = IndexerComponent::new(ComponentName("indexer"), PanicSync);
    let mut orchestra = OrchestraBuilder::new()
        .boot(indexer)
        .await
        .expect("boot indexer (reaches Syncing before it panics)")
        .build();

    // The run-loop panic must escalate through the runtime, not silently freeze
    // the status at Syncing/Healthy while the supervisor waits forever.
    let escalated = tokio::time::timeout(Duration::from_secs(1), orchestra.next_escalation())
        .await
        .expect("escalation arrived — the panic was not a silent death");
    assert_eq!(escalated, Some(ComponentName("indexer")));

    let status = &orchestra.statuses()[0];
    assert_eq!(status.health, Health::Critical);
    assert!(
        status
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("panicked"),
        "the reason names the panic: {:?}",
        status.reason
    );

    orchestra.shutdown();
}
