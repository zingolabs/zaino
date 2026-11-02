//! Wallet-tier predicates across the NU7 activation boundary.
//!
//! The schedule is Ironwood from height 2 and NU7 from [`NU7_BOUNDARY`], pinned
//! once in the `TestEnv` builder and adopted by validator, indexer and wallet
//! alike. NU7 changes no transaction format: what a wallet must get right is the
//! consensus branch its signatures commit to, which flips at the boundary. So the
//! cells here are the ones only a wallet on both sides can cover: a transaction
//! signed for NU7 is accepted by the validator and served by zaino, and receipts
//! confirm in the last NU6.3 block and in the activation block alike.

use std::time::Duration;

use anyhow::Result;
use zaino_testutils::ZEBRAD_VERSION;
use ztest::prelude::*;

use e2e::{assert_pool_present, Pool};

const READY: Duration = Duration::from_secs(120);
const SEND_AMOUNT: u64 = 250_000;
/// NU6.3 (Ironwood) from height 2, NU7 from here.
const NU7_BOUNDARY: u32 = 6;

fn nu7_schedule() -> ActivationHeights {
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
        .set_nu6_3(Some(2))
        .set_nu7(Some(NU7_BOUNDARY))
        .build()
}

/// After activation, a send built and signed for the NU7 branch is mined by the
/// validator, served by zaino as Ironwood compact data, and lands in the
/// recipient's Ironwood pool.
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_send_confirms_after_nu7_activation() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(nu7_schedule());
    let validator = env.add_validator(
        Validator::zebrad(ZEBRAD_VERSION)
            .regtest()
            .mine_to(Pool::Orchard.ztest()),
    );
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet
        .funded_faucet_with_notes(&validator, &indexer, 1)
        .await?;

    // Past the boundary before anything is built: the send targets an NU7 block.
    let cur = u32::from(validator.chain_height().await?);
    if cur < NU7_BOUNDARY {
        let tip = validator.generate_blocks(NU7_BOUNDARY - cur).await?;
        indexer.wait_for_block_num(tip, READY).await?;
    }
    faucet.sync().await?;
    assert!(
        faucet.balances().await?.get(Pool::Ironwood.ztest()) > 0,
        "the faucet's coinbase notes are ironwood"
    );

    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(Pool::Orchard.ztest()).await?;
    let txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await?
        .into_iter()
        .next()
        .expect("send returns a txid");
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;

    let balance = recipient.balances().await?;
    assert_eq!(balance.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
    assert_eq!(balance.get(Pool::Orchard.ztest()), 0);

    let mined = indexer.get_block(tip).await?;
    assert!(u32::try_from(mined.height)? > NU7_BOUNDARY);
    assert_pool_present(&mined, &txid, Pool::Ironwood);
    Ok(())
}

/// Receipts confirm on both sides of the boundary: a send confirmed in the last
/// NU6.3 block (boundary − 1), then a send built at that tip but confirmed in the
/// activation block itself, so its signature commits to the NU7 branch. The
/// pre-boundary receipt survives the flip unchanged.
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn receipts_confirm_on_both_sides_of_the_nu7_boundary() -> Result<()> {
    let mut env = TestEnv::builder()
        .ready_timeout(READY)
        .activation_heights(nu7_schedule());
    let validator = env.add_validator(
        Validator::zebrad(ZEBRAD_VERSION)
            .regtest()
            .mine_to(Pool::Orchard.ztest()),
    );
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet
        .funded_faucet_with_notes(&validator, &indexer, 1)
        .await?;

    // Position the tip at boundary − 2, so the first send confirms at boundary − 1.
    let cur = u32::from(validator.chain_height().await?);
    let target = NU7_BOUNDARY - 2;
    if cur < target {
        let tip = validator.generate_blocks(target - cur).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        faucet.sync().await?;
    }

    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(Pool::Orchard.ztest()).await?;
    let last_nu6_3_txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await?
        .into_iter()
        .next()
        .expect("send returns a txid");
    let tip = validator.generate_blocks(1).await?; // boundary − 1
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;
    assert_eq!(
        recipient.balances().await?.get(Pool::Ironwood.ztest()),
        SEND_AMOUNT,
        "receipt confirmed at boundary - 1"
    );

    // Built at boundary − 1, confirmed in the activation block: an NU7 signature.
    faucet.sync().await?;
    let first_nu7_txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await?
        .into_iter()
        .next()
        .expect("send returns a txid");
    let tip = validator.generate_blocks(1).await?; // boundary
    assert_eq!(u32::from(tip), NU7_BOUNDARY);
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;
    assert_eq!(
        recipient.balances().await?.get(Pool::Ironwood.ztest()),
        2 * SEND_AMOUNT,
        "the activation-block receipt lands beside the pre-boundary one"
    );

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    let last_nu6_3_block = blocks
        .iter()
        .find(|b| b.height == u64::from(NU7_BOUNDARY - 1))
        .expect("boundary-1 block served");
    let activation_block = blocks
        .iter()
        .find(|b| b.height == u64::from(NU7_BOUNDARY))
        .expect("boundary block served");
    assert_pool_present(last_nu6_3_block, &last_nu6_3_txid, Pool::Ironwood);
    assert_pool_present(activation_block, &first_nu7_txid, Pool::Ironwood);
    Ok(())
}
