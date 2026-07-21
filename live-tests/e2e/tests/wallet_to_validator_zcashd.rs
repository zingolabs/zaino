//! Wallet-to-validator end-to-end tests against a **zcashd**-backed Zaino.
//!
//! Entirely gated on `zcashd_support` (mirrors dev's `devtool_zcashd.rs`).
//! Ported onto ztest's `Wallet::librustzcash()` + a zainod pod. zcashd *can* mine a
//! valid orchard coinbase to the abandon-art faucet address, so the faucet is
//! funded from orchard shielded coinbase directly (no sapling workaround needed
//! as on the zebra image). Names / structure mirror dev 1:1.
#![cfg(feature = "zcashd_support")]

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const SEND_AMOUNT: u64 = 250_000;
const SHIELD_FEE: u64 = 15_000;
const FUND: Pool = Pool::Orchard;
const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

async fn wait_tip(indexer: &(impl IndexerBackend + ?Sized), tip: BlockHeight) -> Result<()> {
    indexer.wait_for_block_num(tip, SYNC_TIMEOUT).await?;
    Ok(())
}

#[ztest::qos::wallet]
#[tokio::test(flavor = "multi_thread")]
async fn faucet_receives_zcashd_orchard_reward() -> Result<()> {
    let mut env = TestEnv::builder();
    let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest().mine_to(FUND));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    let wallet = env.add_wallet(Wallet::librustzcash());
    env.build().await?;

    let faucet = wallet
        .funded_faucet_with_notes(&validator, &indexer, 1)
        .await?;
    assert!(
        faucet.balances().await?.get(Pool::Orchard) > 0,
        "faucet must hold a spendable orchard coinbase note"
    );
    Ok(())
}

mod wallet_to_validator {
    use super::*;

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_to_node_get_info() -> Result<()> {
        let mut env = TestEnv::builder();
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;
        let _faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        recipient.sync().await?;
        assert!(!indexer.indexer_info().await?.chain_name.is_empty());
        Ok(())
    }

    async fn send_to_pool(pool: Pool) -> Result<()> {
        let mut env = TestEnv::builder();
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(pool).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        recipient.sync().await?;
        assert_eq!(recipient.balances().await?.get(pool), SEND_AMOUNT);
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_orchard() -> Result<()> {
        send_to_pool(Pool::Orchard).await
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_sapling() -> Result<()> {
        send_to_pool(Pool::Sapling).await
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent() -> Result<()> {
        send_to_pool(Pool::Transparent).await
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield() -> Result<()> {
        let mut env = TestEnv::builder();
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

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
            recipient.balances().await?.get(Pool::Orchard),
            SEND_AMOUNT - SHIELD_FEE
        );
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "heavy: seam-deep orchard advance (~100 halo2 proofs); un-ignore + transparent filler when cheap filler mining lands"
    )]
    async fn send_to_transparent_finalization() -> Result<()> {
        panic!(
            "ZTEST GAP: needs a cheap seam-deep advance across the finalization boundary \
             (tip - FAST_TEST_MAX_NONFINALISED_DEPTH); verify a transparent send is still \
             served after the coinbase finalizes"
        );
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "heavy: seam-deep orchard advance (~100 halo2 proofs); re-port light or un-ignore with transparent filler"
    )]
    async fn send_to_all() -> Result<()> {
        let mut env = TestEnv::builder();
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 3)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        for pool in [Pool::Orchard, Pool::Sapling, Pool::Transparent] {
            let addr = recipient.address(pool).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
        }
        let tip = validator.generate_blocks(1).await?;
        wait_tip(&indexer, tip).await?;
        recipient.sync().await?;
        let b = recipient.balances().await?;
        assert_eq!(b.get(Pool::Orchard), SEND_AMOUNT);
        assert_eq!(b.get(Pool::Sapling), SEND_AMOUNT);
        assert_eq!(b.get(Pool::Transparent), SEND_AMOUNT);
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "devtool WalletBalance has no unconfirmed_*/confirmed_* fields; balance asserts commented out — restore + un-ignore when the wallet surfaces unconfirmed balances"
    )]
    async fn monitor_unverified_mempool() -> Result<()> {
        panic!(
            "ZTEST GAP: needs an Account unconfirmed/pending balance accessor on Wallet::zingo \
             (zingolib supports it; ztest does not expose it yet)"
        );
    }
}

