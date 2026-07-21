use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use zaino_testutils::{assert_rpc_parity, ShieldedProtocol};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

async fn two_pods() -> Result<(TestEnv, ZebraValidator, ZainoIndexer, ZainoIndexer)> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(
        Validator::zebrad("6.2.0")
            .regtest()
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
    env.build().await?;
    Ok((env, validator, fetch, state))
}

async fn mine_and_sync_both(
    validator: &ZebraValidator,
    fetch: &ZainoIndexer,
    state: &ZainoIndexer,
    n: u32,
) -> Result<BlockHeight> {
    let tip = validator.generate_blocks(n).await?;
    for idx in [fetch, state] {
        while idx.latest_block_height().await? != tip {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(tip)
}

mod zebra {

    use super::*;

    pub(crate) mod check_info {

        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn regtest_no_cache() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 1).await?;

            let frpc = fetch.json_rpc().await?;
            let srpc = state.json_rpc().await?;
            assert_rpc_parity("getinfo", "", &frpc, &srpc, &["errorstimestamp"]).await?;
            assert_rpc_parity(
                "getblockchaininfo",
                "",
                &frpc,
                &srpc,
                &[
                    "valuePools",
                    "chainSupply",
                    "verificationprogress",
                    "size_on_disk",
                ],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn state_service_chaintip_update_subscriber() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            for _ in 0..5 {
                let tip = mine_and_sync_both(&validator, &fetch, &state, 1).await?;
                let fetch_hash = fetch.get_block(tip).await?.hash;
                let state_hash = state.get_block(tip).await?.hash;
                assert_eq!(
                    fetch_hash, state_hash,
                    "state tip hash must match fetch at {tip}"
                );
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zcashd::check_info::regtest_no_cache."]
        async fn regtest_with_cache() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 1).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn testnet() -> Result<()> {
            unimplemented!("testnet check_info parity — requires synced testnet zebrad")
        }
    }

    pub(crate) mod get {

        use super::*;

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_utxos_testnet() -> Result<()> {
            // dev: z_get_address_utxos for tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i,
            // fetch/state agreement on cached testnet chain.
            unimplemented!("testnet z_get_address_utxos parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_tx_ids_testnet() -> Result<()> {
            // dev: get_address_tx_ids for tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i over
            // heights [2_000_000, 3_000_000], fetch/state agreement.
            unimplemented!("testnet get_address_tx_ids parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn raw_transaction_testnet() -> Result<()> {
            // dev: get_raw_transaction(txid, None) for
            // abb0399df392130baa45644c421fab553670a2d0d399c4dd776a8f7862ec289d.
            unimplemented!("testnet get_raw_transaction parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn best_blockhash() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_rpc_parity(
                "getbestblockhash",
                "",
                &fetch.json_rpc().await?,
                &state.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_count() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_rpc_parity(
                "getblockcount",
                "",
                &fetch.json_rpc().await?,
                &state.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn mining_info() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            let frpc = fetch.json_rpc().await?;
            let srpc = state.json_rpc().await?;
            let ignore = &["networksolps", "errorstimestamp"];
            assert_rpc_parity("getmininginfo", "", &frpc, &srpc, ignore).await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_rpc_parity("getmininginfo", "", &frpc, &srpc, ignore).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn difficulty() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            let frpc = fetch.json_rpc().await?;
            let srpc = state.json_rpc().await?;
            assert_rpc_parity("getdifficulty", "", &frpc, &srpc, &[]).await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_rpc_parity("getdifficulty", "", &frpc, &srpc, &[]).await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_network_sol_ps() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_rpc_parity(
                "getnetworksolps",
                "",
                &fetch.json_rpc().await?,
                &state.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn peer_info() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_rpc_parity(
                "getpeerinfo",
                "",
                &fetch.json_rpc().await?,
                &state.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn raw_mempool_testnet() -> Result<()> {
            // dev: get_raw_mempool, sorted, fetch/state agreement.
            unimplemented!("testnet get_raw_mempool parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_object_regtest() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 1).await?;
            let frpc = fetch.json_rpc().await?;
            let srpc = state.json_rpc().await?;
            let block = assert_rpc_parity("getblock", r#"["1", 1]"#, &frpc, &srpc, &[]).await?;
            let hash = block
                .get("hash")
                .and_then(Value::as_str)
                .context("getblock.hash")?;
            let by_hash = srpc.call_value("getblock", json!([hash, 1])).await?;
            assert_eq!(
                by_hash, block,
                "state getblock by-hash must equal by-height"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_object_testnet() -> Result<()> {
            // dev: z_get_block("1000000", verbosity 1), by-height vs by-hash,
            // fetch/state agreement.
            unimplemented!("testnet getblock(object) parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_raw_regtest() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 1).await?;
            assert_rpc_parity(
                "getblock",
                r#"["1", 0]"#,
                &fetch.json_rpc().await?,
                &state.json_rpc().await?,
                &[],
            )
            .await?;
            Ok(())
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_raw_testnet() -> Result<()> {
            // dev: z_get_block("1000000", verbosity 0), fetch/state agreement.
            unimplemented!("testnet getblock(raw) parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_balance_testnet() -> Result<()> {
            // dev: z_get_address_balance for tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i,
            // fetch/state agreement.
            unimplemented!("testnet z_get_address_balance parity — requires synced testnet zebrad")
        }

        #[ztest::qos::integration]
        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn address_deltas_testnet() -> Result<()> {
            // dev: get_address_deltas for tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i over
            // [2_000_000, 3_000_000], both chain_info=false and chain_info=true.
            unimplemented!("testnet get_address_deltas parity — requires synced testnet zebrad")
        }

        mod z {
            use super::*;

            #[ztest::qos::integration]
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            pub(crate) async fn z_validate_address() -> Result<()> {
                let (_env, _validator, _fetch, state) = two_pods().await?;
                clientless::rpc::z_validate_address::run_z_validate_for(
                    &state.json_rpc().await?,
                    clientless::rpc::z_validate_address::SaplingSuite::Standard,
                )
                .await
            }

            #[ztest::qos::integration]
            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn subtrees_by_index_testnet() -> Result<()> {
                // dev: z_get_subtrees_by_index for "sapling" and "orchard",
                // start_index=0, limit=None, fetch/state agreement.
                unimplemented!(
                    "testnet z_get_subtrees_by_index parity — requires synced testnet zebrad"
                )
            }

            #[ztest::qos::integration]
            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn treestate_testnet() -> Result<()> {
                // dev: z_get_treestate("3000000"), fetch/state agreement.
                unimplemented!("testnet z_get_treestate parity — requires synced testnet zebrad")
            }
        }
    }

    pub(crate) mod lightwallet_indexer {
        use super::*;

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_block() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            let tip = mine_and_sync_both(&validator, &fetch, &state, 1).await?;
            assert_eq!(
                fetch.latest_block_height().await?,
                state.latest_block_height().await?,
                "latest block height must agree"
            );
            assert_eq!(
                fetch.get_block(tip).await?,
                state.get_block(tip).await?,
                "tip block must agree"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;

            let by_height_f = fetch.get_block(BlockHeight::from(2u32)).await?;
            let by_height_s = state.get_block(BlockHeight::from(2u32)).await?;
            assert_eq!(by_height_f, by_height_s, "get_block(2) must agree");

            let hash_bytes: [u8; 32] = by_height_f
                .hash
                .clone()
                .try_into()
                .ok()
                .context("block hash must be 32 bytes")?;
            let by_hash_f = fetch.get_block_by_hash(BlockHash(hash_bytes)).await?;
            let by_hash_s = state.get_block_by_hash(BlockHash(hash_bytes)).await?;
            assert_eq!(by_hash_f, by_hash_s, "get_block by-hash must agree");
            assert_eq!(by_hash_f, by_height_f, "by-hash must equal by-height");
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            let frpc = fetch.json_rpc().await?;
            let srpc = state.json_rpc().await?;
            for i in 0u32..10 {
                let tip = mine_and_sync_both(&validator, &fetch, &state, 1).await?;
                let _ = tip;
                let blk = frpc
                    .call_value("getblock", json!([i.to_string(), 1]))
                    .await?;
                let hash = blk
                    .get("hash")
                    .and_then(Value::as_str)
                    .with_context(|| format!("getblock({i},1).hash missing"))?
                    .to_string();
                let params = format!(r#"["{hash}", false]"#);
                assert_rpc_parity("getblockheader", &params, &frpc, &srpc, &[]).await?;
            }
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_tree_state() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            let tip = mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_eq!(
                fetch.get_tree_state(tip).await?,
                state.get_tree_state(tip).await?,
                "tree state at tip must agree"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_subtree_roots() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 5).await?;
            assert_eq!(
                fetch
                    .get_subtree_roots(2, ShieldedProtocol::Sapling, 0)
                    .await?,
                state
                    .get_subtree_roots(2, ShieldedProtocol::Sapling, 0)
                    .await?,
                "sapling subtree roots must agree"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_tree_state() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 2).await?;
            assert_eq!(
                fetch.get_latest_tree_state().await?,
                state.get_latest_tree_state().await?,
                "latest tree state must agree"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_full() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 6).await?;
            // Dev used all_pools_i32() = [Transparent, Sapling, Orchard, Ironwood].
            let all_pools = vec![1, 2, 3, 4];
            assert_eq!(
                fetch
                    .get_block_range_with_pools(
                        BlockHeight::from(2u32),
                        BlockHeight::from(5u32),
                        all_pools.clone(),
                    )
                    .await?,
                state
                    .get_block_range_with_pools(
                        BlockHeight::from(2u32),
                        BlockHeight::from(5u32),
                        all_pools,
                    )
                    .await?,
                "block range [2,5] must agree"
            );
            Ok(())
        }

        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_nullifiers() -> Result<()> {
            let (_env, validator, fetch, state) = two_pods().await?;
            mine_and_sync_both(&validator, &fetch, &state, 6).await?;
            assert_eq!(
                fetch
                    .get_block_range_nullifiers(BlockHeight::from(2u32), BlockHeight::from(5u32))
                    .await?,
                state
                    .get_block_range_nullifiers(BlockHeight::from(2u32), BlockHeight::from(5u32))
                    .await?,
                "block range nullifiers [2,5] must agree"
            );
            Ok(())
        }
    }
}
