//! Era composition of the compact blocks a real tonic client receives from a
//! running zainod: the served range is complete and contiguous, and each height's
//! coinbase pays its era's pool (orchard-only, ironwood-only, and the
//! orchard→ironwood flip at the NU6.3 activation height).
//!
//! Known failure: served ironwood actions are missing at the first ironwood-era
//! height — <https://github.com/zingolabs/zaino/issues/1368>.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

/// The mid-chain NU6.3 (Ironwood) activation height for the transition fixture:
/// an Orchard era `[2, 6)` that flips to Ironwood at height 6.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// NU6.3 never activates, so the orchard-receiver coinbase stays in Orchard
/// actions for every block.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_wire_serving_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(
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
            .build(),
    );
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "zainod must serve every block in [1, {tip}]"
    );

    for (offset, block) in blocks.iter().enumerate() {
        let height = 1 + offset as u64;
        assert_eq!(block.height, height, "served heights must be contiguous");
        let orchard: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        assert_eq!(
            orchard > 0,
            height >= 2,
            "orchard actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
        assert_eq!(
            ironwood, 0,
            "no ironwood actions anywhere at height {height} (orchard {orchard})"
        );
    }
    Ok(())
}

/// The zebrad default heights activate NU6.3 at height 2, so the orchard-receiver
/// coinbase is paid as Ironwood actions from the first shielded block.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_wire_serving_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(6).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "zainod must serve every block in [1, {tip}]"
    );

    for (offset, block) in blocks.iter().enumerate() {
        let height = 1 + offset as u64;
        assert_eq!(block.height, height, "served heights must be contiguous");
        let orchard: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        assert_eq!(
            ironwood > 0,
            height >= 2,
            "ironwood actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
        assert_eq!(
            orchard, 0,
            "no orchard actions anywhere at height {height} (ironwood {ironwood})"
        );
    }
    Ok(())
}

/// The unchanged orchard-receiver miner's served stream flips from Orchard to
/// Ironwood actions exactly at the NU6.3 activation height.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_to_ironwood_transition_wire_serving_zebrad() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(
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
            .set_nu6_3(Some(NU6_3_TRANSITION_BOUNDARY))
            .build(),
    );
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    // Two blocks past the boundary, so both eras carry more than one block.
    let mined = NU6_3_TRANSITION_BOUNDARY + 2;
    let tip = validator.generate_blocks(mined).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert_eq!(
        blocks.len() as u64,
        u64::from(tip),
        "zainod must serve every block in [1, {tip}]"
    );

    for (offset, block) in blocks.iter().enumerate() {
        let height = 1 + offset as u64;
        assert_eq!(block.height, height, "served heights must be contiguous");
        let orchard: usize = block.vtx.iter().map(|tx| tx.actions.len()).sum();
        let ironwood: usize = block.vtx.iter().map(|tx| tx.ironwood_actions.len()).sum();
        let boundary = u64::from(NU6_3_TRANSITION_BOUNDARY);
        assert_eq!(
            orchard > 0,
            (2..boundary).contains(&height),
            "orchard actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
        assert_eq!(
            ironwood > 0,
            height >= boundary,
            "ironwood actions at height {height} (orchard {orchard}, ironwood {ironwood})"
        );
    }
    Ok(())
}
