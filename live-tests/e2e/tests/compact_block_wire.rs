//! Wire-tier (zainod gRPC) era tests for compact-block serving.
//!
//! The e2e-exclusive predicate here is **wire fidelity**: the compact blocks a real
//! tonic client receives from a running zainod equal, block for block, what the
//! in-process subscriber produces for the same request. Only this tier crosses the
//! protobuf encode → network → decode boundary and zainod's gRPC server; clientless
//! tests call subscriber methods in-process and can never observe that layer.
//!
//! On top of fidelity, each test asserts the era composition of the served stream for
//! the unfiltered request shape (empty `poolTypes`): orchard-only, ironwood-only, and
//! the orchard→ironwood transition at the NU6.3 activation boundary.

use zaino_common::network::ActivationHeights;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zaino_proto::proto::service::{BlockId, BlockRange};
#[allow(deprecated)]
use zaino_state::FetchService;
use zaino_state::ZcashIndexer as _;
use zaino_testutils::{
    make_uri, MinerPool, TestManager, ValidatorKind, NU6_3_ACTIVE_ACTIVATION_HEIGHTS,
    NU6_3_TRANSITION_BOUNDARY, ORCHARD_ONLY_ACTIVATION_HEIGHTS,
    ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
};
use zcash_local_net::validator::zebrad::Zebrad;

/// Which shielded pool the fixture's coinbase reward must land in at a given height,
/// as observed in the served compact form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoinbaseEra {
    /// Below NU5: the shielded-receiver reward cannot land in either pool.
    Neither,
    /// NU5 through NU6.2: the orchard-receiver reward is paid as Orchard actions.
    Orchard,
    /// From NU6.3: the same receiver's reward is routed to Ironwood actions.
    Ironwood,
}

/// Launches an orchard-receiver mining fixture with zainod enabled, generates
/// `blocks` blocks, streams `[0, tip]` through zainod's real gRPC wire with the empty
/// `poolTypes` filter, and asserts:
///
/// 1. wire fidelity — the streamed blocks equal the in-process subscriber's blocks;
/// 2. era composition — each height's served orchard/ironwood action presence matches
///    the era `expected_era` assigns it.
async fn assert_wire_served_eras(
    activation_heights: ActivationHeights,
    blocks: u32,
    expected_era: impl Fn(u64) -> CoinbaseEra,
) {
    #[allow(deprecated)]
    let mut test_manager = TestManager::<Zebrad, FetchService>::launch_mining_to(
        MinerPool::Orchard,
        &ValidatorKind::Zebrad,
        None,
        Some(activation_heights),
        None,
        true,
        false,
        false,
    )
    .await
    .unwrap();
    let subscriber = test_manager.subscriber().clone();

    test_manager
        .generate_blocks_and_wait_for_tip(blocks, &subscriber)
        .await;
    let tip = u64::from(subscriber.chain_height().await.unwrap().0);

    // The in-process serving result: the same request answered without the wire.
    let in_process = zaino_testutils::collect_block_range(&subscriber, 0, tip, vec![]).await;

    // The same request through zainod's real gRPC server.
    let uri = make_uri(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    );
    let mut grpc_client = CompactTxStreamerClient::connect(uri)
        .await
        .expect("zainod grpc reachable");
    let mut stream = grpc_client
        .get_block_range(BlockRange {
            start: Some(BlockId {
                height: 0,
                hash: vec![],
            }),
            end: Some(BlockId {
                height: tip,
                hash: vec![],
            }),
            // The unfiltered wire request real (including pre-Ironwood) clients send.
            pool_types: vec![],
        })
        .await
        .expect("get_block_range succeeds")
        .into_inner();
    let mut wire: Vec<CompactBlock> = Vec::new();
    while let Some(block) = stream.message().await.expect("stream yields blocks") {
        wire.push(block);
    }

    // E2e-exclusive predicate — wire fidelity: the protobuf encode/decode and zainod's
    // gRPC server must be transparent.
    assert_eq!(
        wire.len(),
        in_process.len(),
        "wire and in-process must serve the same block count"
    );
    for (wire_block, in_process_block) in wire.iter().zip(&in_process) {
        assert_eq!(
            wire_block, in_process_block,
            "wire round-trip must be transparent at height {}",
            wire_block.height
        );
    }

    // Era composition of the served stream. Observed failing at the first
    // ironwood-era height (served orchard 1 / ironwood 0) — hypotheses and the
    // raw-block disambiguation procedure are tracked in
    // <https://github.com/zingolabs/zaino/issues/1368>.
    for block in &wire {
        let orchard_actions: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood_actions: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        let (want_orchard, want_ironwood) = match expected_era(block.height) {
            CoinbaseEra::Neither => (false, false),
            CoinbaseEra::Orchard => (true, false),
            CoinbaseEra::Ironwood => (false, true),
        };
        assert_eq!(
            orchard_actions > 0,
            want_orchard,
            "served orchard actions mismatch at height {} (orchard {orchard_actions}, \
             ironwood {ironwood_actions})",
            block.height
        );
        assert_eq!(
            ironwood_actions > 0,
            want_ironwood,
            "served ironwood actions mismatch at height {} (orchard {orchard_actions}, \
             ironwood {ironwood_actions})",
            block.height
        );
    }

    test_manager.close().await;
}

/// Orchard-only era over the wire: NU6.3 never activates (explicit fixture — the
/// zebrad default heights are now the canonical NU6.3-at-2 set), so the served
/// stream carries Orchard actions from height 2 and no ironwood anywhere.
///
/// multi_thread required: the test manager spawns the validator, indexer, and zainod.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_wire_serving_zebrad() {
    assert_wire_served_eras(ORCHARD_ONLY_ACTIVATION_HEIGHTS, 6, |height| {
        if height >= 2 {
            CoinbaseEra::Orchard
        } else {
            CoinbaseEra::Neither
        }
    })
    .await;
}

/// Ironwood-only era over the wire: NU6.3 active from height 2, so the served stream
/// carries Ironwood actions from height 2 and no Orchard actions anywhere.
///
/// multi_thread required: the test manager spawns the validator, indexer, and zainod.
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_wire_serving_zebrad() {
    assert_wire_served_eras(NU6_3_ACTIVE_ACTIVATION_HEIGHTS, 6, |height| {
        if height >= 2 {
            CoinbaseEra::Ironwood
        } else {
            CoinbaseEra::Neither
        }
    })
    .await;
}

/// The transition over the wire: the same unchanged orchard-receiver miner's served
/// stream flips from Orchard to Ironwood actions exactly at the NU6.3 activation
/// height.
///
/// multi_thread required: the test manager spawns the validator, indexer, and zainod.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_to_ironwood_transition_wire_serving_zebrad() {
    // Two blocks past the boundary, so both eras carry more than one block.
    assert_wire_served_eras(
        ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
        NU6_3_TRANSITION_BOUNDARY + 2,
        |height| {
            if height >= u64::from(NU6_3_TRANSITION_BOUNDARY) {
                CoinbaseEra::Ironwood
            } else if height >= 2 {
                CoinbaseEra::Orchard
            } else {
                CoinbaseEra::Neither
            }
        },
    )
    .await;
}
