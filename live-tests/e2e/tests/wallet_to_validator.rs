//! In-process librustzcash wallet (faucet + recipient) → zainod pod over gRPC.
//!
//! - Sends to each pool, `send_to_all`, shielding, mining-reward receipt, `get_info` smoke
//! - `get_transaction` (mined / mempool), finalisation-seam address txids
//! - Mempool: `get_mempool_tx`, `get_mempool_stream`, unconfirmed-balance monitoring
//! - Address queries: `get_address_utxos`, `get_taddress_*` (recipient + faucet coinbase)
//! - Tree state: `get_tree_state`, `get_subtree_roots`
//! - Block range: default / all pools, out-of-range bounds, compact-block transparent data
//! - Validator JSON-RPC = oracle (zainod serves gRPC only)

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use zaino_testutils::wait_for_finalised;
use ztest::prelude::*;

use e2e::{assert_pool_absent, assert_pool_present, Pool};

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;
/// Shielded funding pool for the faucet coinbase: the miner coinbase pays the
/// Orchard receiver of a unified address. Under this file's NU6.3-active
/// regtest schedule (Ironwood live from height 2), that note is credited to the
/// Ironwood pool — hence `receives_mining_reward` asserts an Ironwood balance.
const FUND: Pool = Pool::Orchard;
/// Buries a transaction's block below the final boundary (regtest depth + margin)
const SEAM_ADVANCE: u32 = ztest::backends::zainod::REGTEST_FINALISED_DEPTH + 5;
/// Longest the finalised writer should need to commit up to a height the
/// validator already serves. It commits per batch, so the frontier moves in
/// steps rather than per block.
const SEAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Wallet-driven flows.
mod wallet {
    use super::*;

