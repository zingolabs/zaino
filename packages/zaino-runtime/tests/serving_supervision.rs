//! POC: the runtime boots and supervises both serve adapters as components.
//!
//! Each port adapter is wrapped in a `ServeComponent` and handed to the
//! Orchestra, which boots them in order (each `Ready` before the next) and
//! babysits them. When one server's health goes Critical, the escalation funnels
//! up and — under the everything-fatal policy — brings the app down naming the
//! offender. This is the serving layer's status/lifecycle owned by the runtime,
//! not the servers.

use std::time::Duration;

use zaino_component::{ComponentName, Health, Lifecycle};
use zaino_core::{BlockHash, BlockId, Height};
use zaino_lightserve::LightServe;
use zaino_noderpc::NodeRpc;
use zaino_runtime::{OrchestraBuilder, RuntimeOutcome, ServeComponent};
use zaino_service::testing::{MockChain, MockIndexerService};

#[tokio::test]
async fn runtime_boots_and_supervises_both_serve_components() {
    let engine = MockIndexerService::new(MockChain {
        tip: Some(BlockId {
            height: Height::try_from(1).expect("valid height"),
            hash: BlockHash::from([0u8; 32]),
        }),
        ..Default::default()
    });

    // One engine, two serve components (one per port), each a supervised component.
    let light = ServeComponent::new(
        ComponentName("light-serve"),
        LightServe::new(engine.clone()),
    );
    let node = ServeComponent::new(ComponentName("node-rpc"), NodeRpc::new(engine));

    let orchestra = OrchestraBuilder::new()
        .boot(light.clone())
        .await
        .expect("boot light-serve")
        .boot(node.clone())
        .await
        .expect("boot node-rpc")
        .build();

    // Both booted through to Ready, in order.
    let phases: Vec<_> = orchestra.statuses().iter().map(|s| s.lifecycle).collect();
    assert_eq!(phases, vec![Lifecycle::Ready, Lifecycle::Ready]);

    // The node-rpc server falls over → escalation funnels up, fatal, naming it.
    node.signal_health(Health::Critical);
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
