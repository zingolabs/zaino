//! A transaction mined only on an orphaned block: gone from every index, back where rebroadcast
//!
//! - Faucet spends a shielded note → a transparent recipient: nullifier, tx lookup and address rows
//!   move with the reorg, address rows checked against zebrad's own address RPCs at every step
//! - zebrad never re-admits an orphaned tx (rebroadcast = the wallet's job, done via zaino)
//! - Contract C15 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{ensure, Context, Result};
use rstest::rstest;
use serde_json::json;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
const DEPTH: u32 = 10;
const SEND: u64 = 250_000;
/// `PoolType` wire enum: transparent, sapling, orchard, ironwood
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];

#[rstest]
#[case::remined_at_the_same_height(0, true)]
#[case::remined_later(2, true)]
#[case::dropped(2, false)]
#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn orphaned_transaction_leaves_every_index_and_returns_where_remined(
    #[case] replacement: u32,
    #[case] rebroadcast: bool,
) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH),
    );
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
    let taddr = wallet.recipient(&validator, &indexer).await?.address(Pool::Transparent).await?;
    let txid =
        faucet.send(&taddr, SEND).await?.into_iter().next().context("send returns a txid")?;
    let raw = indexer.get_transaction(txid).await?.data;
    let raw_hex = zaino_testutils::hex::encode(&raw);
    let mined_at = u32::from(validator.generate_blocks(1).await?);
    indexer.wait_for_tip(validator.tip().await?, CONVERGE).await?;
    let vrpc = validator.json_rpc().await?;

    // zaino's rows for `taddr` == zebrad's → (balance, UTXO heights, (raw hex, height) per txid)
    let address_rows = async |tip: u32| -> Result<(i64, Vec<u64>, Vec<(String, u64)>)> {
        let who = json!([{ "addresses": [&taddr] }]);
        let balance = i64::from(indexer.get_taddress_balance(vec![taddr.clone()]).await?);
        let truth = vrpc.call_value("getaddressbalance", who.clone()).await?;
        assert_eq!(Some(balance), truth["balance"].as_i64(), "balance vs zebrad at tip {tip}");

        let mut utxos: Vec<(String, u32, i64, u64)> = indexer
            .get_address_utxos(vec![taddr.clone()], 0u32.into(), 0)
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
            .call_value("getaddressutxos", who)
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
        assert_eq!(utxos, utxo_truth, "UTXOs (txid, index, value, height) vs zebrad at tip {tip}");

        let mut txids: Vec<(String, u64)> = indexer
            .get_taddress_txids(taddr.clone(), 1u32.into(), tip.into())
            .await?
            .into_iter()
            .map(|tx| (zaino_testutils::hex::encode(&tx.data), tx.height))
            .collect();
        let range = json!([{ "addresses": [&taddr], "start": 1, "end": tip }]);
        let mut tx_truth = Vec::new();
        for txid in vrpc
            .call_value("getaddresstxids", range)
            .await?
            .as_array()
            .context("getaddresstxids returns an array")?
        {
            let tx = vrpc.call_value("getrawtransaction", json!([txid, 1])).await?;
            let hex = tx["hex"].as_str().unwrap_or_default().to_owned();
            tx_truth.push((hex, tx["height"].as_u64().unwrap_or(u64::MAX)));
        }
        txids.sort();
        tx_truth.sort();
        assert_eq!(txids, tx_truth, "txids (raw hex, height) in [1, {tip}] vs zebrad");
        Ok((balance, utxos.iter().map(|u| u.3).collect(), txids))
    };

    let wire_txid = txid.as_ref().to_vec();
    let a_block = indexer.get_block(mined_at.into()).await?;
    let a_tx = a_block.vtx.iter().find(|tx| tx.txid == wire_txid).context("tx in block A")?;
    let nullifiers: Vec<Vec<u8>> = a_tx
        .spends
        .iter()
        .map(|spend| spend.nf.clone())
        .chain(a_tx.actions.iter().map(|action| action.nullifier.clone()))
        .chain(a_tx.ironwood_actions.iter().map(|action| action.nullifier.clone()))
        .collect();
    assert!(!nullifiers.is_empty(), "faucet spent a shielded note (nullifier to track)");
    assert_eq!(indexer.get_transaction(txid).await?.height, u64::from(mined_at), "mined on A");
    let on_a = address_rows(mined_at).await?;
    let credit = i64::try_from(SEND)?;
    let mined = u64::from(mined_at);
    assert_eq!(on_a, (credit, vec![mined], vec![(raw_hex.clone(), mined)]), "recipient on A");

    let reorg = validator.reorg(1, replacement, Some(FILLER_ADDRESS)).await?;
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;
    let tip = u32::from(reorg.tip.0);

    let forgotten = vrpc.call_value("getrawtransaction", json!([txid.to_string(), 1])).await;
    assert!(forgotten.is_err(), "zebrad holds no orphaned tx (else the oracle moved)");
    let lookup = indexer.get_transaction(txid).await.map(|tx| tx.height).map_err(|e| e.grpc_code());
    assert!(lookup.is_err(), "GetTransaction on an orphaned-only tx: {lookup:?}");
    let pool = {
        let started = tokio::time::Instant::now();
        loop {
            let mut served: Vec<String> = indexer
                .get_mempool_tx(Vec::new())
                .await?
                .into_iter()
                .filter_map(|tx| <[u8; 32]>::try_from(tx.txid).ok())
                .map(|bytes| TxId::from_bytes(bytes).to_string())
                .collect();
            let truth = vrpc.call_value("getrawmempool", json!([])).await?;
            let mut truth: Vec<String> = truth
                .as_array()
                .context("getrawmempool returns an array")?
                .iter()
                .filter_map(|txid| txid.as_str().map(str::to_owned))
                .collect();
            served.sort();
            truth.sort();
            if served == truth {
                break served;
            }
            ensure!(
                started.elapsed() < CONVERGE,
                "mempool {served:?} never matched zebrad {truth:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    assert!(!pool.contains(&txid.to_string()), "orphaned tx not in the mirrored mempool");
    let orphaned = address_rows(tip).await?;
    assert_eq!(orphaned, (0, Vec::new(), Vec::new()), "no address row from the orphaned tx");
    let chain =
        indexer.get_block_range_with_pools(1u32.into(), reorg.tip.0, ALL_POOLS.to_vec()).await?;
    let spent_on = |chain: &[CompactBlock]| -> Vec<u64> {
        chain
            .iter()
            .filter(|block| {
                block.vtx.iter().any(|tx| {
                    tx.spends.iter().any(|s| nullifiers.contains(&s.nf))
                        || tx.actions.iter().any(|a| nullifiers.contains(&a.nullifier))
                        || tx.ironwood_actions.iter().any(|a| nullifiers.contains(&a.nullifier))
                })
            })
            .map(|block| block.height)
            .collect()
    };
    assert_eq!(spent_on(&chain), Vec::<u64>::new(), "faucet's note unspent again on B");

    let (landed, expected) = if rebroadcast {
        let sent = indexer.send_transaction(&raw).await?;
        assert_eq!(sent.error_code, 0, "rebroadcast via zaino: {}", sent.error_message);
        let started = tokio::time::Instant::now();
        while !vrpc
            .call_value("getrawmempool", json!([]))
            .await?
            .to_string()
            .contains(&txid.to_string())
        {
            ensure!(started.elapsed() < CONVERGE, "rebroadcast {txid} never reached zebrad");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let at = u32::from(validator.generate_blocks(1).to(FILLER_ADDRESS).await?);
        (vec![u64::from(at)], u64::from(tip) + 1)
    } else {
        validator.generate_blocks(1).to(FILLER_ADDRESS).await?;
        (Vec::new(), 0)
    };
    let tip = validator.tip().await?;
    indexer.wait_for_tip(tip, CONVERGE).await?;
    let chain = indexer.get_block_range_with_pools(1u32.into(), tip.0, ALL_POOLS.to_vec()).await?;
    assert_eq!(spent_on(&chain), landed, "nullifier on exactly the re-mining block");
    let credited = if rebroadcast { credit } else { 0 };
    let rows: Vec<(String, u64)> = landed.iter().map(|at| (raw_hex.clone(), *at)).collect();
    let after = address_rows(u32::from(tip.0)).await?;
    assert_eq!(after, (credited, landed.clone(), rows), "recipient at {landed:?} after re-mine");

    if rebroadcast {
        assert_eq!(landed, vec![expected], "re-mined on the first block after B");
        assert_eq!(indexer.get_transaction(txid).await?.height, expected, "GetTransaction height");
        let b_block = indexer.get_block(u32::try_from(expected)?.into()).await?;
        assert!(b_block.vtx.iter().any(|tx| tx.txid == wire_txid), "compact block carries it");
        if expected == u64::from(mined_at) {
            assert_ne!(b_block.hash, a_block.hash, "same height, different block");
        }
    } else {
        assert!(indexer.get_transaction(txid).await.is_err(), "dropped tx stays unknown");
    }
    Ok(())
}
