//! Wallet-to-validator end-to-end tests: an in-process librustzcash wallet
//! (faucet + recipient) drives Zaino-in-a-pod over its gRPC / JSON-RPC surface.
//!
//! Ported from dev's `devtool.rs` (which drove the zcash-devtool client); here
//! the wallet is ztest's `Wallet::librustzcash()` and Zaino runs in a zainod pod
//! queried via the ztest `indexer` handle. Test names / structure / invariants
//! mirror dev 1:1.
//!
//! Backend matrix: dev's `<Service>` axis was NOT uniform — dev's
//! `mod fetch_service` held 25 tests and `mod state_service` only 12. The
//! send / mining-reward / `get_transaction_mined` / `get_raw_mempool` /
//! `get_mempool_info` core ran on BOTH backends; the address-query, treestate,
//! subtree, `get_raw_transaction`, `get_mempool_tx`/`get_mempool_stream` and
//! `get_transaction_mempool` family ran on the FetchService ONLY. We mirror that
//! split 1:1: tests dev ran on both carry `#[rstest]` `#[case::fetch]` /
//! `#[case::state]`; the fetch-only family carries `#[case::fetch]` alone (their
//! fetch-vs-state agreement is separately covered by the `cross_service` module,
//! which keeps dev's standalone comparisons). `Fetch` is a `zaino-fetch` pod
//! reading zebrad over RPC, `State` a `zaino-state` pod reading zebrad's DB as a
//! RocksDB secondary off a shared volume (see [`single_env`]).
//!
//! Funding: dev funds the faucet from `SHIELDED_FUNDING_POOL` (orchard). The
//! `zfnd/zebra` image can't mine a valid orchard coinbase (block-2+ fails the
//! halo2 proof), so we fund from **sapling**
//! shielded coinbase — equivalent shielded funding (no maturity wait, chain
//! stays short and off the finalised seam), and the send/balance invariants
//! under test are independent of which pool funded the faucet.

use std::time::Duration;

use anyhow::Result;
use rstest::rstest;
use ztest::prelude::*;

/// Standard transfer amount (zatoshis), matching dev's sends.
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;
/// Shielded funding pool for the faucet coinbase (see module note).
const FUND: Pool = Pool::Sapling;
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

/// Wait for the indexer to index up to `tip`.
async fn wait_tip(indexer: &(impl IndexerBackend + ?Sized), tip: BlockHeight) -> Result<()> {
    indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;
    Ok(())
}

/// Pool-type filters for `GetBlockRange`, matching zaino's `PoolType` wire enum:
/// transparent=1, sapling=2, orchard=3, ironwood=4. Ironwood must be included:
/// zaino's default (empty `poolTypes`) filter is every shielded pool
/// (`PoolTypeFilter::default` = sapling+orchard+ironwood), so the explicit
/// shielded set has to match it or the default-vs-explicit parity checks diverge.
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];
const SHIELDED_POOLS: [i32; 3] = [2, 3, 4];

/// The tip block hash (display order), via the indexer's `getbestblockhash`.
async fn best_block_hash(irpc: &JsonRpcClient) -> Result<String> {
    let v = irpc
        .call_value("getbestblockhash", serde_json::json!([]))
        .await?;
    Ok(v.as_str()
        .expect("getbestblockhash returns a hash string")
        .to_string())
}

