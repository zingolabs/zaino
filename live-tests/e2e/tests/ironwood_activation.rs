//! Wallet-observable predicates across a mid-chain Orchard → Ironwood boundary (NU6.3 at 6)
//!
//! - Schedule pinned once (`TestEnv` activation heights); validator, zainod, wallet adopt it
//! - Ironwood-era cells: `wallet_to_validator.rs`; served-chain era composition:
//!   `clientless/tests/compact_block_consistency.rs`
//! - Hermetic only (public testnet: no pre-activation Orchard TAZ, cross-address restriction)

use std::time::Duration;

use anyhow::{Context, Result};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee, one-note shield round
const SHIELD_FEE: u64 = 15_000;
/// NU6.3 activation: `2..6` Orchard era, `6..` Ironwood era
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// ZIP 318 migration: pre-boundary Orchard note spent past it → Ironwood receipt, Orchard empty
///
/// - Faucet's Orchard balance shrinks (cross-address restriction: a real Orchard spend)
/// - Validator `valuePools`: Orchard grows only below the boundary, Ironwood only from it
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn orchard_note_spends_to_ironwood_across_boundary() -> Result<()> {
    let nu6_3_at_boundary = ActivationHeights::builder()
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
        .build();
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(nu6_3_at_boundary);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let boundary = NU6_3_TRANSITION_BOUNDARY as usize;
    let migration_height = boundary + 1;
    // Validator value pools by height (0, 1 = pre-NU5, unread)
    let mut orchard = vec![0u64; migration_height + 1];
    let mut ironwood = vec![0u64; migration_height + 1];
    // Missing pool = failure, never zero (else below-boundary ironwood checks pass vacuously)
    let pool_zats = |info: &serde_json::Value, pool_id: &str| -> Result<u64> {
        info.get("valuePools")
            .and_then(serde_json::Value::as_array)
            .context("getblockchaininfo.valuePools")?
            .iter()
            .find(|pool| pool.get("id").and_then(serde_json::Value::as_str) == Some(pool_id))
            .with_context(|| format!("valuePools has no {pool_id} entry"))?
            .get("chainValueZat")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| format!("valuePools[{pool_id}].chainValueZat"))
    };

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let vrpc = validator.json_rpc().await?;
    let info = vrpc.call_value("getblockchaininfo", serde_json::json!([])).await?;
    orchard[2] = pool_zats(&info, "orchard")?;
    ironwood[2] = pool_zats(&info, "ironwood")?;

    let pre_boundary = faucet.balances().await?;
    assert!(pre_boundary.orchard > 0, "pre-boundary coinbase = orchard: {pre_boundary:?}");

    for height in 3..=boundary {
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let info = vrpc.call_value("getblockchaininfo", serde_json::json!([])).await?;
        orchard[height] = pool_zats(&info, "orchard")?;
        ironwood[height] = pool_zats(&info, "ironwood")?;
    }

    faucet.sync().await?;
    let crossed = faucet.balances().await?;
    let orchard_before_send = crossed.orchard;
    assert!(crossed.ironwood > 0, "boundary coinbase = ironwood: {crossed:?}");

    // Migration send: Orchard note → unified-address (Ironwood) receipt
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(Pool::Orchard).await?;
    faucet
        .send_from(&[Pool::Orchard], &ua, SEND_AMOUNT)
        .await
        .with_context(|| format!("send from Orchard notes to {ua}"))?;
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let info = vrpc.call_value("getblockchaininfo", serde_json::json!([])).await?;
    orchard[migration_height] = pool_zats(&info, "orchard")?;
    ironwood[migration_height] = pool_zats(&info, "ironwood")?;
    faucet.sync().await?;
    recipient.sync().await?;

    let balance = recipient.balances().await?;
    let receipt = (balance.ironwood, balance.orchard);
    assert_eq!(receipt, (SEND_AMOUNT, 0), "(ironwood, orchard) migration receipt: {balance:?}");
    let orchard_after_send = faucet.balances().await?.orchard;
    assert!(
        orchard_after_send < orchard_before_send,
        "migration send spends an orchard note: faucet orchard {orchard_before_send} → \
         {orchard_after_send}",
    );

    for (height, &value) in ironwood.iter().enumerate().take(boundary).skip(2) {
        assert_eq!(value, 0, "ironwood pool empty at {height} (below the boundary)");
    }
    assert!(orchard[2] > 0, "first pre-boundary coinbase funds the orchard pool: {orchard:?}");
    for height in 3..boundary {
        let (now, before) = (orchard[height], orchard[height - 1]);
        assert!(now > before, "orchard pool grows at {height}: {now} after {before}");
    }
    assert_eq!(orchard[boundary], orchard[boundary - 1], "orchard pool steady across the edge");
    assert!(ironwood[boundary] > 0, "activation coinbase funds the ironwood pool: {ironwood:?}");
    let (edge, migrated) = (orchard[boundary], orchard[migration_height]);
    assert!(migrated < edge, "migration shrinks orchard: {edge} → {migrated}");
    Ok(())
}

