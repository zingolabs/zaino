//! In-process librustzcash wallet (faucet + recipient) → zainod pod over gRPC
//!
//! - `send_to_all`, shielding, durable-seam address txids
//! - `get_transaction` (mempool → mined), `get_mempool_tx`, `get_mempool_stream`, unmined sends
//! - Address queries: `get_taddress_*` / `get_address_utxos*` (recipient + faucet coinbase)
//! - Tree state: `get_tree_state`, `get_subtree_roots`; block range pool filters
//! - Validator JSON-RPC = oracle (zainod serves gRPC only)

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee, one-note shield round
const SHIELD_FEE: u64 = 15_000;
/// Coinbase → unified address's Orchard receiver (credited to Ironwood: NU6.3 live from 2)
const FUND: Pool = Pool::Orchard;
/// Buries a transaction's block below the finalized tip (regtest depth + margin)
const SEAM_ADVANCE: u32 = ztest::backends::zainod::REGTEST_FINALISED_DEPTH + 5;
/// Finalized writer's commit lag behind the validator (per batch: frontier moves in steps)
const SEAM_TIMEOUT: Duration = Duration::from_secs(300);

mod wallet {
    use super::*;

    /// Faucet's coinbase notes → Ironwood; one faucet → all three pools, each credited once
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_all() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // One note per send (no chaining of unconfirmed change)
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 3).await?;
        let funded = faucet.balances().await?;
        assert!(funded.ironwood > 0, "NU6.3 regtest: orchard coinbase → ironwood: {funded:?}");
        let recipient = wallet.recipient(&validator, &indexer).await?;
        for pool in [Pool::Ironwood, Pool::Sapling, Pool::Transparent] {
            let addr = recipient.address(pool).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
        }
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;

        let balances = recipient.balances().await?;
        // Nonzero orchard = receipt mislabelled (NU6.3: unified output → Ironwood)
        let (ironwood, orchard) = (balances.ironwood, balances.orchard);
        assert_eq!((ironwood, orchard), (SEND_AMOUNT, 0), "unified receipt → ironwood");
        assert_eq!((balances.sapling, balances.transparent), (SEND_AMOUNT, SEND_AMOUNT));
        Ok(())
    }

    /// Transparent receipt shielded → `SEND_AMOUNT - SHIELD_FEE` in the latest shielded pool
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield_for_validator() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let credited = Pool::Ironwood;
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(recipient.balances().await?.transparent, SEND_AMOUNT);

        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        let shielded = recipient.balances().await?.get(credited);
        assert_eq!(shielded, SEND_AMOUNT - SHIELD_FEE, "net of ZIP-317 fee, in {credited:?}");
        Ok(())
    }

    /// Transparent send → same address txids from the non-finalized state and again once a
    /// seam-deep advance commits it to the durable compact-block index
    ///
    /// - Shipped depth (1000): `SEAM_ADVANCE` buries nothing → vacuous compare (durable poll =
    ///   the loud check against that)
    /// - Advance mined to `FILLER_ADDRESS` (to `FUND` = `SEAM_ADVANCE` halo2 proofs, past the cap)
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent_finalization() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let height = indexer.latest_block_height().await?;
        let non_finalized_txs = indexer.get_taddress_txids(taddr.clone(), height, height).await?;

        // Load-bearing: buries the send below the finalized tip (`tip - depth`)
        let tip = validator.generate_blocks(SEAM_ADVANCE).to(FILLER_ADDRESS).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // - Compact-block `finalized_height` = only proof of the durable commit
        // - Unpublished until the first commit → keep polling
        let deadline = tokio::time::Instant::now() + SEAM_TIMEOUT;
        loop {
            let durable = indexer.finalized_height(ZainoIndex::CompactBlock).await?;
            if durable.is_some_and(|durable| durable >= u32::from(height)) {
                break;
            }
            let expired = tokio::time::Instant::now() >= deadline;
            anyhow::ensure!(!expired, "durable {durable:?}, never {height:?} in {SEAM_TIMEOUT:?}");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let durable_txs = indexer.get_taddress_txids(taddr, height, height).await?;

        recipient.sync().await?;
        let transparent = recipient.balances().await?.transparent;
        assert_eq!(transparent, SEND_AMOUNT, "transparent send still served once durable");
        assert_eq!(non_finalized_txs, durable_txs, "address txs identical across the seam");
        Ok(())
    }

    /// Both unmined txs listed; an exclude-by-txid-suffix filter drops one
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_tx() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?.into_iter().next().expect("txid");
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?.into_iter().next().expect("txid");

        let want = [t_txid.to_string(), u_txid.to_string()];
        let vrpc = validator.json_rpc().await?;
        // - Polled mirror (500 ms): validator + zaino agree only eventually
        // - `want` = non-vacuity probe (a never-relayed send leaves both sides empty and equal)
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = indexer
                    .get_mempool_tx(Vec::new())
                    .await?
                    .into_iter()
                    .map(|tx| {
                        <[u8; 32]>::try_from(tx.txid)
                            .map(|bytes| TxId::from_bytes(bytes).to_string())
                            .map_err(|bytes| anyhow::anyhow!("txid of {} bytes", bytes.len()))
                    })
                    .collect::<Result<Vec<String>>>()?;
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                let expired = tokio::time::Instant::now() >= deadline;
                anyhow::ensure!(!expired, "{indexer_txids:?} ≠ {validator_txids:?} ⊇ {want:?}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        assert_eq!(mempool.len(), 2, "exactly the two broadcast txs: {mempool:?}");

        let mut want = [t_txid.as_ref().to_vec(), u_txid.as_ref().to_vec()];
        want.sort();

        let mut all = indexer.get_mempool_tx(Vec::new()).await?;
        all.sort_by_key(|tx| tx.txid.clone());
        let txids: Vec<_> = all.iter().map(|tx| tx.txid.clone()).collect();
        assert_eq!(txids, want, "both unmined txs");

        let remaining = indexer.get_mempool_tx(vec![want[0][8..].to_vec()]).await?;
        assert_eq!(remaining.len(), 1, "excluding one leaves the other");
        assert_eq!(remaining[0].txid, want[1]);
        Ok(())
    }

    /// Streamed bytes = validator's unmined txs verbatim (`getrawtransaction` oracle)
    ///
    /// - Stream ends when its admitted tip moves → spawn the drain, then mine
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");
        let u_txid =
            faucet.send(&ua, SEND_AMOUNT).await?.into_iter().next().expect("send returns a txid");

        let vrpc = validator.json_rpc().await?;
        let want = [t_txid.to_string(), u_txid.to_string()];
        // - Polled mirror (500 ms): validator + zaino agree only eventually
        // - `want` = non-vacuity probe (a never-relayed send leaves both sides empty and equal)
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = indexer
                    .get_mempool_tx(Vec::new())
                    .await?
                    .into_iter()
                    .map(|tx| {
                        <[u8; 32]>::try_from(tx.txid)
                            .map(|bytes| TxId::from_bytes(bytes).to_string())
                            .map_err(|bytes| anyhow::anyhow!("txid of {} bytes", bytes.len()))
                    })
                    .collect::<Result<Vec<String>>>()?;
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                let expired = tokio::time::Instant::now() >= deadline;
                anyhow::ensure!(!expired, "{indexer_txids:?} ≠ {validator_txids:?} ⊇ {want:?}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        let mut expected = Vec::with_capacity(mempool.len());
        for txid in &mempool {
            expected.push(
                vrpc.call_value("getrawtransaction", json!([txid]))
                    .await?
                    .as_str()
                    .with_context(|| format!("getrawtransaction {txid} returns hex"))?
                    .to_string(),
            );
        }
        expected.sort();

        let drain = tokio::spawn({
            let indexer = indexer.clone();
            async move { indexer.get_mempool_stream().await }
        });
        // Grace period, not a handshake (no stream-open signal; mining first = drain waits a tip
        // past an empty mempool)
        tokio::time::sleep(Duration::from_secs(5)).await;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let txs = drain.await.expect("mempool-stream drain task joins")?;
        let mut streamed: Vec<String> = txs.iter().map(|tx| hex::encode(&tx.data)).collect();
        streamed.sort();
        assert_eq!(streamed, expected, "validator's unmined txs, verbatim");
        assert!(txs.iter().all(|tx| tx.height == 0), "unmined = height-0 sentinel");
        Ok(())
    }

    /// Two unmined sends mirrored, then mined → confirmed balances
    ///
    /// - Unconfirmed split unasserted (ztest's librustzcash wallet: no pending-balance accessor)
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn monitor_unverified_mempool() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // One shielded note per unmined send
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let ua = recipient.address(Pool::Ironwood).await?;
        let zaddr = recipient.address(Pool::Sapling).await?;
        let ua_txid =
            faucet.send(&ua, SEND_AMOUNT).await?.into_iter().next().expect("send returns a txid");
        let sapling_txid = faucet
            .send(&zaddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");

        let want = [ua_txid.to_string(), sapling_txid.to_string()];
        let vrpc = validator.json_rpc().await?;
        // - Polled mirror (500 ms): validator + zaino agree only eventually
        // - `want` = non-vacuity probe (a never-relayed send leaves both sides empty and equal)
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = indexer
                    .get_mempool_tx(Vec::new())
                    .await?
                    .into_iter()
                    .map(|tx| {
                        <[u8; 32]>::try_from(tx.txid)
                            .map(|bytes| TxId::from_bytes(bytes).to_string())
                            .map_err(|bytes| anyhow::anyhow!("txid of {} bytes", bytes.len()))
                    })
                    .collect::<Result<Vec<String>>>()?;
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                let expired = tokio::time::Instant::now() >= deadline;
                anyhow::ensure!(!expired, "{indexer_txids:?} ≠ {validator_txids:?} ⊇ {want:?}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        assert_eq!(mempool.len(), 2, "the two broadcast txs, nothing else: {mempool:?}");

        // `PoolBalances` = confirmed-only (mirrored ≠ credited)
        recipient.sync().await?;
        let unconfirmed = recipient.balances().await?;
        assert_eq!((unconfirmed.ironwood, unconfirmed.sapling), (0, 0), "unmined ≠ confirmed");

        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        let balances = recipient.balances().await?;
        assert_eq!((balances.ironwood, balances.sapling), (SEND_AMOUNT, SEND_AMOUNT));
        Ok(())
    }

    /// Transparent receipt through every address query
    ///
    /// - Txids over a span around the send = exactly the send, validator's bytes
    /// - One utxo (the send's txid, address, value, height); streamed = unary; balance = the send
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_receipt_served_by_address_queries() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        let txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let start = BlockHeight::from(u32::from(tip).saturating_sub(2));
        let txs = indexer.get_taddress_txids(taddr.clone(), start, tip).await?;
        assert_eq!(txs.len(), 1, "the span holds exactly the one send");
        let vrpc = validator.json_rpc().await?;
        let raw = vrpc.call_value("getrawtransaction", json!([txid.to_string(), 0])).await?;
        let raw = raw.as_str().context("getrawtransaction returns a hex string")?;
        assert_eq!(hex::encode(&txs[0].data), raw, "validator's bytes");

        let utxos =
            indexer.get_address_utxos(vec![taddr.clone()], BlockHeight::from(0u32), 0).await?;
        assert_eq!(utxos.len(), 1, "the send leaves exactly one utxo");
        assert_eq!(utxos[0].txid, txid.as_ref().to_vec(), "utxo = the send");
        assert_eq!(utxos[0].address, taddr);
        assert_eq!(utxos[0].value_zat, SEND_AMOUNT as i64);
        assert_eq!(utxos[0].height, u64::from(u32::from(tip)));
        let streamed = indexer
            .get_address_utxos_stream(vec![taddr.clone()], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(streamed, utxos, "stream = unary");

        let balance = indexer.get_taddress_balance(vec![taddr]).await?;
        assert_eq!(i64::from(balance), SEND_AMOUNT as i64, "balance = the one utxo");
        Ok(())
    }

    /// - Unmined → height sentinel `0`, never the tip (else unconfirmed reads as confirmed)
    /// - Served bytes = validator's `getrawtransaction` (independent oracle); mined → its height
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mempool() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard).await?;
        let txid = faucet.send(&addr, SEND_AMOUNT).await?.into_iter().next().expect("txid");
        tokio::time::sleep(Duration::from_secs(1)).await;

        let unmined = indexer.get_transaction(txid).await?;
        assert_eq!(unmined.height, 0, "unmined = mempool height sentinel, not the tip");

        let vrpc = validator.json_rpc().await?;
        let raw_hex = vrpc.call_value("getrawtransaction", json!([txid.to_string()])).await?;
        let raw_hex = raw_hex
            .as_str()
            .context("getrawtransaction must return a hex string at verbosity 0")?;
        let served = hex::encode(&unmined.data);
        assert_eq!(served, raw_hex, "{txid}: validator's bytes");

        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let mined = indexer.get_transaction(txid).await?;
        assert_eq!(mined.height, u64::from(u32::from(tip)), "mined = confirming height");
        assert_eq!(mined.data, unmined.data, "mining leaves the served bytes unchanged");
        Ok(())
    }

    /// Orchard send mined → tip tree state = validator's best block, both trees served
    ///
    /// - Subtree = 2^16 notes, never reached on regtest (a synthesised root fails this)
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn tree_state_and_subtree_roots() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let tree = indexer.get_tree_state(tip).await?;
        assert_eq!(tree.height, u64::from(u32::from(tip)));
        let best = validator.json_rpc().await?.call_value("getbestblockhash", json!([])).await?;
        let best = best.as_str().context("getbestblockhash returns a hex string")?;
        assert_eq!(tree.hash.as_str(), best);
        assert!(!tree.sapling_tree.is_empty(), "sapling tree must be served");
        assert!(!tree.orchard_tree.is_empty(), "orchard tree must be served");

        let roots = indexer.get_subtree_roots(0, ShieldedProtocol::Orchard, 0).await?;
        assert!(roots.is_empty(), "regtest completes no subtree: {roots:?}");
        Ok(())
    }
}

