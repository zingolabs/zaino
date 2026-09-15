//! POC: one engine composes into both public serving ports at once.
//!
//! The runtime's job at the serving edge is to hold a single concrete engine and
//! vend a *profile view* of it to each port adapter. This test shows that shape:
//! one `MockIndexerService` backs both the light-serve (gRPC) adapter and the
//! node-rpc (JSON-RPC) adapter simultaneously, each observing the same pinned
//! chain through its own wire shape.
//!
//! It also pins down the composition requirement the adapters imply: they take
//! the engine by value, so a shared engine must be a cheap-clone handle (here,
//! `MockIndexerService: Clone` is another handle to the same state).

use zaino_core::{BlockHash, BlockId, Height};
use zaino_lightserve::LightServe;
use zaino_noderpc::NodeRpc;
use zaino_service::testing::{MockChain, MockIndexerService};

#[tokio::test]
async fn one_engine_drives_both_ports() {
    let tip = BlockId {
        height: Height::try_from(500).expect("valid height"),
        hash: BlockHash::from([0x11u8; 32]),
    };
    let engine = MockIndexerService::new(MockChain {
        tip: Some(tip),
        ..Default::default()
    });

    // One engine, two profile views: the light-serve port and the node-rpc port.
    let light = LightServe::new(engine.clone());
    let node = NodeRpc::new(engine);

    // Both observe the same pinned tip, each rendering it in its own wire shape.
    let light_tip = light.get_latest_block().await.expect("light latest block");
    assert_eq!(light_tip.height, 500u64);
    assert_eq!(light_tip.hash, vec![0x11u8; 32]);

    assert_eq!(node.get_block_count().await.expect("node block count"), 500);
    assert_eq!(
        node.get_best_block_hash().await.expect("node best hash"),
        "11".repeat(32)
    );
}
