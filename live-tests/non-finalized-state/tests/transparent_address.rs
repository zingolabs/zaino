//! Transparent address index across a reorg: orphaned coinbase debited, winning coinbase credited
//!
//! - Branch A → `MINER_ADDRESS`, B → `FILLER_ADDRESS`: one address loses, the other gains
//! - Contract C6 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use ztest::prelude::*;
use ztest::regtest_conf::MINER_ADDRESS;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;
const ORPHANED: u32 = 4;
const REPLACEMENT: u32 = 6;

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn address_index_moves_orphaned_coinbase_to_the_winning_miner() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer =
        env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest().finalised_depth(DEPTH));
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    let old_tip = validator.tip().await?;
    indexer.wait_for_tip(old_tip, CONVERGE).await?;
    let addresses = [MINER_ADDRESS, FILLER_ADDRESS];
    let mut before = Vec::new();
    for address in addresses {
        let balance = indexer.get_taddress_balance(vec![address.to_owned()]).await?;
        let utxos = indexer.get_address_utxos(vec![address.to_owned()], 0u32.into(), 0).await?;
        before.push((i64::from(balance), utxos));
    }

    let reorg = validator.reorg(ORPHANED, REPLACEMENT, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    let (fork_parent, tip) = (u32::from(reorg.fork_parent.0), u32::from(reorg.tip.0));

    let vrpc = validator.json_rpc().await?;
    for (address, (balance_before, utxos_before)) in addresses.into_iter().zip(&before) {
        let who = json!([{ "addresses": [address] }]);
        let balance = i64::from(indexer.get_taddress_balance(vec![address.to_owned()]).await?);
        let truth = vrpc.call_value("getaddressbalance", who.clone()).await?;
        assert_eq!(Some(balance), truth["balance"].as_i64(), "{address} balance vs zebrad");

        let mut utxos: Vec<(String, u32, i64, u64)> = indexer
            .get_address_utxos(vec![address.to_owned()], 0u32.into(), 0)
            .await?
            .into_iter()
            .map(|u| {
                let txid = <[u8; 32]>::try_from(u.txid.as_slice()).map(TxId::from_bytes);
                (
                    txid.map(|t| t.to_string()).unwrap_or_default(),
                    u.index as u32,
                    u.value_zat,
                    u.height,
                )
            })
            .collect();
        let mut utxo_truth: Vec<(String, u32, i64, u64)> = vrpc
            .call_value("getaddressutxos", who.clone())
            .await?
            .as_array()
            .context("getaddressutxos returns an array")?
            .iter()
            .map(|u| {
                (
                    u["txid"].as_str().unwrap_or_default().to_owned(),
                    u["outputIndex"].as_u64().unwrap_or(u64::MAX) as u32,
                    u["satoshis"].as_i64().unwrap_or(-1),
                    u["height"].as_u64().unwrap_or(u64::MAX),
                )
            })
            .collect();
        utxos.sort();
        utxo_truth.sort();
        assert_eq!(utxos, utxo_truth, "{address} UTXOs (txid, index, value, height) vs zebrad");

        let range = json!([{ "addresses": [address], "start": 1, "end": tip }]);
        let txids = vrpc.call_value("getaddresstxids", range).await?;
        let mut tx_truth = Vec::new();
        for txid in txids.as_array().context("getaddresstxids returns an array")? {
            let tx = vrpc.call_value("getrawtransaction", json!([txid, 1])).await?;
            let hex = tx["hex"].as_str().unwrap_or_default().to_owned();
            tx_truth.push((hex, tx["height"].as_u64().unwrap_or(u64::MAX)));
        }
        let mut served: Vec<(String, u64)> = indexer
            .get_taddress_txids(address.to_owned(), 1u32.into(), tip.into())
            .await?
            .into_iter()
            .map(|tx| (zaino_testutils::hex::encode(&tx.data), tx.height))
            .collect();
        served.sort();
        tx_truth.sort();
        assert_eq!(served, tx_truth, "{address} txids (raw hex, height) in [1, {tip}] vs zebrad");

        let orphaned: i64 = utxos_before
            .iter()
            .filter(|u| u.height > u64::from(fork_parent))
            .map(|u| u.value_zat)
            .sum();
        let debited = balance_before - orphaned;
        let orphans_left = utxos.iter().filter(|u| u.3 > u64::from(fork_parent)).count();
        match address {
            MINER_ADDRESS => {
                assert!(orphaned > 0, "A's coinbase above fork {fork_parent} paid {address}");
                assert_eq!(balance, debited, "{address} debited exactly A's orphaned coinbase");
                assert_eq!(orphans_left, 0, "{address} keeps no UTXO above fork {fork_parent}");
            }
            _ => {
                assert_eq!(*balance_before, 0, "{address} unfunded before B");
                assert_eq!(
                    orphans_left, REPLACEMENT as usize,
                    "{address} one coinbase per B block"
                );
            }
        }
    }
    Ok(())
}