/// Block-range pool filters and faucet-coinbase taddr gRPC queries
mod zebrad {
    use super::*;

    /// - `get_block_range` with no pools == explicit shielded pools
    /// - Tip block = shielded coinbase + the send, no transparent data
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_default_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        faucet.send(&ua, SEND_AMOUNT).await?;
        let end = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(end, READY).await?;

        // `PoolType` codes = `Pools::default()` (else default-vs-explicit checks compare 2 filters)
        let shielded_pools = vec![2, 3, 4];
        let start = BlockHeight::from(1u32);

        let default = indexer.get_block_range(start, end).await?;
        let shielded = indexer.get_block_range_with_pools(start, end, shielded_pools).await?;
        assert_eq!(default, shielded);

        let compact_block = default.last().expect("non-empty range");
        assert_eq!(BlockHeight::from(compact_block.height as u32), end);
        assert_eq!(compact_block.vtx.len(), 2);
        assert_eq!(compact_block.vtx.last().expect("send tx").index, 1);
        for tx in &compact_block.vtx {
            let transparent = (tx.vin.len(), tx.vout.len());
            assert_eq!(transparent, (0, 0), "no transparent data when no pool types requested");
        }
        Ok(())
    }

    /// All pools requested → tip block carries the coinbase + all three sends with their
    /// pool data
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_all_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // One note per send, all three sends mined into one block
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 3).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let mut txids = Vec::new();
        for pool in [Pool::Transparent, Pool::Sapling, Pool::Ironwood] {
            let addr = recipient.address(pool).await?;
            let txid = faucet.send(&addr, SEND_AMOUNT).await?.into_iter().next().expect("txid");
            txids.push(txid);
        }
        let end = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(end, READY).await?;

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let start = BlockHeight::from(1u32);

        let range = indexer.get_block_range_with_pools(start, end, all_pools).await?;

        let compact_block = range.last().expect("non-empty range");
        assert_eq!(BlockHeight::from(compact_block.height as u32), end);
        assert_eq!(compact_block.vtx.len(), 4);

        let mut sends = Vec::new();
        for txid in &txids {
            let tx = compact_block
                .vtx
                .iter()
                .find(|tx| tx.txid == txid.as_ref().to_vec())
                .with_context(|| format!("tip block carries send {txid}"))?;
            sends.push(tx);
        }
        let (transparent, sapling, ironwood) = (sends[0], sends[1], sends[2]);
        assert!(!transparent.vout.is_empty(), "transparent send: vout served");
        assert!(!sapling.outputs.is_empty(), "sapling send: outputs served");
        let unified = (ironwood.ironwood_actions.len(), ironwood.actions.len());
        // NU6.3: unified-address send = Ironwood actions only
        assert!(unified.0 > 0 && unified.1 == 0, "unified send: ironwood only: {unified:?}");
        Ok(())
    }

    /// Faucet coinbase taddr (miner paid every block)
    ///
    /// - Balance = Σ validator `getaddressutxos` (zero / truncated fails)
    /// - `max_entries = 3` binds → exactly three utxos; streamed = unary over the same arguments
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn faucet_coinbase_taddr_balance_and_utxos() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &indexer).await?;
        let faucet_taddr = faucet.address(Pool::Transparent).await?;
        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let utxos = validator
            .json_rpc()
            .await?
            .call_value("getaddressutxos", serde_json::json!([{ "addresses": [&faucet_taddr] }]))
            .await?;
        let utxo_total: i64 = utxos
            .as_array()
            .context("getaddressutxos must return an array")?
            .iter()
            .filter_map(|u| u.get("satoshis").and_then(serde_json::Value::as_i64))
            .sum();
        assert!(utxo_total > 0, "faucet taddr must hold coinbase value");
        let balance = indexer.get_taddress_balance(vec![faucet_taddr.clone()]).await?;
        assert_eq!(i64::from(balance), utxo_total, "balance = Σ validator's utxos");

        let start = BlockHeight::from(2u32);
        let streamed =
            indexer.get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3).await?;
        assert_eq!(streamed.len(), 3, "`max_entries` binds (every block pays the miner)");
        let unary = indexer.get_address_utxos(vec![faucet_taddr], start, 3).await?;
        assert_eq!(streamed, unary, "stream = unary");
        Ok(())
    }
}