    /// The faucet's synced wallet holds a spendable shielded coinbase note.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn receives_mining_reward() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        // NU6.3 regtest: orchard coinbase -> Ironwood
        let credited = Pool::Ironwood;
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let balances = faucet.balances().await?;
        assert!(balances.get(credited.ztest()) > 0, "spendable {credited:?} note: {balances:?}");
        Ok(())
    }

    /// Smoke: faucet and recipient wallets connect and sync without error,
    /// and the indexer reports node info.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_to_node_get_info() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let _faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        recipient.sync().await?;
        assert!(!indexer.indexer_info().await?.chain_name.is_empty());
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's unified address, whose Orchard
    /// receiver credits Ironwood from NU6.3 (Orchard is spend-locked) and Orchard
    /// before it.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_unified() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let credited = Pool::Ironwood;
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(credited.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        let received = recipient.balances().await?.get(credited.ztest());
        assert_eq!(received, SEND_AMOUNT, "recipient {credited:?} balance");
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's sapling address; the
    /// recipient's synced wallet shows it.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_sapling() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Sapling.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(recipient.balances().await?.sapling, SEND_AMOUNT);
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's transparent address; the
    /// recipient's synced wallet shows it.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(recipient.balances().await?.transparent, SEND_AMOUNT);
        Ok(())
    }

    /// One faucet funds a send to all three pools; each recipient pool
    /// reports 250_000.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_all() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // Three notes — one per send (no chaining of unconfirmed change).
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 3).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        // NU6.3: the unified-address (Orchard-receiver) output routes to Ironwood.
        for pool in [Pool::Ironwood, Pool::Sapling, Pool::Transparent] {
            let addr = recipient.address(pool.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
        }
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;

        let balances = recipient.balances().await?;
        // From NU6.3 the unified-address output routes to Ironwood; the
        // orchard pool must stay empty (a nonzero orchard here means the
        // receipt was mislabelled, not merely misrouted).
        let (ironwood, orchard) = (balances.ironwood, balances.orchard);
        assert_eq!((ironwood, orchard), (SEND_AMOUNT, 0), "unified receipt → ironwood");
        assert_eq!((balances.sapling, balances.transparent), (SEND_AMOUNT, SEND_AMOUNT));
        Ok(())
    }

    /// The recipient receives a transparent 250_000, shields it, and reports
    /// 250_000 − 15_000 fee in the pool the chain's latest activation shields into.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield_for_validator() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let credited = Pool::Ironwood;
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(recipient.balances().await?.transparent, SEND_AMOUNT);

        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        let shielded = recipient.balances().await?.get(credited.ztest());
        assert_eq!(shielded, SEND_AMOUNT - SHIELD_FEE, "net of ZIP-317 fee, in {credited:?}");
        Ok(())
    }

    /// A transparent send returns the same address txids from the
    /// non-finalized state and again after a seam-deep advance lands it in
    /// the finalised DB.
    ///
    /// - at the shipped depth (1000) `SEAM_ADVANCE` buries nothing → both reads come from
    ///   the non-finalized state, compare passes vacuously
    /// - `wait_for_finalised` = the loud check against exactly that
    /// - Advance mined to `FILLER_ADDRESS`: on this file's `FUND` coinbase it is
    ///   `SEAM_ADVANCE` halo2 proofs, past the tier's cap on the two cores it reserves
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent_finalization() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // The send's block, queried while it is still in the non-finalized state.
        let height = indexer.latest_block_height().await?;
        let unfinalised_txs = indexer.get_taddress_txids(taddr.clone(), height, height).await?;

        // The load-bearing advance: push the send below the seam so it crosses
        // the finalised floor (`tip - seam`) into the finalized DB.
        let tip = validator.generate_blocks(SEAM_ADVANCE).to(FILLER_ADDRESS).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // Without this the test is vacuous: it would compare two reads that
        // both came from the non-finalized state. `index_frontier` is the
        // only observable that says the finalised writer committed the
        // send's block — a served height proves nothing, because below the
        // seam zaino can answer straight from the validator it proxies.
        wait_for_finalised(&indexer, u32::from(height), SEAM_TIMEOUT).await?;

        let finalised_txs = indexer.get_taddress_txids(taddr, height, height).await?;

        recipient.sync().await?;
        let transparent = recipient.balances().await?.transparent;
        assert_eq!(transparent, SEND_AMOUNT, "transparent send still served once final");
        assert_eq!(unfinalised_txs, finalised_txs, "address txs identical across the seam");
        Ok(())
    }

    /// Smoke: the indexer serves `get_transaction` for the mined orchard
    /// send by txid.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mined() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        let txid =
            faucet.send(&addr, SEND_AMOUNT).await?.into_iter().next().expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let served = indexer.get_transaction(txid).await?;
        // A mined transaction reports its block height, not the mempool sentinel 0.
        assert_eq!(served.height, u64::from(u32::from(tip)));
        let raw = validator
            .json_rpc()
            .await?
            .call_value("getrawtransaction", serde_json::json!([txid.to_string(), 0]))
            .await?;
        let raw = raw.as_str().context("getrawtransaction returns a hex string")?;
        assert_eq!(zaino_testutils::hex::encode(&served.data), raw, "validator's bytes");
        Ok(())
    }

    /// `get_mempool_tx` returns the two unmined transactions, and the
    /// exclude-by-txid-suffix filter drops one.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_tx() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let ua = recipient.address(Pool::Orchard.ztest()).await?;
        let t_txid = faucet.send(&taddr, SEND_AMOUNT).await?.into_iter().next().expect("txid");
        let u_txid = faucet.send(&ua, SEND_AMOUNT).await?.into_iter().next().expect("txid");

        // The validator fixes what the mempool holds before GetMempoolTx is
        // asked; without it a count assertion only says zaino agrees with itself.
        let want = [t_txid.to_string(), u_txid.to_string()];
        let vrpc = validator.json_rpc().await?;
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
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

        // Excluding the first by its txid suffix leaves only the second.
        let remaining = indexer.get_mempool_tx(vec![want[0][8..].to_vec()]).await?;
        assert_eq!(remaining.len(), 1, "excluding one leaves the other");
        assert_eq!(remaining[0].txid, want[1]);
        Ok(())
    }

    /// The stream serves exactly the transaction bytes the validator holds
    /// unmined.
    ///
    /// GetMempoolStream is bound to the chain tip the request was admitted
    /// against and ends when that tip moves, so the drain can only complete
    /// once a block is mined — hence the spawn-then-mine shape. The
    /// per-transaction bytes come from the validator's `getrawtransaction`,
    /// so this catches a mirror that is missing, over-full, or serving the
    /// wrong bytes; `!is_empty()` alone caught none of those.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let ua = recipient.address(Pool::Orchard.ztest()).await?;
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
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
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
        // The validator's own bytes for each mirrored txid: the independent oracle for
        // what zaino streams.
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
        // Mining before the subscription is admitted leaves the drain waiting
        // on the *next* tip change, by which point the mempool is empty.
        // Nothing observable reports that the stream is open, so this is a
        // grace period rather than a handshake. The missing seam is a
        // stream-established signal, e.g.
        //   let (ready, drain) = indexer.get_mempool_stream_handle().await?;
        //   ready.await?; // resolves once zaino has admitted the subscription
        tokio::time::sleep(Duration::from_secs(5)).await;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let txs = drain.await.expect("mempool-stream drain task joins")?;
        let mut streamed: Vec<String> =
            txs.iter().map(|tx| zaino_testutils::hex::encode(&tx.data)).collect();
        streamed.sort();
        assert_eq!(streamed, expected, "validator's unmined txs, verbatim");
        assert!(txs.iter().all(|tx| tx.height == 0), "unmined = height-0 sentinel");
        Ok(())
    }

    /// Broadcast two unmined sends, observe them in zaino's mirror of the
    /// validator's mempool, then mine them in and confirm the balances. The
    /// *unconfirmed* (mempool) pool-balance split under test cannot be
    /// asserted — ztest's librustzcash wallet exposes no pending/unconfirmed
    /// pool-balance accessor — so the confirmed balances stand in.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn monitor_unverified_mempool() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // Two shielded notes — one per unmined send.
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 2).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let ua = recipient.address(Pool::Ironwood.ztest()).await?;
        let zaddr = recipient.address(Pool::Sapling.ztest()).await?;
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
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
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

        // `PoolBalances` is confirmed-only, so an unmined send must credit
        // nothing however visible it is in the mempool mirror above.
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

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let utxos =
            indexer.get_address_utxos(vec![taddr.clone()], BlockHeight::from(0u32), 0).await?;
        assert_eq!(utxos[0].txid, txid.as_ref().to_vec(), "utxo[0] txid must be the send");
        Ok(())
    }

    /// Tip tree state → validator's best block hash + non-empty tree per shielded protocol
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let tip = indexer.latest_block_height().await?;
        let tree = indexer.get_tree_state(tip).await?;
        assert_eq!(tree.height, u64::from(u32::from(tip)));
        let best = validator.json_rpc().await?.call_value("getbestblockhash", json!([])).await?;
        let best = best.as_str().context("getbestblockhash returns a hex string")?;
        assert_eq!(tree.hash.as_str(), best);
        assert!(!tree.sapling_tree.is_empty(), "sapling tree must be served");
        assert!(!tree.orchard_tree.is_empty(), "orchard tree must be served");
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // A subtree completes every 2^16 notes, which no regtest chain reaches;
        // a synthesised root would fail this.
        let roots = indexer.get_subtree_roots(0, ShieldedProtocol::Orchard, 0).await?;
        assert!(roots.is_empty(), "regtest completes no subtree: {roots:?}");
        Ok(())
    }

    /// Smoke: `get_taddress_txids` over the recipient's taddr and a range
    /// around the send succeeds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let tip = indexer.latest_block_height().await?;
        let start = BlockHeight::from(u32::from(tip).saturating_sub(2));
        let txs = indexer.get_taddress_txids(taddr, start, tip).await?;
        assert_eq!(txs.len(), 1, "the span holds exactly the one send");
        let vrpc = validator.json_rpc().await?;
        let raw = vrpc.call_value("getrawtransaction", serde_json::json!([txid.to_string(), 0]));
        let raw = raw.await?;
        let raw = raw.as_str().context("getrawtransaction returns a hex string")?;
        assert_eq!(zaino_testutils::hex::encode(&txs[0].data), raw);
        Ok(())
    }

    /// Smoke: `get_address_utxos` over the recipient's taddr succeeds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let utxos =
            indexer.get_address_utxos(vec![taddr.clone()], BlockHeight::from(0u32), 0).await?;
        assert_eq!(utxos.len(), 1, "the send leaves exactly one utxo");
        assert_eq!(utxos[0].address, taddr);
        assert_eq!(utxos[0].value_zat, SEND_AMOUNT as i64);
        assert_eq!(utxos[0].height, u64::from(u32::from(tip)));
        Ok(())
    }

    /// Smoke: `get_address_utxos_stream` over the recipient's taddr
    /// succeeds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos_stream() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let streamed = indexer
            .get_address_utxos_stream(vec![taddr.clone()], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(streamed.len(), 1, "the send leaves exactly one utxo");
        assert_eq!(streamed[0].address, taddr);
        assert_eq!(streamed[0].value_zat, SEND_AMOUNT as i64);
        let unary = indexer.get_address_utxos(vec![taddr], BlockHeight::from(0u32), 0).await?;
        assert_eq!(streamed, unary, "stream = unary");
        Ok(())
    }

    /// `get_transaction` over an unmined orchard send, then over the same
    /// transaction once mined — the mempool-to-mined transition, end to end.
    ///
    /// The invariants, in order:
    /// - an unmined transaction carries the mempool height sentinel (`0`),
    ///   not the current tip height. This is the whole point of the test:
    ///   returning the tip would make an unconfirmed transaction look
    ///   confirmed to every wallet on the other end of the wire.
    /// - the bytes zaino serves are the transaction that was actually
    ///   broadcast. The validator's `getrawtransaction` is the oracle —
    ///   comparing against a hash zaino also computed would only prove
    ///   zaino agrees with itself.
    /// - once mined, the same query reports the real confirmation height,
    ///   not the sentinel.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mempool() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        let txid = faucet.send(&addr, SEND_AMOUNT).await?.into_iter().next().expect("txid");
        tokio::time::sleep(Duration::from_secs(1)).await;

        let unmined = indexer.get_transaction(txid).await?;
        assert_eq!(unmined.height, 0, "unmined = mempool height sentinel, not the tip");

        // The validator is the independent oracle for what was broadcast.
        let vrpc = validator.json_rpc().await?;
        let raw_hex = vrpc.call_value("getrawtransaction", json!([txid.to_string()])).await?;
        let raw_hex = raw_hex
            .as_str()
            .context("getrawtransaction must return a hex string at verbosity 0")?;
        let served = zaino_testutils::hex::encode(&unmined.data);
        assert_eq!(served, raw_hex, "{txid}: validator's bytes");

        // Mine it: the same query must flip off the sentinel onto the real
        // confirmation height.
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let mined = indexer.get_transaction(txid).await?;
        assert_eq!(mined.height, u64::from(u32::from(tip)), "mined = confirming height");
        assert_eq!(mined.data, unmined.data, "mining leaves the served bytes unchanged");
        Ok(())
    }

    /// `GetTaddressBalance` over the recipient's taddr reports 250_000.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let bal = indexer.get_taddress_balance(vec![taddr]).await?;
        assert_eq!(u64::try_from(i64::from(bal)).unwrap_or(0), SEND_AMOUNT);
        Ok(())
    }
}

