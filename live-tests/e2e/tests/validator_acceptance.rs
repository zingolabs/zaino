//! - every test stands up a regtest zebrad pod and a zainod pod built from `../../Dockerfile`
//! - the wallet-tier tests run an in-process librustzcash wallet that submits transactions
//!   to the validator
//! - block generation pays coinbase to the faucet, or to `FILLER_ADDRESS` where the faucet
//!   must gain no new note

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::json;
use ztest::prelude::*;

use e2e::Pool;

const READY: Duration = Duration::from_secs(120);
const SEND_AMOUNT: u64 = 250_000;
const SPEND_AMOUNT: u64 = 20_000;
const SPEND_FEE_CEILING: u64 = 15_000;
const FUND: Pool = Pool::Orchard;
const CREDITED: Pool = Pool::Ironwood;
const MEMPOOL_INGEST_BOUND: Duration = Duration::from_secs(10);
const MEMPOOL_POLL: Duration = Duration::from_millis(500);
const CONVERGENCE_ROUNDS: u32 = 5;
const CONVERGENCE_BOUND: Duration = Duration::from_secs(15);
const CONVERGENCE_POLL: Duration = Duration::from_millis(250);
const AGED_NOTE_BLOCKS: u32 = 3;
const AGED_COINBASE_BLOCKS: u32 = 10;

#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn indexer_mempool_view_trails_validator_acceptance() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(CREDITED.ztest()).await?;
    let txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await?
        .into_iter()
        .next()
        .context("a single send yields exactly one txid")?;
    let accepted_at = Instant::now();

    let vrpc = validator.json_rpc().await?;
    let validator_mempool = vrpc.call_value("getrawmempool", json!([])).await?;
    let txid_hex = txid.to_string();
    anyhow::ensure!(
        validator_mempool
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(&txid_hex))),
        "the validator's own mempool must hold {txid_hex} as soon as the send returns: \
         {validator_mempool}"
    );

    let lag = loop {
        let elapsed = accepted_at.elapsed();
        anyhow::ensure!(
            elapsed < MEMPOOL_INGEST_BOUND,
            "the validator accepted {txid_hex} {elapsed:?} ago and the indexer's mempool still \
             does not show it"
        );
        let in_indexer = indexer
            .get_mempool_tx(Vec::new())
            .await?
            .iter()
            .any(|tx| tx.txid.as_slice() == txid.as_ref());
        if in_indexer {
            break accepted_at.elapsed();
        }
        tokio::time::sleep(MEMPOOL_POLL).await;
    };
    println!("indexer mempool ingestion lag after validator acceptance: {lag:?}");
    Ok(())
}

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn indexer_converges_with_validator_after_block_generation() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    for round in 1..=CONVERGENCE_ROUNDS {
        let tip = validator.generate_blocks(1).await?;
        let generated_at = Instant::now();
        loop {
            let served = indexer.latest_block_height().await?;
            if served >= tip {
                break;
            }
            let elapsed = generated_at.elapsed();
            anyhow::ensure!(
                elapsed < CONVERGENCE_BOUND,
                "round {round}: {elapsed:?} after block {tip} the indexer still serves {served}"
            );
            tokio::time::sleep(CONVERGENCE_POLL).await;
        }
    }
    Ok(())
}

#[rstest]
#[case::tip_note_to_ironwood(Pool::Ironwood, 0)]
#[case::tip_note_to_sapling(Pool::Sapling, 0)]
#[case::aged_note_to_ironwood(Pool::Ironwood, AGED_NOTE_BLOCKS)]
#[case::aged_note_to_sapling(Pool::Sapling, AGED_NOTE_BLOCKS)]
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn received_note_spends_to_receiver(#[case] target: Pool, #[case] age: u32) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(CREDITED.ztest()).await?;
    faucet.send(&ua, SEND_AMOUNT).await?;
    let tip = validator.generate_blocks(1 + age).to(FILLER_ADDRESS).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;
    let spendable = recipient.balances().await?.get(CREDITED.ztest());
    anyhow::ensure!(
        spendable >= SPEND_AMOUNT + SPEND_FEE_CEILING,
        "the recipient holds {spendable} spendable in {CREDITED:?} after syncing to {tip}"
    );

    let target_addr = faucet.address(target.ztest()).await?;
    let txids = recipient.send(&target_addr, SPEND_AMOUNT).await.with_context(|| {
        format!("the validator must accept a spend of a note aged {age} to {target:?}")
    })?;
    anyhow::ensure!(!txids.is_empty(), "the spend to {target:?} returned no txid");
    let vrpc = validator.json_rpc().await?;
    let validator_mempool = vrpc.call_value("getrawmempool", json!([])).await?;
    for txid in &txids {
        let txid_hex = txid.to_string();
        anyhow::ensure!(
            validator_mempool
                .as_array()
                .is_some_and(|entries| entries.iter().any(|e| e.as_str() == Some(&txid_hex))),
            "the validator's mempool must hold the accepted spend {txid_hex}: {validator_mempool}"
        );
    }
    Ok(())
}

#[rstest]
#[case::young_coinbase_to_ironwood(Pool::Ironwood, 0)]
#[case::young_coinbase_to_sapling(Pool::Sapling, 0)]
#[case::aged_coinbase_to_ironwood(Pool::Ironwood, AGED_COINBASE_BLOCKS)]
#[case::aged_coinbase_to_sapling(Pool::Sapling, AGED_COINBASE_BLOCKS)]
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn coinbase_spends_to_receiver(#[case] target: Pool, #[case] age: u32) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    if age > 0 {
        let tip = validator.generate_blocks(age).to(FILLER_ADDRESS).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        faucet.sync().await?;
    }
    let spendable = faucet.balances().await?.get(CREDITED.ztest());
    anyhow::ensure!(
        spendable >= SPEND_AMOUNT + SPEND_FEE_CEILING,
        "the faucet holds {spendable} spendable in {CREDITED:?} with its coinbase aged {age}"
    );

    let recipient = wallet.recipient(&validator, &indexer).await?;
    let target_addr = recipient.address(target.ztest()).await?;
    let txids = faucet.send(&target_addr, SPEND_AMOUNT).await.with_context(|| {
        format!(
            "the validator must accept the miner's spend of a coinbase aged {age} to {target:?}"
        )
    })?;
    anyhow::ensure!(!txids.is_empty(), "the coinbase spend to {target:?} returned no txid");
    let vrpc = validator.json_rpc().await?;
    let validator_mempool = vrpc.call_value("getrawmempool", json!([])).await?;
    for txid in &txids {
        let txid_hex = txid.to_string();
        anyhow::ensure!(
            validator_mempool
                .as_array()
                .is_some_and(|entries| entries.iter().any(|e| e.as_str() == Some(&txid_hex))),
            "the validator's mempool must hold the accepted spend {txid_hex}: {validator_mempool}"
        );
    }
    Ok(())
}
