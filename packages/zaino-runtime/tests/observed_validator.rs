//! The validator as an observed component: gates bringup, escalates on drop.

use std::time::Duration;

use zaino_component::{ComponentName, Health, Lifecycle};
use zaino_lightserve::{GrpcServer, LightServe};
use zaino_runtime::{
    OrchestraBuilder, RuntimeOutcome, ServeComponent, ValidatorComponent, ValidatorProbe,
    ValidatorUnreachable,
};
use zaino_service::testing::{MockChain, MockIndexerService};

/// A stub reachability probe.
struct Probe(bool);
impl ValidatorProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

#[tokio::test]
async fn an_unreachable_validator_fails_to_connect() {
    let result = ValidatorComponent::connect(&Probe(false)).await;
    assert!(matches!(result, Err(ValidatorUnreachable)));
}

#[tokio::test]
async fn a_validator_drop_escalates_and_is_fatal() {
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("connect");
    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator.clone())
        .await
        .build();

    // Booted Ready, observed (not owned).
    assert_eq!(orchestra.statuses()[0].lifecycle, Lifecycle::Ready);

    // The source layer reports the validator dropped → escalation → fatal.
    validator.report_health(Health::Critical);
    let outcome = tokio::time::timeout(Duration::from_secs(1), orchestra.run())
        .await
        .expect("orchestra ran to a decision");
    assert_eq!(
        outcome,
        RuntimeOutcome::Fatal {
            component: ComponentName("validator")
        }
    );
}

#[tokio::test]
async fn the_validator_boots_before_the_servers() {
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("connect");
    let grpc = ServeComponent::new(
        ComponentName("light-serve"),
        GrpcServer::new(
            LightServe::new(MockIndexerService::new(MockChain::default())),
            "127.0.0.1:0".parse().expect("valid addr"),
        ),
    );

    // Root first (observed), then the owned server that depends on it.
    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator)
        .await
        .boot(grpc)
        .await
        .expect("boot light-serve")
        .build();

    let names: Vec<_> = orchestra.statuses().iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        vec![ComponentName("validator"), ComponentName("light-serve")]
    );
    orchestra.shutdown();
}