/// - Confirmed at boundary − 1 → Orchard; built there, confirmed at the boundary → Ironwood
/// - Second send spends an Orchard note (activation block carries both pools' data)
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn receipts_flip_pools_exactly_at_the_boundary() -> Result<()> {
    let nu6_3_at_boundary = ActivationHeights::builder()
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
        .build();
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(nu6_3_at_boundary);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let faucet_balance = faucet.balances().await?;
    let notes = (faucet_balance.orchard > 0, faucet_balance.ironwood);
    assert_eq!(notes, (true, 0), "pre-boundary coinbase = orchard: {faucet_balance:?}");

    // Tip → boundary − 2 (first send confirms in the last Orchard block)
    let cur = u32::from(validator.chain_height().await?);
    let target = NU6_3_TRANSITION_BOUNDARY - 2;
    if cur < target {
        let tip = validator.generate_blocks(target - cur).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        faucet.sync().await?;
    }

    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(Pool::Orchard).await?;
    let last_orchard_txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await
        .with_context(|| format!("send to {ua} at boundary − 2"))?
        .into_iter()
        .next()
        .context("send at boundary − 2 returned no txid")?;
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;

    let balance = recipient.balances().await?;
    let receipt = (balance.orchard, balance.ironwood);
    assert_eq!(receipt, (SEND_AMOUNT, 0), "receipt at boundary - 1 = orchard: {balance:?}");

    // Built at boundary − 1 (spends an Orchard note), confirmed in the activation block
    faucet.sync().await?;
    let first_ironwood_txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await
        .with_context(|| format!("send to {ua} at boundary − 1"))?
        .into_iter()
        .next()
        .context("send at boundary − 1 returned no txid")?;
    let tip = validator.generate_blocks(1).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;

    let balance = recipient.balances().await?;
    let receipts = (balance.ironwood, balance.orchard);
    assert_eq!(receipts, (SEND_AMOUNT, SEND_AMOUNT), "pre-boundary receipt survives the flip");

    let blocks = indexer.get_block_range(BlockHeight::from(1u32), tip).await?;
    let last_orchard_block = blocks
        .iter()
        .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY - 1))
        .context("boundary − 1 block served")?;
    let activation_block = blocks
        .iter()
        .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY))
        .context("boundary block served")?;
    let last_orchard_tx = last_orchard_block
        .vtx
        .iter()
        .find(|tx| tx.txid == last_orchard_txid.as_ref().to_vec())
        .with_context(|| format!("boundary − 1 block carries Orchard-era {last_orchard_txid}"))?;
    let first_ironwood_tx = activation_block
        .vtx
        .iter()
        .find(|tx| tx.txid == first_ironwood_txid.as_ref().to_vec())
        .with_context(|| format!("boundary block carries Ironwood-era {first_ironwood_txid}"))?;
    let last_orchard = (last_orchard_tx.actions.len(), last_orchard_tx.ironwood_actions.len());
    assert!(
        last_orchard.0 > 0 && last_orchard.1 == 0,
        "{last_orchard_txid}: (orchard, ironwood) actions {last_orchard:?}, want orchard only"
    );
    // Migration-shaped: the Orchard spend's data rides along
    let first_ironwood =
        (first_ironwood_tx.actions.len(), first_ironwood_tx.ironwood_actions.len());
    assert!(
        first_ironwood.0 > 0 && first_ironwood.1 > 0,
        "{first_ironwood_txid}: (orchard, ironwood) actions {first_ironwood:?}, want both"
    );
    Ok(())
}

/// Below the boundary: `shield` → Orchard net of `SHIELD_FEE`, Ironwood empty
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn shield_deposits_to_orchard_before_boundary() -> Result<()> {
    let nu6_3_at_boundary = ActivationHeights::builder()
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
        .build();
    let mut env = TestEnv::builder().ready_timeout(READY).activation_heights(nu6_3_at_boundary);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let taddr = recipient.address(Pool::Transparent).await?;
    faucet.send(&taddr, SEND_AMOUNT).await.with_context(|| format!("send to {taddr}"))?;

    // Transparent receipt confirmed at 4
    let cur = u32::from(validator.chain_height().await?);
    let target = NU6_3_TRANSITION_BOUNDARY - 2;
    let tip = validator.generate_blocks(target.saturating_sub(cur).max(1)).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;
    let received = recipient.balances().await?;
    assert_eq!(received.transparent, SEND_AMOUNT, "premise: transparent receipt {received:?}");

    recipient.shield().await.context("shield the transparent receipt")?;
    let tip = validator.generate_blocks(1).await?; // height 5, still Orchard era
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;

    let balance = recipient.balances().await?;
    let shielded = (balance.orchard, balance.ironwood);
    assert_eq!(shielded, (SEND_AMOUNT - SHIELD_FEE, 0), "shield below NU6.3: orchard, net fee");
    Ok(())
}
