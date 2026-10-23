//! In-process librustzcash wallet (faucet + recipient) → zainod pod over gRPC
//!
//! - Sends + shielding credited per pool, address txids across the durable seam
//! - Unmined txs: `GetMempoolTx`, `GetMempoolStream`, `GetTransaction` (mempool → mined)
//! - Address queries: `get_taddress_*` / `get_address_utxos*` (recipient + faucet coinbase)
//! - Tree state, block-range pool filters
//! - Validator JSON-RPC = oracle (zainod serves gRPC only)

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
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
/// zaino's mempool mirror poll period
const MEMPOOL_POLL: Duration = Duration::from_millis(500);
/// Send returned → mirrored ceiling (20 mirror polls: a stalled mirror fails here, not at `READY`)
const MIRRORED_WITHIN: Duration = Duration::from_secs(10);
/// No stream-open signal → grace before mining (mined first = drain waits a further tip)
const STREAM_OPEN_GRACE: Duration = Duration::from_secs(5);

/// Display-order txid of a wire (internal-order) txid; bad width → `<N bytes>`
fn txid_hex(wire: &[u8]) -> String {
    <[u8; 32]>::try_from(wire)
        .map_or_else(|_| format!("<{} bytes>", wire.len()), |b| TxId::from_bytes(b).to_string())
}

/// First txid of an `Account::send` to `to`
fn sent_txid(sent: Result<Vec<TxId>, RpcError>, to: &str) -> Result<TxId> {
    let txids = sent.with_context(|| format!("send to {to}"))?;
    txids.into_iter().next().with_context(|| format!("send to {to} returned no txid"))
}

/// Polls until zaino's mempool = zebrad's `getrawmempool` and holds every `sent` txid →
/// the agreed txids, sorted
///
/// - `sent` = non-vacuity premise (never-relayed sends leave both sides empty and equal)
async fn mirrored_mempool(
    vrpc: &JsonRpcClient,
    indexer: &impl IndexerBackend,
    sent: &[TxId],
) -> Result<Vec<String>> {
    let sent: Vec<String> = sent.iter().map(TxId::to_string).collect();
    let deadline = tokio::time::Instant::now() + READY;
    loop {
        let reply = vrpc.call_value("getrawmempool", json!([])).await?;
        let mut zebrad: Vec<String> = reply
            .as_array()
            .with_context(|| format!("getrawmempool not an array: {reply}"))?
            .iter()
            .filter_map(|txid| txid.as_str().map(str::to_owned))
            .collect();
        zebrad.sort();
        let mut zaino: Vec<String> = indexer
            .get_mempool_tx(Vec::new())
            .await
            .context("GetMempoolTx, no exclusions")?
            .iter()
            .map(|tx| txid_hex(&tx.txid))
            .collect();
        zaino.sort();
        let missing: Vec<&String> = sent.iter().filter(|txid| !zebrad.contains(txid)).collect();
        if zaino == zebrad && missing.is_empty() {
            return Ok(zebrad);
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "mempool mirror never converged in {READY:?}: zaino {zaino:?}, zebrad {zebrad:?}, \
             sent but absent from zebrad {missing:?}"
        );
        tokio::time::sleep(MEMPOOL_POLL).await;
    }
}

