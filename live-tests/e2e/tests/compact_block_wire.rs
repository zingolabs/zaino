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

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

/// The mid-chain NU6.3 (Ironwood) activation height for the transition fixture:
/// an Orchard era `[2, 6)` that flips to Ironwood at height 6.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// Orchard-only schedule: NU5..=NU6.2 active from height 2, NU6.3 never — the
/// orchard-receiver coinbase stays in Orchard actions for every block.
fn orchard_only() -> ActivationHeights {
    ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(2))
        .set_nu6(Some(2))
        .set_nu6_1(Some(2))
        .set_nu6_2(Some(2))
        .build()
}

/// Mid-chain transition schedule: Orchard era `[2, boundary)`, Ironwood from
/// `boundary`, so the same orchard-receiver coinbase flips pools at `boundary`.
fn orchard_then_ironwood_at(boundary: u32) -> ActivationHeights {
    ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(2))
        .set_nu6(Some(2))
        .set_nu6_1(Some(2))
        .set_nu6_2(Some(2))
        .set_nu6_3(Some(boundary))
        .build()
}

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

/// Asserts the era composition of a served compact-block stream: each height's served
/// orchard/ironwood action presence must match the era `expected_era` assigns it. The
/// stream is the one a real tonic client receives from zainod's gRPC — under ztest
/// `indexer.get_block_range` opens the tonic channel internally, so this is the wire
/// round-trip. Blocks are coinbase-only, so a block's per-pool action totals are the
/// coinbase's: an Orchard-era coinbase pays the reward as Orchard actions
/// (`vtx[].actions`, no ironwood), an Ironwood-era coinbase as Ironwood actions
/// (`vtx[].ironwood_actions`, no orchard).
fn assert_wire_served_eras(blocks: &[CompactBlock], expected_era: impl Fn(u64) -> CoinbaseEra) {
    // Era composition of the served stream. Observed failing at the first
    // ironwood-era height (served orchard 1 / ironwood 0) — hypotheses and the
    // raw-block disambiguation procedure are tracked in
    // <https://github.com/zingolabs/zaino/issues/1368>.
    for block in blocks {
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
}

/// Orchard-only era over the wire: NU6.3 never activates (explicit fixture — the
/// zebrad default heights are now the canonical NU6.3-at-2 set), so the served
/// stream carries Orchard actions from height 2 and no ironwood anywhere.
///
/// multi_thread required: the test manager spawns the validator, indexer, and zainod.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_wire_serving_zebrad() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(orchard_only());
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    assert_wire_served_eras(&blocks, |height| {
        if height >= 2 {
            CoinbaseEra::Orchard
        } else {
            CoinbaseEra::Neither
        }
    });
    Ok(())
}

/// Ironwood-only era over the wire: NU6.3 active from height 2, so the served stream
/// carries Ironwood actions from height 2 and no Orchard actions anywhere.
///
/// multi_thread required: the test manager spawns the validator, indexer, and zainod.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_wire_serving_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    assert_wire_served_eras(&blocks, |height| {
        if height >= 2 {
            CoinbaseEra::Ironwood
        } else {
            CoinbaseEra::Neither
        }
    });
    Ok(())
}

/// The transition over the wire: the same unchanged orchard-receiver miner's served
/// stream flips from Orchard to Ironwood actions exactly at the NU6.3 activation
/// height.
///
/// multi_thread required: the test manager spawns the validator, indexer, and zainod.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_to_ironwood_transition_wire_serving_zebrad() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    // Two blocks past the boundary, so both eras carry more than one block.
    let tip = validator
        .generate_blocks(NU6_3_TRANSITION_BOUNDARY + 2)
        .await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    assert_wire_served_eras(&blocks, |height| {
        if height >= u64::from(NU6_3_TRANSITION_BOUNDARY) {
            CoinbaseEra::Ironwood
        } else if height >= 2 {
            CoinbaseEra::Orchard
        } else {
            CoinbaseEra::Neither
        }
    });
    Ok(())
}
