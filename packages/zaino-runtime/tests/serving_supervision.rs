//! POC: the runtime supervises servers through the `Serve` seam.
//!
//! Each server is a `Serve` impl wrapped in a `ServeComponent` and handed to the
//! Orchestra, which boots them in order and babysits them. Health is driven by
//! the serve task's outcome: a server whose serve loop fails goes `Critical`,
//! the escalation funnels up, and under the everything-fatal policy it brings
//! the app down naming the offender. No health is injected — the failure is
//! real.

use std::sync::Arc;
use std::time::Duration;

use zaino_component::{CancellationToken, ComponentName, Lifecycle};
use zaino_runtime::{OrchestraBuilder, RuntimeOutcome, Serve, ServeComponent};

/// A stub transport server: serves until cancelled, or fails to start if `fail`.
/// Stands in for a real tonic / jsonrpsee server over a profile handle.
struct StubServer {
    fail: bool,
}

#[derive(Debug, thiserror::Error)]
#[error("stub server failed to start")]
struct StubError;

impl Serve for StubServer {
    type Error = StubError;

    async fn serve(self: Arc<Self>, cancel: CancellationToken) -> Result<(), StubError> {
        if self.fail {
            return Err(StubError);
        }
        cancel.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn a_failing_server_escalates_and_is_fatal() {
    let light = ServeComponent::new(ComponentName("light-serve"), StubServer { fail: false });
    let node = ServeComponent::new(ComponentName("node-rpc"), StubServer { fail: true });

    let orchestra = OrchestraBuilder::new()
        .boot(light.clone())
        .await
        .expect("boot light-serve")
        .boot(node.clone())
        .await
        .expect("boot node-rpc")
        .build();

    // Both reached Ready (readiness is a lifecycle fact; node's serve loop then
    // fails, flipping its health rather than its phase).
    let phases: Vec<_> = orchestra.statuses().iter().map(|s| s.lifecycle).collect();
    assert_eq!(phases, vec![Lifecycle::Ready, Lifecycle::Ready]);

    // node-rpc's serve task failed → Critical → escalation → fatal, naming it.
    let outcome = tokio::time::timeout(Duration::from_secs(1), orchestra.run())
        .await
        .expect("orchestra ran to a decision");
    assert_eq!(
        outcome,
        RuntimeOutcome::Fatal {
            component: ComponentName("node-rpc")
        }
    );
}