mod wallet {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Faucet's coinbase notes → Ironwood; one faucet → all three pools, mined into one block
    ///
    /// - All pools requested → tip block = coinbase + each send with its pool data
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_to_every_pool_carry_their_pool_data_and_credit_once() -> Result<()> {
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
        let mut txids = Vec::new();
        for pool in [Pool::Transparent, Pool::Sapling, Pool::Ironwood] {
            let addr = recipient.address(pool).await?;
            txids.push(sent_txid(faucet.send(&addr, SEND_AMOUNT).await, &addr)?);
        }
        let end = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(end, READY).await?;

        let all_pools = vec![1, 2, 3, 4];
        let start = BlockHeight::from(1u32);

        let range = indexer.get_block_range_with_pools(start, end, all_pools).await?;

        let compact_block = range.last().context("non-empty range")?;
        assert_eq!(BlockHeight::from(compact_block.height as u32), end, "last served block");
        let served: Vec<String> = compact_block.vtx.iter().map(|tx| txid_hex(&tx.txid)).collect();
        assert_eq!(served.len(), 4, "tip block = coinbase + the three sends: {served:?}");

        let mut sends = Vec::new();
        for txid in &txids {
            let tx = compact_block
                .vtx
                .iter()
                .find(|tx| tx.txid == txid.as_ref().to_vec())
                .with_context(|| format!("tip block carries send {txid}: {served:?}"))?;
            sends.push(tx);
        }
        let (transparent, sapling, ironwood) = (sends[0], sends[1], sends[2]);
        assert!(!transparent.vout.is_empty(), "transparent send {}: vout served", txids[0]);
        assert!(!sapling.outputs.is_empty(), "sapling send {}: outputs served", txids[1]);
        let unified = (ironwood.ironwood_actions.len(), ironwood.actions.len());
        // NU6.3: unified-address send = Ironwood actions only
        let only_ironwood = unified.0 > 0 && unified.1 == 0;
        assert!(only_ironwood, "unified send {}: (ironwood, orchard) {unified:?}", txids[2]);

        recipient.sync().await?;
        // Nonzero orchard = receipt mislabelled (NU6.3: unified output → Ironwood)
        let expected = PoolBalances {
            orchard: 0,
            ironwood: SEND_AMOUNT,
            sapling: SEND_AMOUNT,
            transparent: SEND_AMOUNT,
        };
        assert_eq!(recipient.balances().await?, expected, "recipient's confirmed balances");
        Ok(())
    }

    /// Transparent receipt through every address query, buried below the finalized tip, shielded
    ///
    /// - Txids over a span around the send = exactly the send, validator's bytes, same once durable
    /// - One utxo (the send's txid, address, value, height); streamed = unary; balance = the send
    /// - Advance mined to `FILLER_ADDRESS` (to `FUND` = `SEAM_ADVANCE` halo2 proofs, past the cap)
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_receipt_served_across_the_seam_then_shielded() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let credited = Pool::Ironwood;
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        let txid = sent_txid(faucet.send(&taddr, SEND_AMOUNT).await, &taddr)?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let mined = u64::from(u32::from(tip));

        let start = BlockHeight::from(u32::from(tip).saturating_sub(2));
        let non_finalized_txs = indexer
            .get_taddress_txids(taddr.clone(), start, tip)
            .await
            .with_context(|| format!("GetTaddressTxids {taddr} over [{start:?}, {tip:?}]"))?;
        let heights: Vec<u64> = non_finalized_txs.iter().map(|tx| tx.height).collect();
        assert_eq!(heights, [mined], "txids over [{start:?}, {tip:?}] = exactly {txid}");
        let vrpc = validator.json_rpc().await?;
        let raw = vrpc.call_value("getrawtransaction", json!([txid.to_string(), 0])).await?;
        let raw = raw.as_str().with_context(|| format!("getrawtransaction {txid}: {raw}"))?;
        assert!(
            hex::encode(&non_finalized_txs[0].data) == raw,
            "{txid}: zaino's bytes != zebrad's"
        );