/// zcashd-vs-zaino JSON-RPC oracle tests (dev's `mod json_server`). Each funds a
/// wallet through the faucet, then compares zcashd's own JSON-RPC (the validator
/// pod) against zaino's JSON-RPC (the zainod pod) over the resulting funded state
/// — balances / utxos / txids / mempool / treestate / subtrees / rawtx / gettxout.
///
/// Harness swap only: dev ran two in-process `FetchService` subscribers (one on
/// zcashd, one on zaino) and compared typed structs; here it is
/// `validator.json_rpc()` vs `indexer.json_rpc()` compared as `serde_json::Value`
/// via [`assert_rpc_parity`] (the same approach as `clientless::json_server`).
/// Assertions, funding amounts, pools, and the sent-txid checks are preserved 1:1.
///
/// Funding note: the abandon-art faucet is funded from orchard coinbase, and
/// zcashd rejects orchard→sapling, so the dev-UA send (which routes orchard on
/// zcashd) is reproduced as an orchard-address send; no sapling send is
/// introduced (dev avoided it too).
mod json_server {
    use super::*;
    use anyhow::Context;
    use serde_json::{json, Value};
    use zaino_testutils::assert_rpc_parity;
    use ztest::LrzWallet;

    async fn dual() -> Result<(TestEnv, ZcashdValidator, ZainoIndexer, LrzWallet)> {
        let mut env = TestEnv::builder();
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest().mine_to(FUND));
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;
        Ok((env, validator, indexer, wallet))
    }

    /// ztest analogue of dev's `jsonrpc_fund`: fund the faucet with orchard
    /// coinbase notes, sync, fetch the recipient's transparent + orchard
    /// addresses, and if `send` is `Some(pool)`, send 250_000 to that pool's
    /// recipient address and mine it in. Returns the funded faucet plus
    /// `(recipient_taddr, recipient_oaddr, sent_txid_hex)`. The send=None mempool
    /// tests broadcast two unmined sends off the returned faucet, so they need
    /// two spendable notes.
    async fn jsonrpc_fund(
        validator: &ZcashdValidator,
        indexer: &ZainoIndexer,
        wallet: &LrzWallet,
        send: Option<Pool>,
    ) -> Result<(Account<LrzWallet>, String, String, Option<String>)> {
        let notes: u32 = if send.is_some() { 1 } else { 2 };
        let faucet = wallet
            .funded_faucet_with_notes(validator, indexer, notes)
            .await?;
        let recipient = wallet.recipient(validator, indexer).await?;

        let recipient_taddr = recipient.address(Pool::Transparent).await?;
        let recipient_oaddr = recipient.address(Pool::Orchard).await?;

        let sent = if let Some(pool) = send {
            let addr = recipient.address(pool).await?;
            let txids = faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            wait_tip(indexer, tip).await?;
            Some(txids[0].to_string())
        } else {
            None
        };

        Ok((faucet, recipient_taddr, recipient_oaddr, sent))
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_address_balance() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (_faucet, recipient_taddr, _oaddr, _txid) =
            jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Transparent)).await?;

        let zrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let params = format!(r#"[{{"addresses": ["{recipient_taddr}"]}}]"#);
        let balance = assert_rpc_parity("getaddressbalance", &params, &zrpc, &irpc, &[]).await?;
        assert_eq!(
            balance.get("balance").and_then(Value::as_u64),
            Some(SEND_AMOUNT)
        );
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (faucet, recipient_taddr, recipient_oaddr, _txid) =
            jsonrpc_fund(&validator, &indexer, &wallet, None).await?;

        faucet.send(&recipient_taddr, SEND_AMOUNT).await?;
        faucet.send(&recipient_oaddr, SEND_AMOUNT).await?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        let zrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let mut zcashd_mempool = zrpc.call_value("getrawmempool", json!([])).await?;
        let mut zaino_mempool = irpc.call_value("getrawmempool", json!([])).await?;
        if let Some(a) = zcashd_mempool.as_array_mut() {
            a.sort_by(|x, y| x.to_string().cmp(&y.to_string()));
        }
        if let Some(a) = zaino_mempool.as_array_mut() {
            a.sort_by(|x, y| x.to_string().cmp(&y.to_string()));
        }
        assert_eq!(zcashd_mempool, zaino_mempool);
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_info() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (faucet, recipient_taddr, recipient_oaddr, _txid) =
            jsonrpc_fund(&validator, &indexer, &wallet, None).await?;

        faucet.send(&recipient_taddr, SEND_AMOUNT).await?;
        faucet.send(&recipient_oaddr, SEND_AMOUNT).await?;

        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_rpc_parity(
            "getmempoolinfo",
            "",
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Orchard)).await?;

        let chain_height = indexer.latest_block_height().await?;
        let params = format!(r#"["{}"]"#, u32::from(chain_height));
        assert_rpc_parity(
            "z_gettreestate",
            &params,
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Orchard)).await?;

        assert_rpc_parity(
            "z_getsubtreesbyindex",
            r#"["orchard", 0]"#,
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (_faucet, _taddr, _oaddr, tx) =
            jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Orchard)).await?;
        let tx = tx.expect("jsonrpc_fund sends a tx when given Some(pool)");

        let params = format!(r#"["{tx}", 1]"#);
        assert_rpc_parity(
            "getrawtransaction",
            &params,
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_tx_out() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (_faucet, recipient_taddr, _oaddr, _txid) =
            jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Transparent)).await?;

        let zrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;

        let zcashd_utxos = zrpc
            .call_value("getaddressutxos", json!([{"addresses": [recipient_taddr]}]))
            .await?;
        let first = zcashd_utxos
            .as_array()
            .and_then(|a| a.first())
            .context("zcashd getaddressutxos returned no utxos")?;
        let txid = first
            .get("txid")
            .and_then(Value::as_str)
            .context("utxo.txid")?
            .to_string();
        let output_index = first
            .get("outputIndex")
            .and_then(Value::as_u64)
            .context("utxo.outputIndex")?;

        let present = format!(r#"["{txid}", {output_index}, true]"#);
        assert_rpc_parity("gettxout", &present, &zrpc, &irpc, &[]).await?;

        let missing = format!(r#"["{txid}", {}]"#, output_index + 100);
        assert_rpc_parity("gettxout", &missing, &zrpc, &irpc, &[]).await?;
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (_faucet, recipient_taddr, _oaddr, tx) =
            jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Transparent)).await?;
        let tx = tx.expect("jsonrpc_fund sends a tx when given Some(pool)");

        let zrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let chain_height = u32::from(indexer.latest_block_height().await?);

        let params = format!(
            r#"[{{"addresses": ["{recipient_taddr}"], "start": {}, "end": {}}}]"#,
            chain_height - 2,
            chain_height
        );
        let zcashd_txids = zrpc
            .call_value("getaddresstxids", serde_json::from_str(&params)?)
            .await?;
        let zaino_txids = irpc
            .call_value("getaddresstxids", serde_json::from_str(&params)?)
            .await?;

        assert_eq!(
            zcashd_txids
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str),
            Some(tx.as_str())
        );
        assert_eq!(zcashd_txids, zaino_txids);
        Ok(())
    }

    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_address_utxos() -> Result<()> {
        let (_env, validator, indexer, wallet) = dual().await?;
        let (_faucet, recipient_taddr, _oaddr, txid_1) =
            jsonrpc_fund(&validator, &indexer, &wallet, Some(Pool::Transparent)).await?;
        let txid_1 = txid_1.expect("jsonrpc_fund sends a tx when given Some(pool)");

        let zrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let params = json!([{"addresses": [recipient_taddr]}]);
        let zcashd_utxos = zrpc.call_value("getaddressutxos", params.clone()).await?;
        let zaino_utxos = irpc.call_value("getaddressutxos", params).await?;

        let zcashd_txid = zcashd_utxos
            .as_array()
            .and_then(|a| a.first())
            .and_then(|u| u.get("txid"))
            .and_then(Value::as_str)
            .context("zcashd utxo.txid")?;
        let zaino_txid = zaino_utxos
            .as_array()
            .and_then(|a| a.first())
            .and_then(|u| u.get("txid"))
            .and_then(Value::as_str)
            .context("zaino utxo.txid")?;

        assert_eq!(txid_1, zcashd_txid);
        assert_eq!(zcashd_txid, zaino_txid);
        Ok(())
    }
}
