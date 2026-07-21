//! Compare Zaino's `FetchService` view against the validator's own JSON-RPC.

use std::time::Duration;

use anyhow::{Context, Result};
use clientless::rpc::z_validate_address::{run_z_validate_for, SaplingSuite};
use serde_json::{json, Value};
use zaino_testutils::{assert_json_equal_ignoring, assert_rpc_parity};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(60);

#[cfg(feature = "zcashd_support")]
mod zcashd {
    use super::*;

    mod launch {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn regtest_no_cache() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let info = indexer.indexer_info().await?;
            assert!(
                !info.chain_name.is_empty(),
                "indexer chain_name must be set: {info:?}"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zcashd::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let info = indexer.indexer_info().await?;
            assert!(
                !info.chain_name.is_empty(),
                "indexer chain_name must be set: {info:?}"
            );
            Ok(())
        }
    }

    mod validation {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn validate_address() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let irpc = indexer.json_rpc().await?;
            for (addr, is_script) in [
                ("tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma", false),
                ("t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2", true),
            ] {
                let i = irpc.call_value("validateaddress", json!([addr])).await?;
                assert_eq!(
                    i.get("isvalid").and_then(Value::as_bool),
                    Some(true),
                    "{addr} isvalid"
                );
                assert_eq!(
                    i.get("address").and_then(Value::as_str),
                    Some(addr),
                    "{addr} address echo"
                );
                assert_eq!(
                    i.get("isscript").and_then(Value::as_bool),
                    Some(is_script),
                    "{addr} isscript"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        pub(crate) async fn z_validate_address() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let irpc = indexer.json_rpc().await?;
            run_z_validate_for(&irpc, SaplingSuite::Standard).await
        }
    }

