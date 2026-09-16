//! The whole runtime booting as one — the zainod-main shape.
//!
//! Connect the validator (observed root), compose the engine, wrap each public
//! port as an owned component, and boot them in dependency order under a single
//! Orchestra: validator first (ADR-0014), then the servers that depend on it.
//! Primitives-free (mock engine) — this exercises the composition and lifecycle,
//! not the reads.

use std::time::Duration;

use zaino_component::{ComponentName, Health, Lifecycle};
use zaino_lightserve::{GrpcServer, LightServe};
use zaino_noderpc::{JsonRpcServer, NodeRpc};
use zaino_runtime::{
    Orchestra, OrchestraBuilder, RuntimeOutcome, ServeComponent, ValidatorComponent, ValidatorProbe,
};
use zaino_service::testing::{MockChain, MockIndexerService};

struct Probe(bool);
impl ValidatorProbe for Probe {
    async fn reachable(&self) -> bool {
        self.0
    }
}

/// The zainod-main shape: gate on the validator, compose the engine, wrap each
/// port as a component, boot in dependency order under one Orchestra.
async fn boot_zaino(engine: MockIndexerService) -> (ValidatorComponent, Orchestra) {
    // Root: confirm the validator is live before anything depends on it.
    let validator = ValidatorComponent::connect(&Probe(true))
        .await
        .expect("validator reachable");

    // Public ports, each a real server over the one engine.
    let light_serve = ServeComponent::new(
        ComponentName("light-serve"),
        GrpcServer::new(
            LightServe::new(engine.clone()),
            "127.0.0.1:0".parse().expect("valid addr"),
        ),
    );
    let node_rpc = ServeComponent::new(
        ComponentName("node-rpc"),
        JsonRpcServer::new(
            NodeRpc::new(engine),
            "127.0.0.1:0".parse().expect("valid addr"),
        ),
    );

    let orchestra = OrchestraBuilder::new()
        .boot_observed(validator.clone())
        .await
        .boot(light_serve)
        .await
        .expect("boot light-serve")
        .boot(node_rpc)
        .await
        .expect("boot node-rpc")
        .build();

    (validator, orchestra)
}

#[tokio::test]
async fn the_whole_runtime_boots_in_dependency_order() {
    let (_validator, orchestra) = boot_zaino(MockIndexerService::new(MockChain::default())).await;

    let statuses = orchestra.statuses();
    assert_eq!(
        statuses.iter().map(|s| s.name).collect::<Vec<_>>(),
        vec![
            ComponentName("validator"),
            ComponentName("light-serve"),
            ComponentName("node-rpc"),
        ],
        "validator boots before the servers that depend on it",
    );
    assert!(
        statuses.iter().all(|s| s.lifecycle == Lifecycle::Ready),
        "every component reached Ready",
    );

    orchestra.shutdown();
}

#[tokio::test]
async fn the_signals_track_the_runtime() {
    let (validator, orchestra) = boot_zaino(MockIndexerService::new(MockChain::default())).await;
    let mut signals = orchestra.signals();

    // Booted: startup latched, ready, live.
    let booted = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let s = *signals.borrow();
            if s.started && s.ready && s.live {
                return;
            }
            if signals.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(booted.is_ok(), "runtime reaches started + ready");

    // The validator drops -> readiness falls, but startup stays latched (it
    // booted; this is a readiness change, not an un-boot).
    validator.report_health(Health::Critical);
    let dropped = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !signals.borrow().ready {
                return;
            }
            if signals.changed().await.is_err() {
                return;
            }
        }
    })
    .await;
    assert!(dropped.is_ok(), "a critical component drops readiness");
    let s = *signals.borrow();
    assert!(!s.ready);
    assert!(s.started, "startup stays latched after boot");

    orchestra.shutdown();
}

#[tokio::test]
async fn a_validator_drop_brings_the_whole_app_down() {
    let (validator, orchestra) = boot_zaino(MockIndexerService::new(MockChain::default())).await;

    // The root falls over → escalation → everything-fatal, naming the root.
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