/// Block-range (pool filters, range edges) and faucet-coinbase taddr gRPC queries
mod zebrad {
    use super::*;

    /// - `get_block_range` with no pools == explicit shielded pools
    /// - Tip block = shielded coinbase + the send, no transparent data
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_default_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // fund_and_send(Orchard): one shielded coinbase note, then send it to the
        // recipient's unified address and mine the send in.
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let ua = recipient.address(Pool::Orchard.ztest()).await?;
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
        // The tip block holds the shielded coinbase and the send.
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
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_all_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // Three shielded coinbase notes (one per send below), then one send to
        // each pool's recipient address, mined into a single block.
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 3).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let mut txids = Vec::new();
        // NU6.3: the unified-address (Orchard-receiver) send emits Ironwood
        // actions, so the compact block carries them under `ironwood_actions`.
        for pool in [Pool::Transparent, Pool::Sapling, Pool::Ironwood] {
            let addr = recipient.address(pool.ztest()).await?;
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
        // coinbase + the three sends
        assert_eq!(compact_block.vtx.len(), 4);

        assert_pool_present(compact_block, &txids[0], Pool::Transparent);
        assert_pool_present(compact_block, &txids[1], Pool::Sapling);
        assert_pool_present(compact_block, &txids[2], Pool::Ironwood);
        // The unified-address send must carry no Orchard actions from NU6.3.
        assert_pool_absent(compact_block, &txids[2], Pool::Orchard);
        Ok(())
    }

    /// Transparent mining → every compact-block tx carries a transparent vout with a
    /// non-empty `script_pub_key`
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_data_in_compact_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env
            .add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let chain_height = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(chain_height, READY).await?;

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        // Zaino cannot serve the non-standard genesis coinbase script in compact blocks, so
        // this starts at height 1, not 0 (zingolabs/zaino#818).
        let start = BlockHeight::from(1u32);
        let range = indexer.get_block_range_with_pools(start, chain_height, all_pools).await?;
        let heights = u32::from(chain_height) as usize;
        assert_eq!(range.len(), heights, "every height in [1, {chain_height:?}]");
        for cb in range {
            for tx in cb.vtx {
                let vout = tx.vout.first().expect("transparent vout present");
                assert!(!vout.script_pub_key.is_empty(), "transparent output carries a script");
            }
        }
        Ok(())
    }

    /// Faucet coinbase taddr balance == Σ validator `getaddressutxos` (zero / truncated fails)
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance_faucet() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env
            .add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &indexer).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
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

        let balance = indexer.get_taddress_balance(vec![faucet_taddr]).await?;
        assert_eq!(i64::from(balance), utxo_total, "balance = Σ validator's utxos");
        Ok(())
    }

    /// - Faucet coinbase taddr, `max_entries = 3` → exactly three utxos (miner paid every block)
    /// - Streamed reply == unary reply over the same arguments
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_stream_faucet() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env
            .add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &indexer).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let start = BlockHeight::from(2u32);
        let streamed =
            indexer.get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3).await?;
        assert_eq!(streamed.len(), 3, "`max_entries` binds (every block pays the miner)");
        let unary = indexer.get_address_utxos(vec![faucet_taddr], start, 3).await?;
        assert_eq!(streamed, unary, "stream = unary");
        Ok(())
    }

    /// Drain 1 to 106 (both inclusive) on a 100-block chain → the 100 available blocks, then an
    /// error (not a clean end)
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_upper_bound() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env
            .add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let height = u32::from(indexer.latest_block_height().await?);
        let tip = validator.generate_blocks(100u32.saturating_sub(height)).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let (start, end) = (BlockHeight::from(1u32), BlockHeight::from(106u32));
        let (blocks, errored) = indexer.drain_block_range(start, end, all_pools).await?;

        let compact_block = blocks.last().expect("non-empty range");
        assert_eq!(compact_block.height, 100, "drain stops at the tip, not the requested end");
        assert_eq!(blocks.len(), 100);
        assert!(errored, "stream should terminate with an error");
        Ok(())
    }

    /// Drain inverted range 106 to 1 (both inclusive) → no blocks, then an error (not a clean end)
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_lower_bound() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env
            .add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent.ztest()));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let height = u32::from(indexer.latest_block_height().await?);
        let tip = validator.generate_blocks(100u32.saturating_sub(height)).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let (start, end) = (BlockHeight::from(106u32), BlockHeight::from(1u32));
        let (blocks, errored) = indexer.drain_block_range(start, end, all_pools).await?;

        assert!(blocks.is_empty(), "descending from past the tip serves nothing: {blocks:?}");
        assert!(errored, "stream should terminate with an error");
        Ok(())
    }
}
