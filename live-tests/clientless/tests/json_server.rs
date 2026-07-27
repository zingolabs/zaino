//! Tests that compare the output of both `zcashd` and `zainod` through `FetchService`.
//!
//! Entirely gated on `zcashd_support`: every test here launches the
//! zcashd-backed dual fetch services. See
//! docs/adr/0001-zcashd-support-feature-gate.md.
#![cfg(feature = "zcashd_support")]

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::assert_rpc_parity;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

// TODO: This module should not be called `zcashd`
mod zcashd {
    use super::*;

    pub(crate) mod zcash_indexer {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn check_info_no_cookie() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getinfo", "", &zrpc, &irpc, &["timestamp"]).await?;
            let z = zrpc.call_value("getblockchaininfo", json!([])).await?;
            let i = irpc.call_value("getblockchaininfo", json!([])).await?;
            for field in [
                "chain",
                "blocks",
                "bestblockhash",
                "estimatedheight",
                "valuePools",
                "upgrades",
                "consensus",
            ] {
                assert_eq!(
                    z.get(field),
                    i.get(field),
                    "getblockchaininfo.{field} differs from zcashd"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn check_info_with_cookie() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getinfo", "", &zrpc, &irpc, &["timestamp"]).await?;
            let z = zrpc.call_value("getblockchaininfo", json!([])).await?;
            let i = irpc.call_value("getblockchaininfo", json!([])).await?;
            for field in [
                "chain",
                "blocks",
                "bestblockhash",
                "estimatedheight",
                "valuePools",
                "upgrades",
                "consensus",
            ] {
                assert_eq!(
                    z.get(field),
                    i.get(field),
                    "getblockchaininfo.{field} differs from zcashd"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_best_blockhash() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            assert_rpc_parity(
                "getbestblockhash",
                "",
                &validator.json_rpc().await?,
                &indexer.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_count() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            assert_rpc_parity(
                "getblockcount",
                "",
                &validator.json_rpc().await?,
                &indexer.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        /// Checks that the difficulty is the same between zcashd and zaino.
        ///
        /// This tests generates blocks and checks that the difficulty is the same between zcashd and zaino
        /// after each block is generated.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_difficulty() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for _ in 0..10 {
                assert_rpc_parity("getdifficulty", "", &zrpc, &irpc, &[]).await?;
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_deltas() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for _ in 0..10 {
                let hash = zrpc
                    .call_value("getbestblockhash", json!([]))
                    .await?
                    .as_str()
                    .context("getbestblockhash non-string")?
                    .to_string();
                let params = format!(r#"["{hash}"]"#);
                assert_rpc_parity("getblockdeltas", &params, &zrpc, &irpc, &[]).await?;
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_mining_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for _ in 0..10 {
                assert_rpc_parity("getmininginfo", "", &zrpc, &irpc, &[]).await?;
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_tx_out_set_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let z = validator
                .json_rpc()
                .await?
                .call_value("gettxoutsetinfo", json!([]))
                .await?;
            let i = indexer
                .json_rpc()
                .await?
                .call_value("gettxoutsetinfo", json!([]))
                .await?;

            // Structural parity with zcashd: height, bestblock, transactions, txouts and total_amount
            // must match. `bytes_serialized` and `hash_serialized` are Zaino-defined and intentionally
            // diverge from zcashd; only Zaino-internal invariants are asserted on those fields.
            for field in ["height", "bestblock", "transactions", "txouts"] {
                assert_eq!(
                    z.get(field),
                    i.get(field),
                    "gettxoutsetinfo.{field} differs from zcashd"
                );
            }
            let z_amt = z
                .get("total_amount")
                .and_then(Value::as_f64)
                .context("zcashd total_amount")?;
            let i_amt = i
                .get("total_amount")
                .and_then(Value::as_f64)
                .context("zaino total_amount")?;
            assert!(
                (z_amt - i_amt).abs() < 1e-8,
                "gettxoutsetinfo.total_amount differs: zcashd={z_amt} zaino={i_amt}"
            );
            let txouts = i.get("txouts").and_then(Value::as_i64).context("txouts")?;
            let bytes_serialized = i
                .get("bytes_serialized")
                .and_then(Value::as_i64)
                .context("bytes_serialized")?;
            assert_eq!(
                bytes_serialized,
                txouts * 65,
                "bytes_serialized must equal txouts * 65"
            );
            let hash_serialized = i
                .get("hash_serialized")
                .and_then(Value::as_str)
                .context("hash_serialized")?;
            assert_eq!(
                hash_serialized.len(),
                64,
                "hash_serialized must be 64 hex chars"
            );
            assert!(
                hash_serialized.chars().all(|c| c.is_ascii_hexdigit()),
                "hash_serialized must be hex: got {hash_serialized}"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_peer_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            assert_rpc_parity(
                "getpeerinfo",
                "",
                &validator.json_rpc().await?,
                &indexer.json_rpc().await?,
                &[],
            )
            .await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_subsidy() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            assert_rpc_parity(
                "getblocksubsidy",
                "[1]",
                &validator.json_rpc().await?,
                &indexer.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn validate_address() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            // Using a testnet transparent address
            for addr in [
                "tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb",
                "t3TAfQ9eYmXWGe3oPae1XKhdTxm8JvsnFRL",
            ] {
                let params = format!(r#"["{addr}"]"#);
                assert_rpc_parity("validateaddress", &params, &zrpc, &irpc, &[]).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn z_validate_address() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let _indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            clientless::rpc::z_validate_address::run_z_validate_for(&validator.json_rpc().await?)
                .await
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_block() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getblock", r#"["1", 0]"#, &zrpc, &irpc, &[]).await?;
            let block = assert_rpc_parity("getblock", r#"["1", 1]"#, &zrpc, &irpc, &[]).await?;
            let hash = block
                .get("hash")
                .and_then(Value::as_str)
                .context("getblock.hash")?;
            let by_hash = irpc.call_value("getblock", json!([hash, 1])).await?;
            assert_eq!(
                by_hash, block,
                "zaino getblock by-hash must equal by-height"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let zrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for i in 0u32..10 {
                let blk = zrpc
                    .call_value("getblock", json!([i.to_string(), 1]))
                    .await?;
                let hash = blk
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("getblock({i},1).hash missing"))?
                    .to_string();
                let params = format!(r#"["{hash}", false]"#);
                assert_rpc_parity("getblockheader", &params, &zrpc, &irpc, &[]).await?;
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
            }
            Ok(())
        }
    }
}