/// The string elements of a JSON array response (e.g. the `getrawmempool`
/// txid list); non-strings and non-arrays yield an empty vec.
fn json_string_array(v: serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A `getblockdeltas` delta's `outputs` array (empty if absent).
fn delta_outputs(delta: &serde_json::Value) -> Vec<serde_json::Value> {
    delta
        .get("outputs")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A `getblockdeltas` delta's `inputs` array (empty if absent).
fn delta_inputs(delta: &serde_json::Value) -> Vec<serde_json::Value> {
    delta
        .get("inputs")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The `satoshis` (zatoshis) of a `getblockdeltas` output entry.
fn output_satoshis(output: &serde_json::Value) -> Option<i64> {
    output.get("satoshis").and_then(serde_json::Value::as_i64)
}

mod zebrad {
    use super::*;

    /// The indexer backend a wallet test runs against — dev's `<Service>` axis.
    #[derive(Clone, Copy)]
    enum Backend {
        Fetch,
        State,
    }

    /// Build a single-indexer env for `backend`, mining coinbase to `mine_to`,
    /// plus a librustzcash wallet. The dev `<Service>` instantiation, as a pod:
    /// `Fetch` = one `regtest` zainod reading zebrad over RPC; `State` = a
    /// persistent-state zebrad on a shared volume + one `regtest_state_in`
    /// zainod (RocksDB secondary) — mirrors `cross_service::two_pods_mining_to`.
    async fn single_env(
        backend: Backend,
        mine_to: Pool,
    ) -> Result<(TestEnv, ZebraValidator, ZainoIndexer, ztest::LrzWallet)> {
        let mut env = TestEnv::builder().ready_timeout(SYNC_TIMEOUT);
        // `vol` must outlive `build()` for the State backend (as in two_pods).
        let vol = match backend {
            Backend::State => Some(env.shared_volume("zebra-db")),
            Backend::Fetch => None,
        };
        let validator = match &vol {
            Some(vol) => env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(mine_to)
                    .persistent_state_in(vol),
            ),
            None => env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(mine_to)),
        };
        let indexer = match &vol {
            Some(vol) => env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest_state_in(vol, &validator)
                    .named("zaino-state"),
            ),
            None => env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest()),
        };
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;
        Ok((env, validator, indexer, wallet))
    }

    /// The `send_to_pool` family: the faucet sends 250_000 to the recipient's
    /// `pool` address; the recipient's synced wallet shows it.
    async fn send_to_pool(backend: Backend, pool: Pool) -> Result<()> {
        let (_env, validator, indexer, wallet) = single_env(backend, FUND).await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(pool).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        recipient.sync().await?;

        assert_eq!(
            recipient.balances().await?.get(pool),
            SEND_AMOUNT,
            "recipient {pool:?} balance must equal the send"
        );
        Ok(())
    }

    /// Fund the faucet, send 250_000 to the recipient's `pool` address, mine it
    /// in, and return the env/handles plus the send txid and recipient address.
    /// The queries below hit the indexer, not the wallet.
    async fn fund_and_send_to(
        backend: Backend,
        pool: Pool,
    ) -> Result<(TestEnv, ZebraValidator, ZainoIndexer, TxId, String)> {
        let (env, validator, indexer, wallet) = single_env(backend, FUND).await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(pool).await?;
        let txid = faucet
            .send(&addr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        Ok((env, validator, indexer, txid, addr))
    }

    /// Broadcast (unmined) a transparent and a unified send; returns the
    /// env/handles and the two txids.
    async fn fill_mempool(
        backend: Backend,
    ) -> Result<(TestEnv, ZebraValidator, ZainoIndexer, TxId, TxId)> {
        let (env, validator, indexer, wallet) = single_env(backend, FUND).await?;
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 2)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        let ua = recipient.address(Pool::Orchard).await?;
        let t_txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("txid");
        let u_txid = faucet
            .send(&ua, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("txid");
        tokio::time::sleep(Duration::from_secs(1)).await;
        Ok((env, validator, indexer, t_txid, u_txid))
    }

    /// Port of `receives_mining_reward`: the faucet's synced wallet holds a
    /// spendable shielded coinbase note. (dev asserts orchard coinbase; we fund
    /// from sapling — see module note — so we assert the sapling note.)
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn receives_mining_reward(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, wallet) = single_env(backend, FUND).await?;
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let balances = faucet.balances().await?;
        assert!(
            balances.get(FUND) > 0,
            "faucet must hold a spendable shielded coinbase note, got {balances:?}"
        );
        Ok(())
    }

    /// Port of `connect_to_node_get_info`: faucet and recipient wallets connect
    /// and sync without error (smoke).
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_to_node_get_info(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, wallet) = single_env(backend, FUND).await?;
        let _faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        recipient.sync().await?;
        indexer.indexer_info().await?;
        Ok(())
    }

    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_ironwood(#[case] backend: Backend) -> Result<()> {
        // The recipient's unified address exposes an Orchard receiver, but from
        // NU6.3 librustzcash routes the output value to the Ironwood pool
        // (Orchard is spend-locked), so the receipt lands in — and is asserted
        // against — the Ironwood balance. Verified on-chain: the send credits
        // `ironwood`, not `orchard`.
        send_to_pool(backend, Pool::Ironwood).await
    }

    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_sapling(#[case] backend: Backend) -> Result<()> {
        send_to_pool(backend, Pool::Sapling).await
    }

    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent(#[case] backend: Backend) -> Result<()> {
        send_to_pool(backend, Pool::Transparent).await
    }

    /// Port of `send_to_all`: one faucet funds a send to all three pools; each
    /// recipient pool reports 250_000.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_all(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, wallet) = single_env(backend, FUND).await?;

        // Three notes — one per send (no chaining of unconfirmed change).
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 3)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        // NU6.3: the unified-address (Orchard-receiver) output routes to Ironwood.
        for pool in [Pool::Ironwood, Pool::Sapling, Pool::Transparent] {
            let addr = recipient.address(pool).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
        }
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        recipient.sync().await?;

        let balances = recipient.balances().await?;
        assert_eq!(balances.get(Pool::Ironwood), SEND_AMOUNT);
        // From NU6.3 the unified-address output routes to Ironwood; the orchard
        // pool must stay empty (a nonzero orchard here means the receipt was
        // mislabelled, not merely misrouted).
        assert_eq!(balances.get(Pool::Orchard), 0);
        assert_eq!(balances.get(Pool::Sapling), SEND_AMOUNT);
        assert_eq!(balances.get(Pool::Transparent), SEND_AMOUNT);
        Ok(())
    }

    /// Port of `shield_for_validator`: the recipient receives a transparent
    /// 250_000, shields it to orchard, and reports 250_000 − 15_000 fee.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield_for_validator(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, wallet) = single_env(backend, FUND).await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let taddr = recipient.address(Pool::Transparent).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        recipient.sync().await?;
        assert_eq!(
            recipient.balances().await?.get(Pool::Transparent),
            SEND_AMOUNT
        );

        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        recipient.sync().await?;
        assert_eq!(
            recipient.balances().await?.get(Pool::Ironwood),
            SEND_AMOUNT - SHIELD_FEE,
            "shielded balance must be the send net of the ZIP-317 fee \
             (NU6.3 shields transparent funds into the Ironwood pool)"
        );
        Ok(())
    }

    /// Port of `get_address_tx_ids`: `getaddresstxids` over the recipient's
    /// taddr returns the send's txid.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, txid, taddr) = fund_and_send_to(backend, Pool::Transparent).await?;
        let start = u32::from(indexer.latest_block_height().await?).saturating_sub(2);
        let irpc = indexer.json_rpc().await?;
        let res = irpc
            .call_value(
                "getaddresstxids",
                serde_json::json!([{ "addresses": [taddr], "start": start }]),
            )
            .await?;
        let txids: Vec<String> = res
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        // dev asserted the send is the *first* txid the address query returns
        // (devtool.rs:357 `assert_eq!(txid_hex.trim(), txids[0])`).
        assert_eq!(
            txids[0],
            txid.to_string(),
            "getaddresstxids first txid must be the send {txid}, got {txids:?}"
        );
        Ok(())
    }

    /// Port of `get_address_utxos`: `z_getaddressutxos` over the recipient's
    /// taddr returns a utxo whose txid is the send's.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, txid, taddr) = fund_and_send_to(backend, Pool::Transparent).await?;
        let utxos = indexer
            .get_address_utxos(vec![taddr], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(
            utxos[0].txid,
            txid.as_ref().to_vec(),
            "utxo[0] txid must be the send"
        );
        Ok(())
    }

    /// Port of `get_address_balance`: `getaddressbalance` over the recipient's
    /// taddr reports exactly 250_000.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, taddr) =
            fund_and_send_to(backend, Pool::Transparent).await?;
        let res = indexer
            .json_rpc()
            .await?
            .call_value(
                "getaddressbalance",
                serde_json::json!([{ "addresses": [taddr] }]),
            )
            .await?;
        assert_eq!(
            res.get("balance").and_then(serde_json::Value::as_u64),
            Some(SEND_AMOUNT),
            "getaddressbalance must report the send amount: {res:?}"
        );
        Ok(())
    }

    /// Port of `get_taddress_balance`: `GetTaddressBalance` over the recipient's
    /// taddr reports 250_000.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, taddr) =
            fund_and_send_to(backend, Pool::Transparent).await?;
        let bal = indexer.get_taddress_balance(vec![taddr]).await?;
        assert_eq!(
            u64::try_from(i64::from(bal)).unwrap_or(0),
            SEND_AMOUNT,
            "get_taddress_balance must report the send amount"
        );
        Ok(())
    }

    /// Port of `get_taddress_txids` (smoke): `get_taddress_txids` over the
    /// recipient's taddr and a range around the send succeeds.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, taddr) =
            fund_and_send_to(backend, Pool::Transparent).await?;
        let tip = indexer.latest_block_height().await?;
        let start = BlockHeight::from(u32::from(tip).saturating_sub(2));
        let _ = indexer.get_taddress_txids(taddr, start, tip).await?;
        Ok(())
    }

    /// Port of `get_taddress_utxos` (smoke): `get_address_utxos` over the
    /// recipient's taddr succeeds.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, taddr) =
            fund_and_send_to(backend, Pool::Transparent).await?;
        let _ = indexer
            .get_address_utxos(vec![taddr], BlockHeight::from(0u32), 0)
            .await?;
        Ok(())
    }

    /// Port of `get_taddress_utxos_stream` (smoke): `get_address_utxos_stream`
    /// over the recipient's taddr succeeds.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos_stream(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, taddr) =
            fund_and_send_to(backend, Pool::Transparent).await?;
        let _ = indexer
            .get_address_utxos_stream(vec![taddr], BlockHeight::from(0u32), 0)
            .await?;
        Ok(())
    }

    /// Port of `z_get_treestate` (smoke): tree state at the tip succeeds.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, _addr) = fund_and_send_to(backend, Pool::Orchard).await?;
        let tip = indexer.latest_block_height().await?;
        let _ = indexer.get_tree_state(tip).await?;
        Ok(())
    }

    /// Port of `z_get_subtrees_by_index` (smoke): orchard subtree roots succeed.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, _txid, _addr) = fund_and_send_to(backend, Pool::Orchard).await?;
        let _ = indexer
            .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
            .await?;
        Ok(())
    }

    /// Port of `get_raw_transaction` (smoke): `getrawtransaction` for the orchard
    /// send's txid succeeds.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, txid, _addr) = fund_and_send_to(backend, Pool::Orchard).await?;
        let _ = indexer
            .json_rpc()
            .await?
            .call_value(
                "getrawtransaction",
                serde_json::json!([txid.to_string(), 1]),
            )
            .await?;
        Ok(())
    }

    /// Port of `get_transaction_mined` (smoke): the indexer serves
    /// `get_transaction` for the mined orchard send by txid.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mined(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, txid, _addr) = fund_and_send_to(backend, Pool::Orchard).await?;
        let _ = indexer.get_transaction(txid).await?;
        Ok(())
    }

    /// Port of `get_transaction_mempool` (smoke): the indexer serves
    /// `get_transaction` for an unmined orchard send from the mempool.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mempool(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, wallet) = single_env(backend, FUND).await?;
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(Pool::Orchard).await?;
        let txid = faucet
            .send(&addr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("txid");
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = indexer.get_transaction(txid).await?;
        Ok(())
    }

    /// Port of `get_raw_mempool`: the indexer's `getrawmempool` matches the
    /// validator's, with two unmined transactions.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, _t, _u) = fill_mempool(backend).await?;
        // `getrawmempool` returns the mempool txid set in unspecified order, so
        // dev sorted both sides before comparing (devtool.rs:
        // `zaino_mempool.sort(); validator_mempool.sort(); assert_eq!`). The
        // generic `assert_rpc_parity` compares arrays positionally and so flags
        // a spurious mismatch when the two sides enumerate the same txids in a
        // different order; sort both txid lists and compare as sets, matching
        // dev's normalization.
        let mut validator_txids = json_string_array(
            validator
                .json_rpc()
                .await?
                .call_value("getrawmempool", serde_json::json!([]))
                .await?,
        );
        let mut indexer_txids = json_string_array(
            indexer
                .json_rpc()
                .await?
                .call_value("getrawmempool", serde_json::json!([]))
                .await?,
        );
        validator_txids.sort();
        indexer_txids.sort();
        assert_eq!(
            validator_txids, indexer_txids,
            "getrawmempool txid sets must agree (order-insensitive)"
        );
        Ok(())
    }

    /// Port of `get_mempool_tx`: `get_mempool_tx` returns the two unmined
    /// transactions, and the exclude-by-txid-suffix filter drops one.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_tx(#[case] backend: Backend) -> Result<()> {
        let (_env, _v, indexer, t_txid, u_txid) = fill_mempool(backend).await?;
        let mut want = [t_txid.as_ref().to_vec(), u_txid.as_ref().to_vec()];
        want.sort();

        let mut all = indexer.get_mempool_tx(Vec::new()).await?;
        all.sort_by_key(|tx| tx.txid.clone());
        // dev asserted the ordered contents of the 2-tx set, not just the length
        // (devtool.rs:616-633).
        assert_eq!(all.len(), 2, "both unmined txs must be present");
        assert_eq!(all[0].txid, want[0]);
        assert_eq!(all[1].txid, want[1]);

        // Excluding the first by its txid suffix leaves only the second.
        let remaining = indexer.get_mempool_tx(vec![want[0][8..].to_vec()]).await?;
        assert_eq!(remaining.len(), 1, "excluding one leaves the other");
        assert_eq!(remaining[0].txid, want[1]);
        Ok(())
    }

    /// Port of `get_mempool_stream` (smoke): a mempool subscription observes
    /// unmined transactions.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, _t, _u) = fill_mempool(backend).await?;
        // zaino's GetMempoolStream snapshots the current mempool then stays open
        // until a block is mined, so dev spawned the drain and mined concurrently
        // (draining before mining hangs). Mirror that: subscribe, mine to close,
        // then collect.
        let drain = tokio::spawn({
            let indexer = indexer.clone();
            async move { indexer.get_mempool_stream().await }
        });
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        let txs = drain.await.expect("mempool-stream drain task joins")?;
        assert!(
            !txs.is_empty(),
            "mempool stream must observe the unmined txs"
        );
        Ok(())
    }

    /// Port of `get_mempool_info` (devtool.rs:2225/2262 `get_mempool_info_fetch`
    /// / `get_mempool_info_state`): `getmempoolinfo` matches values recomputed
    /// from the mempool's own contents. dev recomputed `size`/`bytes`/`usage`
    /// from the in-process subscriber internals; over a pod we recompute `size`
    /// and `bytes` from the mempool-stream's serialized transactions. dev's exact
    /// `usage == bytes + Σ txid-key heap-capacity` cannot be matched from a pod
    /// (`key.txid.capacity()` is an in-process heap detail with no RPC surface),
    /// so we assert `usage >= bytes` instead — the one place this port cannot
    /// reproduce dev's assertion 1:1. The exact in-process recompute is preserved
    /// by name as `cross_service::get_mempool_info_{fetch,state}`.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_info(#[case] backend: Backend) -> Result<()> {
        let (_env, validator, indexer, _t, _u) = fill_mempool(backend).await?;

        // Query getmempoolinfo while the two txs are still unmined (mining below
        // clears the mempool, so this must come first).
        let info = indexer
            .json_rpc()
            .await?
            .call_value("getmempoolinfo", serde_json::json!([]))
            .await?;

        // The mempool-stream carries each unmined tx's serialized bytes; recompute
        // the expected byte total from them. zaino's GetMempoolStream snapshots the
        // current mempool then stays open until a block is mined (mining ends the
        // subscription), so — like dev — spawn the drain and mine concurrently;
        // draining before mining hangs (see mempool-stream drain semantics).
        let drain = tokio::spawn({
            let indexer = indexer.clone();
            async move { indexer.get_mempool_stream().await }
        });
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        let txs = drain.await.expect("mempool-stream drain task joins")?;
        let expected_bytes: u64 = txs.iter().map(|tx| tx.data.len() as u64).sum();

        let size = info.get("size").and_then(serde_json::Value::as_u64);
        let bytes = info.get("bytes").and_then(serde_json::Value::as_u64);
        let usage = info.get("usage").and_then(serde_json::Value::as_u64);

        assert_eq!(size, Some(txs.len() as u64), "size must equal the tx count");
        assert!(size.is_some_and(|s| s >= 1), "size must be at least one");
        assert!(bytes.is_some_and(|b| b > 0), "bytes must be positive");
        assert_eq!(
            bytes,
            Some(expected_bytes),
            "bytes must equal Σ serialized-tx lengths"
        );
        assert!(
            matches!((usage, bytes), (Some(u), Some(b)) if u >= b),
            "usage must be at least bytes: {info:?}"
        );
        Ok(())
    }

    /// Port of `send_to_transparent_finalization`. Heavy: a seam-deep advance
    /// past the finalisation seam (`tip - seam`, plus a small margin), which on
    /// this operational chain is ~100 blocks. Under ztest that needs that many
    /// shielded coinbase blocks; the zfnd/zebra image can't mine orchard coinbase
    /// and sapling would cost ~100 groth16 proofs. Gated + ignored, as on dev.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "heavy: seam-deep orchard advance (~100 halo2 proofs); un-ignore + transparent filler when cheap filler mining lands"
    )]
    async fn send_to_transparent_finalization(#[case] _backend: Backend) -> Result<()> {
        panic!(
            "ZTEST GAP: needs a cheap way to advance past the finalisation seam \
             (tip - seam, ~100 blocks) with a spendable shielded coinbase; verify a \
             transparent send is still served after the coinbase finalizes"
        );
    }

    /// Port of `monitor_unverified_mempool`. dev ignores it because the
    /// zcash-devtool wallet has no unconfirmed-balance surface. ztest's zingo
    /// wallet *does* track unconfirmed balances, so this is a candidate to
    /// implement — needs a wallet API to read pending/unconfirmed pool balances
    /// (e.g. Account::unconfirmed_balances).
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "devtool WalletBalance has no unconfirmed_*/confirmed_* fields; balance asserts are commented out — restore + un-ignore when devtool surfaces unconfirmed balances"
    )]
    async fn monitor_unverified_mempool(#[case] _backend: Backend) -> Result<()> {
        panic!(
            "ZTEST GAP: needs an Account unconfirmed/pending balance accessor on \
             Wallet::zingo to assert mempool-tracked balances (zingolib supports it; \
             ztest does not expose it yet)"
        );
    }

    // dev ran these once (not per-`<Service>`); preserved as single tests.

    /// Regression coverage for AP-03 / Zellic #48500 (devtool.rs:1817
    /// `get_block_deltas_resolves_transparent_spend`): the State backend used to
    /// silently drop every *non-coinbase transparent spend input* from
    /// `getblockdeltas`, because Zebra's stateless verbosity-2 transaction object
    /// leaves an input's value/address unset. The fix resolves each spend's
    /// prevout and reads the spent output's value/address.
    ///
    /// The faucet pays the recipient a transparent output; the recipient then
    /// shields it, producing a non-coinbase transparent input that references the
    /// funding output. The spend block's `InputDelta` must be present and resolve
    /// to the funding output's address and full (negated) value — pre-fix this
    /// input was dropped and `inputs` was empty, so balances overstated funds.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_deltas_resolves_transparent_spend() -> Result<()> {
        // The only transparent output in the funding block: coinbase pays the
        // shielded pool and the faucet's change returns shielded, so the funding
        // output is uniquely identifiable by its amount.
        const FUNDING_AMOUNT: i64 = 250_000;

        // State-backend only, mirroring origin/dev: zebra serves no
        // `getblockdeltas` RPC, so only the state backend — which synthesizes
        // the deltas from a verbosity-2 block and resolves each spend's prevout
        // via its `ReadStateService` — can answer it. There is no fetch path and
        // no fetch-vs-state cross-check. The state zainod opens the validator's
        // zebra-state DB as a RocksDB secondary over the shared volume, so the
        // validator is built with `persistent_state_in` on the same `vol`.
        //
        // Coinbase mines to Orchard, matching dev's `SHIELDED_FUNDING_POOL`.
        // Orchard is invalid before NU5 (height 2) and the miner address pins
        // the pool, so `funded_faucet_with_notes` warms the chain past NU5
        // before mining the faucet's notes (mirroring upstream's launch
        // pre-mine). The coinbase is shielded, so the 250_000 transparent send
        // is the funding block's only transparent output.
        //
        // zebra must link the same orchard 0.15 / zcash_protocol 0.10 as the
        // wallet and miner to verify their proofs; an older 5.2.0 (orchard
        // ~0.13) rejects them with "could not validate orchard proof".
        let mut env = TestEnv::builder().ready_timeout(SYNC_TIMEOUT);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Orchard)
                .persistent_state_in(&vol),
        );
        let indexer = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile").regtest_state_in(&vol, &validator),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;
        let irpc = indexer.json_rpc().await?;

        // One shielded coinbase note, then fund the recipient's transparent
        // address with a non-coinbase transparent output.
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let recipient_taddr = recipient.address(Pool::Transparent).await?;
        faucet.send(&recipient_taddr, FUNDING_AMOUNT as u64).await?;
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        let funding_block_hash = best_block_hash(&irpc).await?;

        // The recipient confirms the received output and shields it; the
        // shielding tx spends that output, producing the non-coinbase transparent
        // input under test.
        recipient.sync().await?;
        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        let spend_block_hash = best_block_hash(&irpc).await?;

        // Locate the funding output (unique by amount) and capture its
        // txid / index / address.
        let funding = irpc
            .call_value("getblockdeltas", serde_json::json!([funding_block_hash]))
            .await?;
        let funding_delta = funding
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .find(|d| {
                delta_outputs(d)
                    .iter()
                    .any(|o| output_satoshis(o) == Some(FUNDING_AMOUNT))
            })
            .cloned()
            .expect("funding tx paying the recipient should be in its block");
        let funding_output = delta_outputs(&funding_delta)
            .into_iter()
            .find(|o| output_satoshis(o) == Some(FUNDING_AMOUNT))
            .expect("funding output paying the recipient should be present");
        let funding_txid = funding_delta
            .get("txid")
            .and_then(serde_json::Value::as_str)
            .expect("delta txid")
            .to_string();
        let funding_vout = funding_output
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .expect("output index");
        let funding_address = funding_output
            .get("address")
            .and_then(serde_json::Value::as_str)
            .expect("output address")
            .to_string();

        // The spend's input must be present and resolved to the funding output's
        // address and full value (negative — it is a debit). Pre-fix `inputs` was
        // empty and this lookup would fail.
        let spend = irpc
            .call_value("getblockdeltas", serde_json::json!([spend_block_hash]))
            .await?;
        let input = spend
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .flat_map(|d| delta_inputs(d))
            .find(|i| {
                i.get("prevtxid").and_then(serde_json::Value::as_str) == Some(&funding_txid)
                    && i.get("prevout").and_then(serde_json::Value::as_u64) == Some(funding_vout)
            })
            .expect("spend input referencing the funding output should be present");

        assert_eq!(
            input.get("address").and_then(serde_json::Value::as_str),
            Some(funding_address.as_str()),
            "input must resolve to the prevout's address"
        );
        assert_eq!(
            input.get("satoshis").and_then(serde_json::Value::as_i64),
            Some(-FUNDING_AMOUNT),
            "input must resolve to the prevout's full value, negated"
        );
        Ok(())
    }

    /// Port of devtool.rs:1928
    /// `get_block_deltas_coinbase_only_block_has_no_inputs`: a freshly mined
    /// block carries only its (shielded) coinbase transaction — the coinbase
    /// input is skipped and `getblockdeltas` fabricates no transparent input
    /// deltas.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_deltas_coinbase_only_block_has_no_inputs() -> Result<()> {
        // State-backend only, mirroring origin/dev (cf.
        // `get_block_deltas_resolves_transparent_spend`): zebra serves no
        // `getblockdeltas` RPC, so only the synthesizing state backend can
        // answer it. Coinbase mines to Orchard (`SHIELDED_FUNDING_POOL`); see
        // the sibling test for why the orchard version must match the wallet.
        // This test has no faucet, so it mines the NU5 warmup block itself
        // (height 1, pre-NU5 fallback coinbase) and inspects the height-2 block
        // — the first true Orchard coinbase — which carries only its coinbase
        // tx, so `getblockdeltas` fabricates no transparent inputs.
        let mut env = TestEnv::builder().ready_timeout(SYNC_TIMEOUT);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Orchard)
                .persistent_state_in(&vol),
        );
        let indexer = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile").regtest_state_in(&vol, &validator),
        );
        env.build().await?;
        let irpc = indexer.json_rpc().await?;

        // Height 1 warms past NU5; height 2 is the Orchard coinbase-only block
        // under test.
        let tip = validator.generate_blocks(2).await?;
        wait_tip(&indexer, tip).await?;
        let block_hash = best_block_hash(&irpc).await?;

        let deltas = irpc
            .call_value("getblockdeltas", serde_json::json!([block_hash]))
            .await?;
        let all_empty = deltas
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .all(|d| delta_inputs(d).is_empty());
        assert!(all_empty, "a coinbase-only block must have no input deltas");
        Ok(())
    }

    // These compare a fetch-backend zainod pod against a state-backend zainod
    // pod, both reading one shared zebrad regtest chain (dev ran both zaino
    // services in-process). The two-pod fixture is `two_pods*` below, modelled
    // on `clientless::state_service::two_pods`: one zebrad on a shared volume, a
    // fetch zainod (`regtest`) and a state zainod (`regtest_state_in`). Each test
    // reproduces dev's exact `assert_eq!(fetch, state)` comparison over the pods'
    // gRPC / JSON-RPC surface.
    mod cross_service {
        use super::*;

        const READY: Duration = Duration::from_secs(120);

        /// One zebrad + a fetch-backend zainod pod + a state-backend zainod pod +
        /// a wallet, all sharing the one regtest chain. `mine_to` fixes the
        /// coinbase pool (shielded [`FUND`] unless a test needs transparent
        /// coinbase on the faucet taddr).
        async fn two_pods_mining_to(
            pool: Pool,
        ) -> Result<(
            TestEnv,
            ZebraValidator,
            ZainoIndexer,
            ZainoIndexer,
            ztest::LrzWallet,
        )> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(pool)
                    .persistent_state_in(&vol),
            );
            let fetch = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .named("zaino-fetch"),
            );
            let state = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest_state_in(&vol, &validator)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;
            Ok((env, validator, fetch, state, wallet))
        }

        /// Mine `n` blocks and wait for both pods to index the new tip.
        async fn sync_both(
            validator: &ZebraValidator,
            fetch: &ZainoIndexer,
            state: &ZainoIndexer,
            n: u32,
        ) -> Result<BlockHeight> {
            let tip = validator.generate_blocks(n).await?;
            for idx in [fetch, state] {
                idx.wait_for_block_num(tip, READY).await?;
            }
            Ok(tip)
        }

        /// getrawmempool as a sorted `Vec<String>` for order-independent parity.
        async fn sorted_raw_mempool(irpc: &JsonRpcClient) -> Result<Vec<String>> {
            let v = irpc
                .call_value("getrawmempool", serde_json::json!([]))
                .await?;
            let mut txids: Vec<String> = v
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            txids.sort();
            Ok(txids)
        }

        /// `getaddresstxids` over `taddr` in `[start, end]` as a `Vec<String>`.
        async fn address_tx_ids(
            irpc: &JsonRpcClient,
            taddr: &str,
            start: u32,
            end: u32,
        ) -> Result<Vec<String>> {
            let v = irpc
                .call_value(
                    "getaddresstxids",
                    serde_json::json!([{ "addresses": [taddr], "start": start, "end": end }]),
                )
                .await?;
            Ok(v.as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect())
        }

        /// Port of devtool.rs:772 `block_range_returns_default_pools`:
        /// `get_block_range` with no pools == requesting the shielded pools,
        /// fetch==state, and the tip block holds the shielded coinbase + the send
        /// with no transparent data.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_range_returns_default_pools() -> Result<()> {
            let (_env, validator, fetch, state, wallet) = two_pods_mining_to(FUND).await?;

            // fund_and_send(Orchard): one shielded coinbase note, then send it to
            // the recipient's unified address and mine the send in.
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &fetch, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &fetch).await?;
            let ua = recipient.address(Pool::Orchard).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            let end = sync_both(&validator, &fetch, &state, 1).await?;
            let start = BlockHeight::from(1u32);

            let fetch_default = fetch.get_block_range(start, end).await?;
            let fetch_shielded = fetch
                .get_block_range_with_pools(start, end, SHIELDED_POOLS.to_vec())
                .await?;
            assert_eq!(fetch_default, fetch_shielded);

            let state_shielded = state
                .get_block_range_with_pools(start, end, SHIELDED_POOLS.to_vec())
                .await?;
            let state_default = state.get_block_range(start, end).await?;
            assert_eq!(state_default, state_shielded);

            assert_eq!(fetch_default, state_default);

            let compact_block = state_default.last().expect("non-empty range");
            assert_eq!(BlockHeight::from(compact_block.height as u32), end);
            // The tip block holds the shielded coinbase and the send.
            assert_eq!(compact_block.vtx.len(), 2);
            assert_eq!(compact_block.vtx.last().expect("send tx").index, 1);
            for tx in &compact_block.vtx {
                assert!(
                    tx.vin.is_empty(),
                    "transparent data should be absent when no pool types are requested"
                );
                assert!(
                    tx.vout.is_empty(),
                    "transparent data should be absent when no pool types are requested"
                );
            }
            Ok(())
        }

        /// Port of devtool.rs:863 `block_range_returns_all_pools`: with all pools
        /// requested the fetch and state indexers agree, and the tip block carries
        /// the coinbase plus all three sends with their pool data.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_range_returns_all_pools() -> Result<()> {
            let (_env, validator, fetch, state, wallet) = two_pods_mining_to(FUND).await?;

            // Three shielded coinbase notes (one per send below), then one send to
            // each pool's recipient address, mined into a single block.
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &fetch, 3)
                .await?;
            let recipient = wallet.recipient(&validator, &fetch).await?;
            let mut txids = Vec::new();
            // NU6.3: the unified-address (Orchard-receiver) send emits Ironwood
            // actions, so the compact block carries them under `ironwood_actions`.
            for pool in [Pool::Transparent, Pool::Sapling, Pool::Ironwood] {
                let addr = recipient.address(pool).await?;
                let txid = faucet
                    .send(&addr, SEND_AMOUNT)
                    .await?
                    .into_iter()
                    .next()
                    .expect("txid");
                txids.push(txid);
            }
            let end = sync_both(&validator, &fetch, &state, 1).await?;
            let start = BlockHeight::from(1u32);

            let fetch_range = fetch
                .get_block_range_with_pools(start, end, ALL_POOLS.to_vec())
                .await?;
            let state_range = state
                .get_block_range_with_pools(start, end, ALL_POOLS.to_vec())
                .await?;
            assert_eq!(fetch_range, state_range);

            let compact_block = state_range.last().expect("non-empty range");
            assert_eq!(BlockHeight::from(compact_block.height as u32), end);
            // coinbase + the three sends
            assert_eq!(compact_block.vtx.len(), 4);

            e2e::assert_pool_present(compact_block, &txids[0], e2e::Pool::Transparent);
            e2e::assert_pool_present(compact_block, &txids[1], e2e::Pool::Sapling);
            e2e::assert_pool_present(compact_block, &txids[2], e2e::Pool::Ironwood);
            // The unified-address send must carry no Orchard actions from NU6.3.
            e2e::assert_pool_absent(compact_block, &txids[2], e2e::Pool::Orchard);
            Ok(())
        }

        /// Launch dual pods, fund the faucet, send 250_000 to the recipient's
        /// `pool` address, mine it in, and return the pods + the send txid + the
        /// recipient address. The dual-backend analogue of `fund_and_send_to`.
        async fn fund_and_send_dual(
            pool: Pool,
        ) -> Result<(
            TestEnv,
            ZebraValidator,
            ZainoIndexer,
            ZainoIndexer,
            TxId,
            String,
        )> {
            let (env, validator, fetch, state, wallet) = two_pods_mining_to(FUND).await?;
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &fetch, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &fetch).await?;
            let addr = recipient.address(pool).await?;
            let txid = faucet
                .send(&addr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("txid");
            sync_both(&validator, &fetch, &state, 1).await?;
            Ok((env, validator, fetch, state, txid, addr))
        }

        /// Port of devtool.rs:1034 `z_get_treestate_fetch_vs_state`: the fetch and
        /// state indexers agree on the tree state at the tip.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_treestate_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state, _txid, _addr) = fund_and_send_dual(Pool::Orchard).await?;
            let tip = fetch.latest_block_height().await?;
            assert_eq!(
                fetch.get_tree_state(tip).await?,
                state.get_tree_state(tip).await?
            );
            Ok(())
        }

        /// Port of devtool.rs:1055 `z_get_subtrees_by_index_fetch_vs_state`: the
        /// fetch and state indexers agree on orchard subtree roots.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_subtrees_by_index_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state, _txid, _addr) = fund_and_send_dual(Pool::Orchard).await?;
            assert_eq!(
                fetch
                    .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                    .await?,
                state
                    .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                    .await?,
            );
            Ok(())
        }

        /// Port of devtool.rs:1075 `get_raw_transaction_fetch_vs_state`: the fetch
        /// and state indexers agree on `getrawtransaction` for the send.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_transaction_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state, txid, _addr) = fund_and_send_dual(Pool::Orchard).await?;
            let params = serde_json::json!([txid.to_string(), 1]);
            assert_eq!(
                fetch
                    .json_rpc()
                    .await?
                    .call_value("getrawtransaction", params.clone())
                    .await?,
                state
                    .json_rpc()
                    .await?
                    .call_value("getrawtransaction", params)
                    .await?,
            );
            Ok(())
        }

        /// Port of devtool.rs:1097 `get_address_tx_ids_fetch_vs_state`:
        /// `getaddresstxids` over the recipient's taddr returns the send txid, and
        /// the fetch and state indexers agree.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_tx_ids_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state, txid, taddr) =
                fund_and_send_dual(Pool::Transparent).await?;
            let tip = u32::from(fetch.latest_block_height().await?);
            let (start, end) = (tip.saturating_sub(2), tip);
            let fetch_txids = address_tx_ids(&fetch.json_rpc().await?, &taddr, start, end).await?;
            let state_txids = address_tx_ids(&state.json_rpc().await?, &taddr, start, end).await?;
            assert_eq!(fetch_txids[0], txid.to_string());
            assert_eq!(fetch_txids, state_txids);
            Ok(())
        }

        /// Port of devtool.rs:1130 `get_address_utxos_fetch_vs_state`:
        /// `z_getaddressutxos` over the recipient's taddr returns the send txid,
        /// and the fetch and state indexers agree.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state, txid, taddr) =
                fund_and_send_dual(Pool::Transparent).await?;
            let z = BlockHeight::from(0u32);
            let fetch_utxos = fetch.get_address_utxos(vec![taddr.clone()], z, 0).await?;
            let state_utxos = state.get_address_utxos(vec![taddr], z, 0).await?;
            assert_eq!(fetch_utxos[0].txid, txid.as_ref().to_vec());
            assert_eq!(fetch_utxos[0].txid, state_utxos[0].txid);
            Ok(())
        }

        /// Port of devtool.rs:1297 `get_address_balance_fetch_vs_state`: the
        /// recipient taddr reports the 250_000 send, and the fetch and state
        /// indexers agree.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_balance_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state, _txid, taddr) =
                fund_and_send_dual(Pool::Transparent).await?;
            let params = serde_json::json!([{ "addresses": [taddr] }]);
            let fetch_bal = fetch
                .json_rpc()
                .await?
                .call_value("getaddressbalance", params.clone())
                .await?;
            let state_bal = state
                .json_rpc()
                .await?
                .call_value("getaddressbalance", params)
                .await?;
            assert_eq!(
                fetch_bal.get("balance").and_then(serde_json::Value::as_u64),
                Some(SEND_AMOUNT)
            );
            assert_eq!(fetch_bal, state_bal);
            Ok(())
        }

        /// Broadcast a transparent and a unified send (unmined) so both pods'
        /// mempools hold them. The dual-backend analogue of `fill_mempool`.
        async fn fill_mempool_dual() -> Result<(TestEnv, ZebraValidator, ZainoIndexer, ZainoIndexer)>
        {
            let (env, validator, fetch, state, wallet) = two_pods_mining_to(FUND).await?;
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &fetch, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &fetch).await?;
            let taddr = recipient.address(Pool::Transparent).await?;
            let ua = recipient.address(Pool::Orchard).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok((env, validator, fetch, state))
        }

        /// Port of devtool.rs:1191 `get_raw_mempool_fetch_vs_state`: the fetch and
        /// state indexers agree on `getrawmempool` while two sends sit unmined.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_mempool_fetch_vs_state() -> Result<()> {
            let (_env, _v, fetch, state) = fill_mempool_dual().await?;
            assert_eq!(
                sorted_raw_mempool(&fetch.json_rpc().await?).await?,
                sorted_raw_mempool(&state.json_rpc().await?).await?,
            );
            Ok(())
        }

        /// Port of devtool.rs:1211 `get_address_transactions_regtest`: after a
        /// transparent send, the state indexer's transparent-address txid query
        /// over that taddr yields at least one transaction.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_transactions_regtest() -> Result<()> {
            let (_env, _v, fetch, state, _txid, taddr) =
                fund_and_send_dual(Pool::Transparent).await?;
            let chain_height = fetch.latest_block_height().await?;
            let start = BlockHeight::from(u32::from(chain_height).saturating_sub(2));
            let txids = state.get_taddress_txids(taddr, start, chain_height).await?;
            assert!(
                !txids.is_empty(),
                "at least one tx must touch the recipient taddr"
            );
            Ok(())
        }

        /// Port of devtool.rs:1249 `transparent_data_in_compact_block`: with
        /// transparent mining, every compact-block tx carries a transparent vout
        /// (the miner's transparent coinbase is the data source), so each vout's
        /// `script_pub_key` is non-empty.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn transparent_data_in_compact_block() -> Result<()> {
            let (_env, validator, fetch, state, _wallet) =
                two_pods_mining_to(Pool::Transparent).await?;
            let chain_height = sync_both(&validator, &fetch, &state, 5).await?;

            // NOTE: Zaino cannot serve the non-standard genesis coinbase script in
            // compact blocks, so this starts at height 1, not 0
            // (zingolabs/zaino#818).
            let range = state
                .get_block_range_with_pools(
                    BlockHeight::from(1u32),
                    chain_height,
                    ALL_POOLS.to_vec(),
                )
                .await?;
            for cb in range {
                for tx in cb.vtx {
                    assert!(
                        !tx.vout
                            .first()
                            .expect("transparent vout present")
                            .script_pub_key
                            .is_empty(),
                        "each tx's transparent output must carry a script pub key"
                    );
                }
            }
            Ok(())
        }

        /// Launch transparent-mining dual pods and mine `blocks` coinbase blocks
        /// to the faucet's transparent address; return the pods and that taddr.
        /// Under `mine_to(Transparent)` the faucet account is the miner address,
        /// so its taddr holds the coinbase — dev's faucet-taddr cluster.
        async fn transparent_faucet_taddr(
            blocks: u32,
        ) -> Result<(TestEnv, ZainoIndexer, ZainoIndexer, String)> {
            let (env, validator, fetch, state, wallet) =
                two_pods_mining_to(Pool::Transparent).await?;
            let faucet = wallet.faucet(&validator, &fetch).await?;
            let faucet_taddr = faucet.address(Pool::Transparent).await?;
            sync_both(&validator, &fetch, &state, blocks).await?;
            Ok((env, fetch, state, faucet_taddr))
        }

        /// Port of devtool.rs:1360 `get_taddress_txids_faucet_fetch_vs_state`: the
        /// fetch and state indexers agree on `getaddresstxids` over the faucet's
        /// coinbase taddr. The non-vacuity probe guards against a silent
        /// empty==empty pass.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_txids_faucet_fetch_vs_state() -> Result<()> {
            let (_env, fetch, state, faucet_taddr) = transparent_faucet_taddr(100).await?;
            let fetch_txids = address_tx_ids(&fetch.json_rpc().await?, &faucet_taddr, 2, 5).await?;
            let state_txids = address_tx_ids(&state.json_rpc().await?, &faucet_taddr, 2, 5).await?;
            assert!(
                !fetch_txids.is_empty(),
                "faucet taddr must hold coinbase txids in range"
            );
            assert_eq!(fetch_txids, state_txids);
            Ok(())
        }

        /// Port of devtool.rs:1389 `get_taddress_balance_faucet_fetch_vs_state`:
        /// the fetch and state indexers agree on the transparent balance of the
        /// faucet's coinbase taddr. The non-vacuity probe guards against 0==0.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_balance_faucet_fetch_vs_state() -> Result<()> {
            let (_env, fetch, state, faucet_taddr) = transparent_faucet_taddr(5).await?;
            let fetch_bal = fetch
                .get_taddress_balance(vec![faucet_taddr.clone()])
                .await?;
            let state_bal = state.get_taddress_balance(vec![faucet_taddr]).await?;
            assert!(
                i64::from(fetch_bal) > 0,
                "faucet taddr must hold coinbase value"
            );
            assert_eq!(i64::from(fetch_bal), i64::from(state_bal));
            Ok(())
        }

        /// Port of devtool.rs:1421 `get_address_utxos_faucet_fetch_vs_state`: the
        /// fetch and state indexers agree on `get_address_utxos` over the faucet's
        /// coinbase taddr.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos_faucet_fetch_vs_state() -> Result<()> {
            let (_env, fetch, state, faucet_taddr) = transparent_faucet_taddr(5).await?;
            let start = BlockHeight::from(2u32);
            let fetch_utxos = fetch
                .get_address_utxos(vec![faucet_taddr.clone()], start, 3)
                .await?;
            let state_utxos = state
                .get_address_utxos(vec![faucet_taddr], start, 3)
                .await?;
            assert!(
                !fetch_utxos.is_empty(),
                "faucet taddr must hold coinbase utxos"
            );
            assert_eq!(fetch_utxos, state_utxos);
            Ok(())
        }

        /// Port of devtool.rs:1449 `get_address_utxos_stream_faucet_fetch_vs_state`:
        /// the streamed utxos agree between the fetch and state indexers over the
        /// faucet's coinbase taddr.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos_stream_faucet_fetch_vs_state() -> Result<()> {
            let (_env, fetch, state, faucet_taddr) = transparent_faucet_taddr(5).await?;
            let start = BlockHeight::from(2u32);
            let fetch_utxos = fetch
                .get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3)
                .await?;
            let state_utxos = state
                .get_address_utxos_stream(vec![faucet_taddr], start, 3)
                .await?;
            assert!(
                !fetch_utxos.is_empty(),
                "faucet taddr must hold coinbase utxos"
            );
            assert_eq!(fetch_utxos, state_utxos);
            Ok(())
        }

        /// Launch transparent-mining dual pods and mine up to chain height 100.
        async fn transparent_to_height_100() -> Result<(TestEnv, ZainoIndexer, ZainoIndexer)> {
            let (env, validator, fetch, state, _wallet) =
                two_pods_mining_to(Pool::Transparent).await?;
            let height = u32::from(fetch.latest_block_height().await?);
            sync_both(&validator, &fetch, &state, 100u32.saturating_sub(height)).await?;
            Ok((env, fetch, state))
        }

        /// Port of devtool.rs:1511 `get_block_range_out_of_range_upper_bound`:
        /// draining [1, 106] on a 100-block chain yields the 100 available blocks
        /// (fetch == state) and then errors rather than ending cleanly.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_out_of_range_upper_bound() -> Result<()> {
            let (_env, fetch, state) = transparent_to_height_100().await?;
            let (start, end) = (BlockHeight::from(1u32), BlockHeight::from(106u32));

            let (fetch_blocks, fetch_errored) = fetch
                .drain_block_range(start, end, ALL_POOLS.to_vec())
                .await?;
            let (state_blocks, state_errored) = state
                .drain_block_range(start, end, ALL_POOLS.to_vec())
                .await?;

            assert_eq!(fetch_blocks, state_blocks);
            let compact_block = state_blocks.last().expect("non-empty range");
            assert!(compact_block.height < 106);
            assert_eq!(fetch_blocks.len(), 100);
            assert!(state_errored, "state stream should terminate with an error");
            assert!(fetch_errored, "fetch stream should terminate with an error");
            Ok(())
        }

        /// Port of devtool.rs:1551 `get_block_range_out_of_range_lower_bound`:
        /// draining the inverted range [106, 1] yields no blocks (fetch == state,
        /// both empty) and then errors rather than ending cleanly.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_out_of_range_lower_bound() -> Result<()> {
            let (_env, fetch, state) = transparent_to_height_100().await?;
            let (start, end) = (BlockHeight::from(106u32), BlockHeight::from(1u32));

            let (fetch_blocks, fetch_errored) = fetch
                .drain_block_range(start, end, ALL_POOLS.to_vec())
                .await?;
            let (state_blocks, state_errored) = state
                .drain_block_range(start, end, ALL_POOLS.to_vec())
                .await?;

            assert_eq!(fetch_blocks, state_blocks);
            assert!(fetch_blocks.is_empty());
            assert!(state_errored, "state stream should terminate with an error");
            assert!(fetch_errored, "fetch stream should terminate with an error");
            Ok(())
        }

        /// Port of devtool.rs:1655 `address_deltas` (fetch-vs-state
        /// `getaddressdeltas`). BLOCKED: the zaino pod's JSON-RPC serves
        /// `getblockdeltas` but NOT `getaddressdeltas`, and dev drove the
        /// in-process `state_subscriber.get_address_deltas(...)` API, which has no
        /// gRPC/JSON-RPC pod surface. dev additionally `#[ignore]`d it for a
        /// devtool coinbase-maturity off-by-one on `shield_faucet`. Preserved 1:1
        /// by name until zaino exposes `getaddressdeltas` over the pod.
        #[test]
        #[cfg_attr(
            not(feature = "devtool-incompatible"),
            ignore = "devtool `shield` drains all transparent funds, so it always includes the freshly-mined tip-99 coinbase; devtool computes coinbase maturity against the target height (tip+1) while zebra's mempool enforces it against the current tip, so that coinbase is immature by one block and the shield is rejected — un-ignore when devtool fixes the coinbase-maturity off-by-one"
        )]
        fn address_deltas() {
            // Asserted (devtool.rs:1655): Simple/Filtered/WithChainInfo variants,
            // the recipient send delta at output index 0, multi-address deltas,
            // start/end clamping to the tip, and empty deltas for a non-existent
            // address.
        }

        /// Port of devtool.rs:2225 `get_mempool_info_fetch`: `getmempoolinfo`
        /// matches values recomputed from the fetch subscriber's mempool
        /// internals. BLOCKED: dev recomputed `bytes`/`usage` from
        /// `FetchServiceSubscriber.indexer.get_mempool_txids()` /
        /// `get_mempool_transactions()`, in-process APIs with no pod surface for
        /// the exact byte/usage recompute. The reachable subset (size, bytes from
        /// the mempool stream, usage >= bytes) is covered by
        /// `zebrad::get_mempool_info`.
        #[test]
        #[ignore = "in-process subscriber mempool internals have no pod surface"]
        fn get_mempool_info_fetch() {
            // Asserted (devtool.rs:2225): info.size == values.len(); size >= 1;
            // info.bytes == Σ serialized-tx lengths; info.usage == bytes + Σ
            // txid-key hex-string capacities.
        }

        /// Port of devtool.rs:2262 `get_mempool_info_state`: `getmempoolinfo`
        /// matches values recomputed from the state subscriber's mempool
        /// internals. BLOCKED: dev recomputed from
        /// `StateServiceSubscriber.mempool.get_mempool()` / `serialized_tx`,
        /// in-process APIs with no pod surface. Reachable subset covered by
        /// `zebrad::get_mempool_info`.
        #[test]
        #[ignore = "in-process subscriber mempool internals have no pod surface"]
        fn get_mempool_info_state() {
            // Asserted (devtool.rs:2262): entries.len() == info.size; size >= 1;
            // info.bytes == Σ serialized_tx lengths; info.usage == bytes + Σ
            // txid-key capacities.
        }

        /// Rewrite of devtool.rs:2065 `get_outpoint_spenders_fetch_vs_state`.
        /// BLOCKED: this drives the IN-PROCESS `zaino_state::ChainIndex` API
        /// (`snapshot_nonfinalized_state` + `get_outpoint_spenders` with
        /// `ChainScope::{FullChain, Finalised}`), which has no gRPC/JSON-RPC
        /// surface — not reachable from a ztest pod test.
        ///
        /// dev's assertions (devtool.rs:2065-2140): build three transparent
        /// outpoints at the recipient taddr — one spent and buried past the
        /// finalised seam (a small margin below `tip - seam`; ~105 blocks on the
        /// operational chain), one spent but left in the non-finalised window,
        /// one left unspent — then for BOTH backends:
        ///   - `ChainScope::FullChain`  => vec![Some(spender_finalised), Some(spender_nonfinalised), None]
        ///   - `ChainScope::Finalised`  => vec![Some(spender_finalised), None, None]
        /// i.e. FullChain resolves both spends, Finalised only the buried one.
        /// Preserved 1:1 by name; do not delete.
        #[test]
        #[cfg_attr(
            not(feature = "devtool-incompatible"),
            ignore = "heavy: mines ~105 orchard-coinbase blocks (~105 halo2 proofs) to bury a finalised spend below the finalisation seam; un-ignore for manual / dedicated CI"
        )]
        fn get_outpoint_spenders_fetch_vs_state() {}
    }
}
