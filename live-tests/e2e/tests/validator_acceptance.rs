//! Guarantees exported from zingolib's regtest suite under its regtest
//! removal (zingolib `docs/testing/regtest-port-plan.md`, ruling 4): each one
//! is about the validator or the indexer rather than the wallet, so it lives
//! here, on the harness that launches both.
//!
//! - the indexer's mempool view trails the validator's acceptance within a bound;
//! - the indexer converges with the validator after every generated block;
//! - the validator accepts a spend of a tip-block note, an aged note, a young
//!   coinbase and an aged coinbase, to Orchard and to Sapling receivers.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::json;
use ztest::prelude::*;

use e2e::Pool;

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// The spend the acceptance cells make back toward the faucet.
const SPEND_AMOUNT: u64 = 20_000;
/// Shielded funding pool for the faucet coinbase.
const FUND: Pool = Pool::Orchard;
/// zingolib's bound on the indexer's mempool ingestion after validator acceptance.
const MEMPOOL_INGEST_BOUND: Duration = Duration::from_secs(10);
/// The indexer mirrors the validator's mempool on a 500 ms cadence.
const MEMPOOL_POLL: Duration = Duration::from_millis(50);
const CONVERGENCE_ROUNDS: u32 = 5;
const AGED_NOTE_BLOCKS: u32 = 3;
const AGED_COINBASE_BLOCKS: u32 = 10;

/// The indexer's ingest path, as in wallet_to_validator.rs.
#[derive(Copy, Clone, Debug)]
enum Backend {
    Fetch,
    State,
}

/// H-LAG and H-FAITHFUL-ACCEPT: after the validator accepts a transaction, the
/// indexer's mempool shows it within the bound.
#[rstest]
#[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
#[case::state(Validator::zebrad("6.2.3"), Backend::State)]
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn indexer_mempool_view_trails_validator_acceptance<B: ValidatorConfig>(
    #[case] validator: Validator<B>,
    #[case] backend: Backend,
) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let base = validator.regtest().mine_to(FUND.ztest());
    let image = dev!(Indexer::Zainod, "../../Dockerfile");
    let (validator, indexer) = match backend {
        Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
        Backend::State => {
            let vol = env.shared_volume("zebra-db");
            (
                env.add_validator(base.mount(&vol)),
                env.add_indexer(
                    image.regtest().tuning(ZainoTuning::State).mount(&vol).named("zaino-state"),
                ),
            )
        }
    };
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let ua = recipient.address(Pool::Orchard.ztest()).await?;
    let txid = faucet
        .send(&ua, SEND_AMOUNT)
        .await?
        .into_iter()
        .next()
        .expect("send returns a txid")
        .to_string();
    let accepted_at = Instant::now();

    let vrpc = validator.json_rpc().await?;
    let validator_mempool = vrpc.call_value("getrawmempool", json!([])).await?;
    anyhow::ensure!(
        validator_mempool
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(&txid))),
        "the validator's own mempool must hold {txid} as soon as the send returns: \
         {validator_mempool}"
    );

    let irpc = indexer.json_rpc().await?;
    let lag = loop {
        let indexer_mempool = irpc.call_value("getrawmempool", json!([])).await?;
        if indexer_mempool
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| entry.as_str() == Some(&txid)))
        {
            break accepted_at.elapsed();
        }
        anyhow::ensure!(
            accepted_at.elapsed() < MEMPOOL_INGEST_BOUND,
            "the validator accepted {txid} {MEMPOOL_INGEST_BOUND:?} ago and the indexer's \
             mempool still does not show it"
        );
        tokio::time::sleep(MEMPOOL_POLL).await;
    };
    println!("indexer mempool ingestion lag after validator acceptance: {lag:?}");
    anyhow::ensure!(
        lag < MEMPOOL_INGEST_BOUND,
        "indexer mempool lag {lag:?} exceeded the {MEMPOOL_INGEST_BOUND:?} bound"
    );
    Ok(())
}

/// After each generated block the indexer serves the validator's tip.
#[rstest]
#[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
#[case::state(Validator::zebrad("6.2.3"), Backend::State)]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn indexer_converges_with_validator_after_block_generation<B: ValidatorConfig>(
    #[case] validator: Validator<B>,
    #[case] backend: Backend,
) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let base = validator.regtest();
    let image = dev!(Indexer::Zainod, "../../Dockerfile");
    let (validator, indexer) = match backend {
        Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
        Backend::State => {
            let vol = env.shared_volume("zebra-db");
            (
                env.add_validator(base.mount(&vol)),
                env.add_indexer(
                    image.regtest().tuning(ZainoTuning::State).mount(&vol).named("zaino-state"),
                ),
            )
        }
    };
    env.build().await?;

    let irpc = indexer.json_rpc().await?;
    for round in 1..=CONVERGENCE_ROUNDS {
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let indexer_height = irpc
            .call_value("getblockcount", json!([]))
            .await?
            .as_u64()
            .context("getblockcount returns a number")?;
        anyhow::ensure!(
            indexer_height >= u64::from(tip),
            "round {round}: indexer served tip {indexer_height} behind validator tip {tip}"
        );
    }
    Ok(())
}

/// The validator accepts a recipient's spend of a note received at the tip, and
/// of a note aged a few blocks, to each shielded receiver.
#[rstest]
#[case::tip_note_to_orchard(Pool::Orchard, 0)]
#[case::tip_note_to_sapling(Pool::Sapling, 0)]
#[case::aged_note_to_orchard(Pool::Orchard, AGED_NOTE_BLOCKS)]
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
    let ua = recipient.address(Pool::Orchard.ztest()).await?;
    faucet.send(&ua, SEND_AMOUNT).await?;
    let tip = validator.generate_blocks(1 + age).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    recipient.sync().await?;

    let target_addr = faucet.address(target.ztest()).await?;
    recipient.send(&target_addr, SPEND_AMOUNT).await.with_context(|| {
        format!("the validator must accept a spend of a note aged {age} to {target:?}")
    })?;
    Ok(())
}

/// The validator accepts the miner's spend of its coinbase at the tip, and of a
/// coinbase aged ten blocks, to each shielded receiver.
#[rstest]
#[case::young_coinbase_to_ironwood(Pool::Orchard, 0)]
#[case::young_coinbase_to_sapling(Pool::Sapling, 0)]
#[case::aged_coinbase_to_orchard(Pool::Orchard, AGED_COINBASE_BLOCKS)]
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
        let tip = validator.generate_blocks(age).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        faucet.sync().await?;
    }
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let target_addr = recipient.address(target.ztest()).await?;
    faucet.send(&target_addr, SPEND_AMOUNT).await.with_context(|| {
        format!(
            "the validator must accept the miner's spend of a coinbase aged {age} to {target:?}"
        )
    })?;
    Ok(())
}
