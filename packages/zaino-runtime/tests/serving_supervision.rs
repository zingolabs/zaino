//! Supervising servers through the `Serve` seam.
//!
//! Health is driven by the serve task's real outcome: a bind failure means the
//! component never becomes `Ready` (a boot failure); a serve loop that dies
//! *after* binding goes `Critical` and escalates.

use std::sync::Arc;
use std::time::Duration;

use zaino_component::{
    CancellationToken, ComponentName, Health, Lifecycle, ReadySignal, StatusSource,
};
use zaino_runtime::{BootError, OrchestraBuilder, RuntimeOutcome, Serve, ServeComponent};

/// A stub transport server with a scriptable behavior.
enum Behavior {
    /// Bind, report ready, serve until cancelled.
    ServeUntilCancel,
    /// Fail before binding (never reports ready).
    FailToBind,
    /// Report ready, then the serve loop dies.
    FailAfterReady,
}

struct StubServer {
    behavior: Behavior,
}

#[derive(Debug, thiserror::Error)]
#[error("stub server failed")]
struct StubError;

impl Serve for StubServer {
    type Error = StubError;

    async fn serve(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> Result<(), StubError> {
        match self.behavior {
            Behavior::FailToBind => Err(StubError),
            Behavior::ServeUntilCancel => {
                ready.notify();
                cancel.cancelled().await;
                Ok(())
            }
            Behavior::FailAfterReady => {
                ready.notify();
                Err(StubError)
            }
        }
    }
}

#[tokio::test]
async fn a_serve_loop_failure_after_ready_escalates_and_is_fatal() {
    let light = ServeComponent::new(
        ComponentName("light-serve"),
        StubServer {
            behavior: Behavior::ServeUntilCancel,
        },
    );
    let node = ServeComponent::new(
        ComponentName("node-rpc"),
        StubServer {
            behavior: Behavior::FailAfterReady,
        },
    );

    let orchestra = OrchestraBuilder::new()
        .boot(light.clone())
        .await
        .expect("boot light-serve")
        .boot(node.clone())
        .await
        .expect("boot node-rpc") // becomes Ready (notify), then its serve loop dies
        .build();

    let phases: Vec<_> = orchestra.statuses().iter().map(|s| s.lifecycle).collect();
    assert_eq!(phases, vec![Lifecycle::Ready, Lifecycle::Ready]);

    let outcome = tokio::time::timeout(Duration::from_secs(1), orchestra.run())
        .await
        .expect("orchestra ran to a decision");
    assert_eq!(
        outcome,
        RuntimeOutcome::Fatal {
            component: ComponentName("node-rpc")
        }
    );

    // The failure is not just a flag: the component's status carries *why* it is
    // Critical — the serve error's cause — so a health reader learns the reason,
    // not merely that it failed.
    let failed = node.status();
    assert_eq!(failed.health, Health::Critical);
    assert_eq!(failed.reason.as_deref(), Some("stub server failed"));
}

#[tokio::test]
async fn a_bind_failure_fails_to_boot() {
    let node = ServeComponent::new(
        ComponentName("node-rpc"),
        StubServer {
            behavior: Behavior::FailToBind,
        },
    );
    let result = OrchestraBuilder::new().boot(node).await;
    assert!(matches!(
        result,
        Err(BootError::Unready(ComponentName("node-rpc")))
    ));
}