    mod get {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_raw() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let block = indexer
                .json_rpc()
                .await?
                .call_value("getblock", json!(["1", 0]))
                .await?;
            assert!(
                block.as_str().is_some_and(|s| !s.is_empty()),
                "getblock(1, 0) must return non-empty hex"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_object() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let block = indexer
                .json_rpc()
                .await?
                .call_value("getblock", json!(["1", 1]))
                .await?;
            assert_eq!(
                block.get("height").and_then(Value::as_u64),
                Some(1),
                "getblock(1, 1).height"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_block() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let indexer_tip = indexer.latest_block_height().await?;
            let validator_tip = validator.chain_height().await?;
            assert_eq!(
                indexer_tip, validator_tip,
                "indexer tip ({indexer_tip}) must equal validator tip ({validator_tip})"
            );

            let indexer_best_hash = indexer
                .json_rpc()
                .await?
                .call_value("getbestblockhash", json!([]))
                .await?;
            let validator_best_hash = validator
                .json_rpc()
                .await?
                .call_value("getbestblockhash", json!([]))
                .await?;
            assert_eq!(
                indexer_best_hash, validator_best_hash,
                "indexer best block hash must equal validator best block hash"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let by_height = indexer.get_block(BlockHeight::from(1u32)).await?;
            assert_eq!(by_height.height, 1, "get_block(1).height");
            assert_eq!(by_height.hash.len(), 32, "block hash must be 32 bytes");
            let hash_bytes: [u8; 32] = by_height
                .hash
                .clone()
                .try_into()
                .ok()
                .context("block hash must be 32 bytes")?;
            let by_hash = indexer.get_block_by_hash(BlockHash(hash_bytes)).await?;
            assert_eq!(
                by_height.height, by_hash.height,
                "by-hash height round-trip"
            );
            assert_eq!(by_height.hash, by_hash.hash, "by-hash hash round-trip");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_header() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for i in 0u32..10 {
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
                let blk = vrpc
                    .call_value("getblock", json!([i.to_string(), 1]))
                    .await?;
                let hash = blk
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("getblock({i},1).hash missing"))?
                    .to_string();
                for verbose in [false, true] {
                    let params = format!(r#"["{hash}", {verbose}]"#);
                    assert_rpc_parity("getblockheader", &params, &vrpc, &irpc, &[]).await?;
                }
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn difficulty() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getdifficulty", "", &vrpc, &irpc, &[]).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_deltas() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            // Note: we need an 'expected' block hash in order to query its deltas.
            // Having a predictable or test vector chain is the way to go here.
            let hash = validator
                .json_rpc()
                .await?
                .call_value("getbestblockhash", json!([]))
                .await?
                .as_str()
                .context("getbestblockhash returned non-string")?
                .to_string();
            let params = json!([hash]);
            let v = validator
                .json_rpc()
                .await?
                .call_value("getblockdeltas", params.clone())
                .await?;
            let i = indexer
                .json_rpc()
                .await?
                .call_value("getblockdeltas", params)
                .await?;
            zaino_testutils::assert_json_shape_matches(
                &format!("getblockdeltas({hash})"),
                v.clone(),
                i.clone(),
                &["difficulty"],
                &["hash", "height", "confirmations", "deltas", "chainwork"],
            );
            assert_eq!(
                v.get("difficulty").and_then(Value::as_f64),
                i.get("difficulty").and_then(Value::as_f64),
                "getblockdeltas({hash}) difficulty mismatch (f64-compared)"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mining_info() -> Result<()> {
            // TODO: fix the underlying shape mismatch instead of ignoring columns.
            // zaino's `GetMiningInfoWire` (packages/zaino-fetch/src/jsonrpsee/response/
            // mining_info.rs) lacks `skip_serializing_if = "Option::is_none"` on its
            // zcashd-only fields, so the fetch service re-serializes them as explicit
            // `null`s that zebrad omits entirely. That makes the indexer response an
            // object superset of the validator's, so parity fails on key count.
            // Adding `skip_serializing_if` (or matching zebrad's exact shape) would let
            // us drop everything below except the genuinely volatile solps fields.
            let ignore: &[&str] = &[
                "networksolps",
                "localsolps",
                "errorstimestamp",
                "difficulty",
                "errors",
                "genproclimit",
                "pooledtx",
                "generate",
            ];

            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let v = validator
                .json_rpc()
                .await?
                .call_value("getmininginfo", json!([]))
                .await?;
            let i = indexer
                .json_rpc()
                .await?
                .call_value("getmininginfo", json!([]))
                .await?;
            assert_json_equal_ignoring("getmininginfo", v, i, ignore);
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn peer_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getpeerinfo", "", &vrpc, &irpc, &[]).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_subsidy() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            // getblocksubsidy is a pure function of (height, network schedule): the
            // validator never reads the chain for an explicit height, so no mining is
            // needed — heights far past the tip answer identically. Sweep through the
            // first halving boundary plus a margin on both validators.
            let block_limit = match validator.chain_config().await?.first_halving_height {
                Some(first_halving) => u32::from(first_halving) + 10,
                None => 10,
            };
            let tip = validator.generate_blocks(block_limit).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for height in 0u32..block_limit {
                let params = format!("[{height}]");
                assert_rpc_parity("getblocksubsidy", &params, &vrpc, &irpc, &[]).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn best_blockhash() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(5).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let tip = u32::from(validator.chain_height().await?);
            let irpc = indexer.json_rpc().await?;
            let block = irpc
                .call_value("getblock", json!([tip.to_string(), 1]))
                .await?;
            let block_hash = block
                .get("hash")
                .and_then(Value::as_str)
                .context("getblock.hash missing")?;
            let best_hash = irpc
                .call_value("getbestblockhash", json!([]))
                .await?
                .as_str()
                .context("getbestblockhash returned non-string")?
                .to_string();
            assert_eq!(
                block_hash, best_hash,
                "getblock(tip).hash must equal getbestblockhash"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_count() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let pre = validator.chain_height().await?;
            let tip = validator.generate_blocks(5).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            let count = assert_rpc_parity("getblockcount", "", &vrpc, &irpc, &[]).await?;
            assert_eq!(
                count.as_u64(),
                Some(u64::from(pre + 5)),
                "getblockcount must equal pre+5"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_nullifiers() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(3).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let cb = indexer
                .get_block_nullifiers(BlockHeight::from(1u32))
                .await?;
            assert_eq!(
                cb.height, 1,
                "GetBlockNullifiers must return the requested height"
            );
            assert_eq!(cb.hash.len(), 32, "block hash must be 32 bytes");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(10).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let blocks = indexer
                .get_block_range(BlockHeight::from(1u32), BlockHeight::from(10u32))
                .await?;
            assert_eq!(blocks.len(), 10, "get_block_range(1,10) must yield 10");
            for (i, b) in blocks.iter().enumerate() {
                assert_eq!(b.height, (i + 1) as u64, "block at index {i} height");
                assert_eq!(b.hash.len(), 32, "block hash must be 32 bytes");
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range_nullifiers() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(10).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let blocks = indexer
                .get_block_range_nullifiers(BlockHeight::from(1u32), BlockHeight::from(10u32))
                .await?;
            assert_eq!(
                blocks.len(),
                10,
                "stream must yield exactly 10 entries for 1..=10"
            );
            for (i, cb) in blocks.iter().enumerate() {
                assert_eq!(
                    cb.height as usize,
                    i + 1,
                    "heights must be contiguous from 1"
                );
                assert_eq!(cb.hash.len(), 32, "block hash must be 32 bytes");
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn tree_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let chain_tip = validator.chain_height().await?;
            let ts = indexer.get_tree_state(chain_tip).await?;
            assert_eq!(
                ts.height,
                u64::from(chain_tip),
                "tree state height must equal tip"
            );
            assert_eq!(ts.hash.len(), 64, "tree state hash must be 64 hex chars");
            assert!(ts.time > 0, "tree state time must be positive");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_tree_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let ts = indexer.get_latest_tree_state().await?;
            let tip = u64::from(validator.chain_height().await?);
            assert_eq!(
                ts.height, tip,
                "latest tree state must be at the current tip"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn subtree_roots() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let roots = indexer
                .get_subtree_roots(0, ShieldedProtocol::Sapling, 0)
                .await?;
            for r in &roots {
                assert_eq!(r.root_hash.len(), 32, "subtree root_hash must be 32 bytes");
                assert_eq!(
                    r.completing_block_hash.len(),
                    32,
                    "completing_block_hash must be 32 bytes"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn lightd_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let info = indexer.indexer_info().await?;
            assert!(
                !info.chain_name.is_empty(),
                "lightd_info.chain_name must be set: {info:?}"
            );
            assert!(
                !info.consensus_branch_id.is_empty(),
                "lightd_info.consensus_branch_id must be set: {info:?}"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn get_network_sol_ps() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getnetworksolps", "", &vrpc, &irpc, &[]).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn get_tx_out_set_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let v = validator
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
            // must match. `bytes_serialized` and `hash_serialized` are Zaino-defined (see the
            // `gettxoutsetinfo` spec in zaino-state) and intentionally diverge from zcashd; only
            // Zaino-internal invariants are asserted on those fields.
            for field in ["height", "bestblock", "transactions", "txouts"] {
                assert_eq!(
                    v.get(field),
                    i.get(field),
                    "gettxoutsetinfo.{field} differs from zcashd"
                );
            }
            let v_amt = v
                .get("total_amount")
                .and_then(Value::as_f64)
                .context("validator total_amount")?;
            let i_amt = i
                .get("total_amount")
                .and_then(Value::as_f64)
                .context("indexer total_amount")?;
            assert!(
                (v_amt - i_amt).abs() < 1e-8,
                "gettxoutsetinfo.total_amount differs: zcashd={v_amt} zaino={i_amt}"
            );
            let txouts = i.get("txouts").and_then(Value::as_i64).context("txouts")?;
            let bytes_serialized = i
                .get("bytes_serialized")
                .and_then(Value::as_i64)
                .context("bytes_serialized")?;
            assert_eq!(
                bytes_serialized,
                txouts * 65,
                "bytes_serialized must equal txouts * 65 under zaino's UTXO entry encoding"
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
    }
}

mod zebrad {
    use super::*;

    mod launch {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn regtest_no_cache() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let info = indexer.indexer_info().await?;
            assert!(
                !info.chain_name.is_empty(),
                "indexer chain_name must be set: {info:?}"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zebrad::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let info = indexer.indexer_info().await?;
            assert!(
                !info.chain_name.is_empty(),
                "indexer chain_name must be set: {info:?}"
            );
            Ok(())
        }
    }

    mod validation {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn validate_address() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let irpc = indexer.json_rpc().await?;
            for (addr, is_script) in [
                ("tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma", false),
                ("t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2", true),
            ] {
                let i = irpc.call_value("validateaddress", json!([addr])).await?;
                assert_eq!(
                    i.get("isvalid").and_then(Value::as_bool),
                    Some(true),
                    "{addr} isvalid"
                );
                assert_eq!(
                    i.get("address").and_then(Value::as_str),
                    Some(addr),
                    "{addr} address echo"
                );
                assert_eq!(
                    i.get("isscript").and_then(Value::as_bool),
                    Some(is_script),
                    "{addr} isscript"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        pub(crate) async fn z_validate_address() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let irpc = indexer.json_rpc().await?;
            run_z_validate_for(&irpc, SaplingSuite::ZebradPassthroughFetchService).await
        }
    }

    mod get {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_raw() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let block = indexer
                .json_rpc()
                .await?
                .call_value("getblock", json!(["1", 0]))
                .await?;
            assert!(
                block.as_str().is_some_and(|s| !s.is_empty()),
                "getblock(1, 0) must return non-empty hex"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_object() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let block = indexer
                .json_rpc()
                .await?
                .call_value("getblock", json!(["1", 1]))
                .await?;
            assert_eq!(
                block.get("height").and_then(Value::as_u64),
                Some(1),
                "getblock(1, 1).height"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_block() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let indexer_tip = indexer.latest_block_height().await?;
            let validator_tip = validator.chain_height().await?;
            assert_eq!(
                indexer_tip, validator_tip,
                "indexer tip ({indexer_tip}) must equal validator tip ({validator_tip})"
            );

            let indexer_best_hash = indexer
                .json_rpc()
                .await?
                .call_value("getbestblockhash", json!([]))
                .await?;
            let validator_best_hash = validator
                .json_rpc()
                .await?
                .call_value("getbestblockhash", json!([]))
                .await?;
            assert_eq!(
                indexer_best_hash, validator_best_hash,
                "indexer best block hash must equal validator best block hash"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let by_height = indexer.get_block(BlockHeight::from(1u32)).await?;
            assert_eq!(by_height.height, 1, "get_block(1).height");
            assert_eq!(by_height.hash.len(), 32, "block hash must be 32 bytes");
            let hash_bytes: [u8; 32] = by_height
                .hash
                .clone()
                .try_into()
                .ok()
                .context("block hash must be 32 bytes")?;
            let by_hash = indexer.get_block_by_hash(BlockHash(hash_bytes)).await?;
            assert_eq!(
                by_height.height, by_hash.height,
                "by-hash height round-trip"
            );
            assert_eq!(by_height.hash, by_hash.hash, "by-hash hash round-trip");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_header() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for i in 0u32..10 {
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
                let blk = vrpc
                    .call_value("getblock", json!([i.to_string(), 1]))
                    .await?;
                let hash = blk
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("getblock({i},1).hash missing"))?
                    .to_string();
                for verbose in [false, true] {
                    let params = format!(r#"["{hash}", {verbose}]"#);
                    assert_rpc_parity("getblockheader", &params, &vrpc, &irpc, &[]).await?;
                }
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn difficulty() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getdifficulty", "", &vrpc, &irpc, &[]).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mining_info() -> Result<()> {
            // TODO: fix the underlying shape mismatch instead of ignoring columns.
            // zaino's `GetMiningInfoWire` (packages/zaino-fetch/src/jsonrpsee/response/
            // mining_info.rs) lacks `skip_serializing_if = "Option::is_none"` on its
            // zcashd-only fields, so the fetch service re-serializes them as explicit
            // `null`s that zebrad omits entirely. That makes the indexer response an
            // object superset of the validator's, so parity fails on key count.
            // Adding `skip_serializing_if` (or matching zebrad's exact shape) would let
            // us drop everything below except the genuinely volatile solps fields.
            let ignore: &[&str] = &[
                "networksolps",
                "localsolps",
                "errorstimestamp",
                "difficulty",
                "errors",
                "genproclimit",
                "pooledtx",
                "generate",
            ];

            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let v = validator
                .json_rpc()
                .await?
                .call_value("getmininginfo", json!([]))
                .await?;
            let i = indexer
                .json_rpc()
                .await?
                .call_value("getmininginfo", json!([]))
                .await?;
            assert_json_equal_ignoring("getmininginfo", v, i, ignore);
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn peer_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getpeerinfo", "", &vrpc, &irpc, &[]).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_subsidy() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            // getblocksubsidy is a pure function of (height, network schedule): the
            // validator never reads the chain for an explicit height, so no mining is
            // needed — heights far past the tip answer identically. Sweep through the
            // first halving boundary plus a margin on both validators.
            let block_limit = match validator.chain_config().await?.first_halving_height {
                Some(first_halving) => u32::from(first_halving) + 10,
                None => 10,
            };
            let tip = validator.generate_blocks(block_limit).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            for height in 0u32..block_limit {
                let params = format!("[{height}]");
                assert_rpc_parity("getblocksubsidy", &params, &vrpc, &irpc, &[]).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn best_blockhash() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(5).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let tip = u32::from(validator.chain_height().await?);
            let irpc = indexer.json_rpc().await?;
            let block = irpc
                .call_value("getblock", json!([tip.to_string(), 1]))
                .await?;
            let block_hash = block
                .get("hash")
                .and_then(Value::as_str)
                .context("getblock.hash missing")?;
            let best_hash = irpc
                .call_value("getbestblockhash", json!([]))
                .await?
                .as_str()
                .context("getbestblockhash returned non-string")?
                .to_string();
            assert_eq!(
                block_hash, best_hash,
                "getblock(tip).hash must equal getbestblockhash"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_count() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let pre = validator.chain_height().await?;
            let tip = validator.generate_blocks(5).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            let count = assert_rpc_parity("getblockcount", "", &vrpc, &irpc, &[]).await?;
            assert_eq!(
                count.as_u64(),
                Some(u64::from(pre + 5)),
                "getblockcount must equal pre+5"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_nullifiers() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(3).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let cb = indexer
                .get_block_nullifiers(BlockHeight::from(1u32))
                .await?;
            assert_eq!(
                cb.height, 1,
                "GetBlockNullifiers must return the requested height"
            );
            assert_eq!(cb.hash.len(), 32, "block hash must be 32 bytes");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(10).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let blocks = indexer
                .get_block_range(BlockHeight::from(1u32), BlockHeight::from(10u32))
                .await?;
            assert_eq!(blocks.len(), 10, "get_block_range(1,10) must yield 10");
            for (i, b) in blocks.iter().enumerate() {
                assert_eq!(b.height, (i + 1) as u64, "block at index {i} height");
                assert_eq!(b.hash.len(), 32, "block hash must be 32 bytes");
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range_nullifiers() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(10).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let blocks = indexer
                .get_block_range_nullifiers(BlockHeight::from(1u32), BlockHeight::from(10u32))
                .await?;
            assert_eq!(
                blocks.len(),
                10,
                "stream must yield exactly 10 entries for 1..=10"
            );
            for (i, cb) in blocks.iter().enumerate() {
                assert_eq!(
                    cb.height as usize,
                    i + 1,
                    "heights must be contiguous from 1"
                );
                assert_eq!(cb.hash.len(), 32, "block hash must be 32 bytes");
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn tree_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let chain_tip = validator.chain_height().await?;
            let ts = indexer.get_tree_state(chain_tip).await?;
            assert_eq!(
                ts.height,
                u64::from(chain_tip),
                "tree state height must equal tip"
            );
            assert_eq!(ts.hash.len(), 64, "tree state hash must be 64 hex chars");
            assert!(ts.time > 0, "tree state time must be positive");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_tree_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let ts = indexer.get_latest_tree_state().await?;
            let tip = u64::from(validator.chain_height().await?);
            assert_eq!(
                ts.height, tip,
                "latest tree state must be at the current tip"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn subtree_roots() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let roots = indexer
                .get_subtree_roots(0, ShieldedProtocol::Sapling, 0)
                .await?;
            for r in &roots {
                assert_eq!(r.root_hash.len(), 32, "subtree root_hash must be 32 bytes");
                assert_eq!(
                    r.completing_block_hash.len(),
                    32,
                    "completing_block_hash must be 32 bytes"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn lightd_info() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let _validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let info = indexer.indexer_info().await?;
            assert!(
                !info.chain_name.is_empty(),
                "lightd_info.chain_name must be set: {info:?}"
            );
            assert!(
                !info.consensus_branch_id.is_empty(),
                "lightd_info.consensus_branch_id must be set: {info:?}"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn get_network_sol_ps() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            env.build().await?;

            let vrpc = validator.json_rpc().await?;
            let irpc = indexer.json_rpc().await?;
            assert_rpc_parity("getnetworksolps", "", &vrpc, &irpc, &[]).await?;
            Ok(())
        }
    }
}