        let utxos =
            indexer.get_address_utxos(vec![taddr.clone()], BlockHeight::from(0u32), 0).await?;
        let served: Vec<(String, String, i64, u64)> = utxos
            .iter()
            .map(|u| (txid_hex(&u.txid), u.address.clone(), u.value_zat, u.height))
            .collect();
        let expected = [(txid.to_string(), taddr.clone(), SEND_AMOUNT as i64, mined)];
        assert_eq!(served, expected, "utxos (txid, address, value, height) = the one send");
        let streamed = indexer
            .get_address_utxos_stream(vec![taddr.clone()], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(streamed, utxos, "streamed vs unary utxos");
        let balance = indexer.get_taddress_balance(vec![taddr.clone()]).await?;
        assert_eq!(i64::from(balance), SEND_AMOUNT as i64, "balance = the one utxo");

        let advanced = validator.generate_blocks(SEAM_ADVANCE).to(FILLER_ADDRESS).await?;
        indexer.wait_for_block_num(advanced, READY).await?;

        // - Compact-block `finalized_height` = only proof of the durable commit
        // - Unpublished until the first commit → keep polling
        let deadline = tokio::time::Instant::now() + SEAM_TIMEOUT;
        loop {
            let durable = indexer.finalized_height(ZainoIndex::CompactBlock).await?;
            if durable.is_some_and(|durable| durable >= u32::from(tip)) {
                break;
            }
            let expired = tokio::time::Instant::now() >= deadline;
            anyhow::ensure!(!expired, "durable {durable:?}, never {tip:?} in {SEAM_TIMEOUT:?}");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let durable_txs =
            indexer.get_taddress_txids(taddr.clone(), start, tip).await.with_context(|| {
                format!("GetTaddressTxids {taddr} over [{start:?}, {tip:?}], durable")
            })?;
        let heights: Vec<u64> = durable_txs.iter().map(|tx| tx.height).collect();
        assert_eq!(heights, [mined], "the one send listed at {mined}, durable");
        let (before, after) = (&non_finalized_txs[0].data, &durable_txs[0].data);
        let first_diff = before.iter().zip(after).position(|(b, a)| b != a);
        assert!(
            before == after,
            "send's bytes non-finalized vs durable: {} vs {} bytes, first differing byte \
             {first_diff:?}",
            before.len(),
            after.len(),
        );

        let zaino = i64::from(indexer.get_taddress_balance(vec![taddr.clone()]).await?);
        let reply =
            vrpc.call_value("getaddressbalance", json!([{ "addresses": [&taddr] }])).await?;
        assert_eq!(
            (zaino, reply["balance"].as_i64()),
            (SEND_AMOUNT as i64, Some(SEND_AMOUNT as i64)),
            "{taddr} balance once durable: (zaino, zebrad)",
        );
        recipient.sync().await?;
        let transparent = recipient.balances().await?.transparent;
        assert_eq!(transparent, SEND_AMOUNT, "wallet's transparent balance synced past the seam");

        recipient.shield().await.context("shield the transparent receipt")?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        let shielded = recipient.balances().await?.get(credited);
        assert_eq!(shielded, SEND_AMOUNT - SHIELD_FEE, "net of ZIP-317 fee, in {credited:?}");
        Ok(())
    }

    /// Transparent, Ironwood and Sapling sends, unmined then mined
    ///
    /// - Stream ends when its admitted tip moves → spawn the drain, then mine
    /// - Unconfirmed split unasserted (ztest's librustzcash wallet: no pending-balance accessor)
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn unmined_sends_mirrored_streamed_and_credited_only_once_mined() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // One note per unmined send
        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 3).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Ironwood).await?;
        let zaddr = recipient.address(Pool::Sapling).await?;
        let t_txid = sent_txid(faucet.send(&taddr, SEND_AMOUNT).await, &taddr)?;
        let ua_txid = sent_txid(faucet.send(&ua, SEND_AMOUNT).await, &ua)?;
        let sapling_txid = sent_txid(faucet.send(&zaddr, SEND_AMOUNT).await, &zaddr)?;

        let vrpc = validator.json_rpc().await?;
        let mempool = mirrored_mempool(&vrpc, &indexer, &[t_txid, ua_txid, sapling_txid]).await?;
        let mut sent = vec![t_txid.to_string(), ua_txid.to_string(), sapling_txid.to_string()];
        sent.sort();
        assert_eq!(mempool, sent, "mempool (zaino = zebrad) vs the three sends");

        let suffix = t_txid.as_ref()[8..].to_vec();
        let mut remaining: Vec<String> = indexer
            .get_mempool_tx(vec![suffix])
            .await
            .with_context(|| format!("GetMempoolTx excluding {t_txid}'s suffix"))?
            .iter()
            .map(|tx| txid_hex(&tx.txid))
            .collect();
        remaining.sort();
        let mut shielded_sends = vec![ua_txid.to_string(), sapling_txid.to_string()];
        shielded_sends.sort();
        assert_eq!(remaining, shielded_sends, "excluding {t_txid} by suffix");

        // `PoolBalances` = confirmed-only (mirrored ≠ credited)
        recipient.sync().await?;
        let unconfirmed = recipient.balances().await?;
        assert_eq!(unconfirmed, PoolBalances::default(), "nothing credited while unmined");

        let mut zebrad_raw = Vec::with_capacity(mempool.len());
        for txid in &mempool {
            let raw = vrpc.call_value("getrawtransaction", json!([txid])).await?;
            let raw = raw.as_str().with_context(|| format!("getrawtransaction {txid}: {raw}"))?;
            zebrad_raw.push((raw.to_owned(), txid.clone()));
        }
        let drain = tokio::spawn({
            let indexer = indexer.clone();
            async move { indexer.get_mempool_stream().await }
        });
        tokio::time::sleep(STREAM_OPEN_GRACE).await;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let streamed = drain.await.context("mempool-stream drain task")??;

        // Txid per streamed tx = zebrad's raw-byte match, else the byte length
        let mut served: Vec<(String, u64)> = streamed
            .iter()
            .map(|tx| {
                let hex = hex::encode(&tx.data);
                let txid = zebrad_raw.iter().find(|(raw, _)| *raw == hex).map(|(_, id)| id.clone());
                let txid =
                    txid.unwrap_or_else(|| format!("<{} bytes, not zebrad's>", tx.data.len()));
                (txid, tx.height)
            })
            .collect();
        served.sort();
        let expected: Vec<(String, u64)> = mempool.iter().map(|txid| (txid.clone(), 0)).collect();
        assert_eq!(served, expected, "streamed (txid, height): zebrad's bytes, height 0 = unmined");

        recipient.sync().await?;
        let expected = PoolBalances {
            transparent: SEND_AMOUNT,
            ironwood: SEND_AMOUNT,
            sapling: SEND_AMOUNT,
            ..PoolBalances::default()
        };
        assert_eq!(recipient.balances().await?, expected, "every send credited once mined");
        Ok(())
    }

    /// One Orchard send, unmined then mined into the tip block
    ///
    /// - Mirrored within `MIRRORED_WITHIN` of the send returning
    /// - Unmined → height sentinel `0`, never the tip (else unconfirmed reads as confirmed)
    /// - Served bytes = validator's `getrawtransaction` (independent oracle); mined → its height
    /// - Tip block = shielded coinbase + the send: unfiltered = explicit shielded pools
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn orchard_send_mirrored_at_height_0_promptly_then_served_in_its_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.funded_faucet_with_notes(&validator, &indexer, 1).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let txid = sent_txid(faucet.send(&ua, SEND_AMOUNT).await, &ua)?;
        let sent_at = std::time::Instant::now();
        let vrpc = validator.json_rpc().await?;
        mirrored_mempool(&vrpc, &indexer, &[txid]).await?;
        let mirrored_after = sent_at.elapsed();
        assert!(
            mirrored_after < MIRRORED_WITHIN,
            "{txid}: mirrored {mirrored_after:?} after the send returned, ceiling {MIRRORED_WITHIN:?}"
        );

        let unmined = indexer
            .get_transaction(txid)
            .await
            .with_context(|| format!("GetTransaction {txid}, mirrored unmined"))?;
        assert_eq!(unmined.height, 0, "{txid} unmined = mempool height sentinel, not the tip");
        let raw_hex = vrpc.call_value("getrawtransaction", json!([txid.to_string()])).await?;
        let raw_hex = raw_hex
            .as_str()
            .with_context(|| format!("getrawtransaction {txid} at verbosity 0: {raw_hex}"))?;
        assert!(hex::encode(&unmined.data) == raw_hex, "{txid}: zaino's bytes != zebrad's");

        let end = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(end, READY).await?;
        let tip = u32::from(end);

        let mined = indexer
            .get_transaction(txid)
            .await
            .with_context(|| format!("GetTransaction {txid}, mined at {tip}"))?;
        assert_eq!(mined.height, u64::from(tip), "{txid} mined = confirming height");
        assert!(mined.data == unmined.data, "{txid}: mining changed the served bytes");

        let tree = indexer.get_tree_state(end).await?;
        assert!(!tree.sapling_tree.is_empty(), "sapling tree served at {tip}: {tree:?}");
        assert!(!tree.orchard_tree.is_empty(), "orchard tree served at {tip}: {tree:?}");

        // `PoolType` codes = `Pools::default()` (else default-vs-explicit checks compare 2 filters)
        let shielded_pools = vec![2, 3, 4];
        let start = BlockHeight::from(1u32);

        let default = indexer.get_block_range(start, end).await?;
        let shielded = indexer.get_block_range_with_pools(start, end, shielded_pools).await?;
        assert_eq!(default.len(), shielded.len(), "blocks in [{start:?}, {end:?}]");
        for (unfiltered, explicit) in default.iter().zip(&shielded) {
            let height = unfiltered.height;
            assert_eq!(unfiltered, explicit, "block {height}: unfiltered vs explicit shielded");
        }

        let block = vrpc.call_value("getblock", json!([tip.to_string(), 1])).await?;
        let zebrad: Vec<(u64, String)> = block["tx"]
            .as_array()
            .with_context(|| format!("getblock {tip} carries no tx list: {block}"))?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .zip(0..)
            .map(|(txid, index)| (index, txid))
            .collect();
        let sent_second = zebrad.get(1).is_some_and(|(_, sent)| *sent == txid.to_string());
        assert!(sent_second, "premise: zebrad's block {tip} = [coinbase, {txid}]: {zebrad:?}");
        let compact_block = default.last().context("non-empty range")?;
        assert_eq!(BlockHeight::from(compact_block.height as u32), end, "last served block");
        let served: Vec<(u64, String)> =
            compact_block.vtx.iter().map(|tx| (tx.index, txid_hex(&tx.txid))).collect();
        assert_eq!(served, zebrad, "block {tip} (index, txid): zaino vs zebrad");
        for tx in &compact_block.vtx {
            let transparent = (tx.vin.len(), tx.vout.len());
            let txid = txid_hex(&tx.txid);
            assert_eq!(transparent, (0, 0), "{txid}: (vin, vout) with no pool types requested");
        }
        Ok(())
    }

    /// Faucet coinbase taddr (miner paid every block)
    ///
    /// - Balance = Σ validator `getaddressutxos` (zero / truncated fails)
    /// - `max_entries = 3` binds → exactly three utxos; streamed = unary over the same arguments
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn faucet_coinbase_taddr_balance_and_utxos_match_the_validator() -> Result<()> {
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
            .call_value("getaddressutxos", json!([{ "addresses": [&faucet_taddr] }]))
            .await?;
        let utxo_total: i64 = utxos
            .as_array()
            .with_context(|| format!("getaddressutxos not an array: {utxos}"))?
            .iter()
            .filter_map(|u| u.get("satoshis").and_then(Value::as_i64))
            .sum();
        assert!(utxo_total > 0, "premise: {faucet_taddr} holds coinbase per zebrad: {utxos}");
        let balance = indexer.get_taddress_balance(vec![faucet_taddr.clone()]).await?;
        assert_eq!(i64::from(balance), utxo_total, "{faucet_taddr} balance: zaino vs Σ zebrad's");

        let start = BlockHeight::from(2u32);
        let streamed =
            indexer.get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3).await?;
        let heights: Vec<u64> = streamed.iter().map(|u| u.height).collect();
        assert_eq!(
            heights.len(),
            3,
            "`max_entries` binds (every block pays the miner): {heights:?}"
        );
        let unary = indexer.get_address_utxos(vec![faucet_taddr], start, 3).await?;
        assert_eq!(streamed, unary, "streamed vs unary utxos");
        Ok(())
    }
}
