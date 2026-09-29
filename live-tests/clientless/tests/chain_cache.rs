use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

mod chain_query_interface {
    use super::*;

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let tip_u32 = u32::from(validator.chain_height().await?);
        let range =
            indexer.get_block_range(BlockHeight::from(1u32), BlockHeight::from(tip_u32)).await?;
        assert_eq!(range.len(), tip_u32 as usize, "every block over [1, tip]");
        for (offset, block) in range.iter().enumerate() {
            assert_eq!(block.height, (offset + 1) as u64, "contiguous from height 1");
            assert_eq!(block.hash.len(), 32, "block hash width");
        }
        Ok(())
    }

    fn validator_subtrees(reply: &Value) -> Result<Vec<(Vec<u8>, u64)>> {
        reply
            .get("subtrees")
            .and_then(Value::as_array)
            .context("z_getsubtreesbyindex reply missing `subtrees` array")?
            .iter()
            .map(|subtree| {
                let root_hex = subtree
                    .get("root")
                    .and_then(Value::as_str)
                    .context("subtree root from validator is not a string")?;
                let bytes = zaino_testutils::hex::decode(root_hex, "z_getsubtreesbyindex root")?;
                let end_height = subtree
                    .get("end_height")
                    .and_then(Value::as_u64)
                    .context("subtree missing end_height")?;
                Ok((bytes, end_height))
            })
            .collect()
    }

    fn indexer_subtrees(roots: &[SubtreeRoot]) -> Vec<(Vec<u8>, u64)> {
        roots.iter().map(|r| (r.root_hash.clone(), r.completing_block_height)).collect()
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let vrpc = validator.json_rpc().await?;

        let test_pools =
            [("sapling", ShieldedProtocol::Sapling), ("orchard", ShieldedProtocol::Orchard)];
        let valid_start_index: u32 = 0;
        let max_entries: u32 = 0;

        // *** Test valid requests ***
        for (pool_string, protocol) in test_pools {
            let indexer_roots =
                indexer.get_subtree_roots(valid_start_index, protocol, max_entries).await?;
            let validator_reply = vrpc
                .call_value(
                    "z_getsubtreesbyindex",
                    json!([pool_string, valid_start_index, max_entries]),
                )
                .await?;
            let expected = validator_subtrees(&validator_reply)?;
            assert_eq!(indexer_subtrees(&indexer_roots), expected, "pool {pool_string}");
        }

        // *** Test invalid requests ***
        let invalid_start_index: u32 = 10000;
        let (orchard_string, orchard_protocol) = test_pools[1];
        let indexer_roots =
            indexer.get_subtree_roots(invalid_start_index, orchard_protocol, max_entries).await?;
        let validator_reply = vrpc
            .call_value(
                "z_getsubtreesbyindex",
                json!([orchard_string, invalid_start_index, max_entries]),
            )
            .await?;
        let expected = validator_subtrees(&validator_reply)?;
        assert_eq!(indexer_subtrees(&indexer_roots), expected, "invalid start index");
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        for iteration in 0..5 {
            tokio::time::sleep(Duration::from_millis(500)).await;

            let stream = indexer.get_mempool_stream();
            let mine = async {
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await
            };

            let joined =
                tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(stream, mine) })
                    .await;
            let (stream_result, mine_result) = joined
                .unwrap_or_else(|_| panic!("stream open past a tip change, iteration {iteration}"));
            stream_result.with_context(|| {
                format!("mempool stream yielded unexpected error on iteration {iteration}")
            })?;
            mine_result?;
        }
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn zallet_like_steady_state_loop() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let mut prev_tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(prev_tip, READY).await?;

        for iteration in 0..5 {
            let current_tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(current_tip, READY).await?;

            let latest = indexer.latest_block_height().await?;
            assert_eq!(latest, current_tip, "tracks the tip, iteration {iteration}");

            let prev = u32::from(prev_tip);
            let current = u32::from(current_tip);
            let applied = indexer
                .get_block_range(BlockHeight::from(prev + 1), BlockHeight::from(current))
                .await?;
            let mined = (current - prev) as usize;
            assert_eq!(applied.len(), mined, "new blocks, iteration {iteration}");
            for (offset, block) in applied.iter().enumerate() {
                let expected = (prev + 1 + offset as u32) as u64;
                assert_eq!(block.height, expected, "contiguous, iteration {iteration}");
            }

            prev_tip = current_tip;
        }
        Ok(())
    }
}
