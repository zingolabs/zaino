//! The node-RPC passthrough deployment boots its serving adapter under the
//! Orchestra — the zainod shape for the explorer use case, following
//! `full_boot.rs`.
//!
//! Primitives-free (mock engine): this exercises the deployment's plan
//! (sync-gated readiness, the use-case component name) and that the real
//! `JsonRpcServer` boots in dependency order behind the validator, not the
//! reads. The full indexed assembly (`boot_indexed`) is driven over a real
//! validator client; the compile-time `Serves<NodeRpc>` assertion beside the
//! deployment proves that composition typechecks over the real store and head
//! tiers.

use zaino_component::{ComponentName, Lifecycle};
use zaino_noderpc::{JsonRpcServer, NodeRpc};
use zaino_runtime::deployment::NodeRpcPassthrough;
use zaino_runtime::{
    OrchestraBuilder, ReachabilityProbe, RunComponent, RuntimePlan, ValidatorComponent,
};
use zaino_service::testing::{MockChain, MockIndexerService};
use zaino_service::use_cases::{NodeRpc as NodeRpcUseCase, UseCase};
use zcash_protocol::consensus::Network;

struct Reachable;
impl ReachabilityProbe for Reachable {
    async fn reachable(&self) -> bool {
        true
    }
}

/// The node-RPC adapter boots behind the validator and reaches Ready, under the
/// deployment's own readiness criteria and its use case's component name.
#[tokio::test]
async fn the_node_rpc_deployment_boots_its_server() {
    let validator = ValidatorComponent::connect(&Reachable)
        .await
        .expect("validator reachable");

    let node_rpc = RunComponent::new(
        ComponentName(NodeRpcUseCase::NAME),
        JsonRpcServer::new(
            NodeRpc::new(
                MockIndexerService::new(MockChain::default()),
                Network::MainNetwork,
            ),
            "127.0.0.1:0".parse().expect("valid addr"),
        ),
    );

    let orchestra = OrchestraBuilder::new()
        .with_readiness(NodeRpcPassthrough::READINESS)
        .boot_observed(validator)
        .await
        .boot(node_rpc)
        .await
        .expect("boot node-rpc")
        .build();

    let statuses = orchestra.statuses();
    assert_eq!(
        statuses.iter().map(|s| s.name).collect::<Vec<_>>(),
        vec![
            ComponentName("validator"),
            ComponentName(NodeRpcUseCase::NAME),
        ],
        "the validator boots before the node-RPC server that depends on it",
    );
    assert!(
        statuses.iter().all(|s| s.lifecycle == Lifecycle::Ready),
        "every component reached Ready",
    );

    orchestra.shutdown();
}
