//! Wallet spends at each note age, synced through zainod, accepted by zebrad
//!
//! - Spends anchor on zainod's served tree states (a wrong tree = a rejected spend)
//! - Aging mined to `FILLER_ADDRESS` (no spender gains a younger note)

use std::time::Duration;

use anyhow::{Context, Result};
use pretty_assertions::assert_eq;
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);
const SEND_AMOUNT: u64 = 250_000;
const SPEND_AMOUNT: u64 = 20_000;
/// ZIP-317 fee bound for one spend (premise: spendable covers amount + fee per receiver)
const SPEND_FEE_CEILING: u64 = 15_000;
/// Coinbase → unified address's Orchard receiver (credited to Ironwood: NU6.3 live from 2)
const FUND: Pool = Pool::Orchard;
const AGED_COINBASE_BLOCKS: u32 = 10;
const AGED_NOTE_BLOCKS: u32 = 3;
/// One spend per receiver per stage, each its own note (unconfirmed change never chains)
const RECEIVERS: [Pool; 2] = [Pool::Ironwood, Pool::Sapling];

/// Each stage spends to every receiver; zebrad's mempool holds every spend once `send` returns
///
/// - aged coinbase: the faucet's only notes, never spent before
/// - tip note: the recipient's receipts, one confirmation
/// - aged note: the recipient's change, `1 + AGED_NOTE_BLOCKS` confirmations
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn aged_coinbase_tip_and_aged_notes_spend_to_ironwood_and_sapling() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
    let recipient = wallet.recipient(&validator, &indexer).await?;
    let vrpc = validator.json_rpc().await?;
    // stage, blocks mined before it, spender, payee, amount per receiver
    #[rustfmt::skip]
    let stages = [
        ("aged coinbase", AGED_COINBASE_BLOCKS, &faucet,    &recipient, SEND_AMOUNT),
        ("tip note",      1,                    &recipient, &faucet,    SPEND_AMOUNT),
        ("aged note",     1 + AGED_NOTE_BLOCKS, &recipient, &faucet,    SPEND_AMOUNT),
    ];
    for (stage, aging, spender, payee, amount) in stages {
        let tip = validator.generate_blocks(aging).to(FILLER_ADDRESS).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        spender.sync().await?;
        let spendable = spender.balances().await?;
        let needed = RECEIVERS.len() as u64 * (amount + SPEND_FEE_CEILING);
        assert!(
            spendable.total() >= needed,
            "{stage} premise at {tip:?}: spendable {spendable:?} under {needed}"
        );

        for receiver in RECEIVERS {
            let to = payee.address(receiver).await?;
            let sent = spender
                .send(&to, amount)
                .await
                .with_context(|| format!("{stage} at {tip:?} → {receiver:?}: send {amount}"))?;
            let reply = vrpc.call_value("getrawmempool", json!([])).await?;
            let mempool: Vec<&str> = reply
                .as_array()
                .with_context(|| format!("getrawmempool not an array: {reply}"))?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let missing: Vec<String> = sent
                .iter()
                .map(TxId::to_string)
                .filter(|txid| !mempool.contains(&txid.as_str()))
                .collect();
            assert!(!sent.is_empty(), "{stage} at {tip:?} → {receiver:?}: send returned no txid");
            assert_eq!(
                missing,
                Vec::<String>::new(),
                "{stage} at {tip:?} → {receiver:?}: spends absent from zebrad's mempool {mempool:?}"
            );
        }
    }
    Ok(())
}
