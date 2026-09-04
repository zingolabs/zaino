use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);
/// One `/metrics` round trip, not a readiness budget
const SCRAPE: Duration = Duration::from_secs(10);

mod chain_query_interface {
    use super::*;

    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range<B: ValidatorConfig>(#[case] validator: Validator<B>) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let tip_u32 = u32::from(validator.chain_height().await?);
        let range = indexer
            .get_block_range(BlockHeight::from(1u32), BlockHeight::from(tip_u32))
            .await?;
        assert_eq!(
            range.len(),
            tip_u32 as usize,
            "get_block_range must serve every block over [1, tip]"
        );
        for (offset, block) in range.iter().enumerate() {
            assert_eq!(
                block.height,
                (offset + 1) as u64,
                "blocks must be contiguous from height 1"
            );
            assert_eq!(block.hash.len(), 32, "block hash must be 32 bytes");
        }
        Ok(())
    }

    /// Ephemeral mode opens no persistent finalised-state database: every read that
    /// would need one is passed through to the validator instead.
    ///
    /// `db_height` is pinned at `0` in ephemeral mode, so the non-finalised cache
    /// retains only `tip - MAX_NFS_DEPTH` (= seam + 10) upward. The chain is mined
    /// past that so the reads below the floor are genuinely served by the passthrough
    /// and not by the cache — at the operational seam of 1001 the whole chain fits in
    /// the cache and the passthrough is never reached, hence the `fast-test-seam` image.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn ephemeral_serves_finalised_blocks_zebrad() -> Result<()> {
        const CHAIN_LEN: u32 = 160;
        // seam (100) + the 10-block margin MAX_NFS_DEPTH adds
        const CACHE_FLOOR: u32 = 160 - 110;

        let mut env = TestEnv::builder().ready_timeout(Duration::from_secs(180));
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(
            dev!(
                Indexer::Zainod,
                "../../Dockerfile",
                features = [
                    "no_tls_use_unencrypted_traffic",
                    "allow_unencrypted_public_json_rpc_bind",
                    "prometheus",
                    "fast-test-seam",
                ]
            )
            .regtest()
            .tuning(ZainoTuning::Ephemeral),
        );
        env.build().await?;

        let tip = validator.generate_blocks(CHAIN_LEN).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let tip = u32::from(tip);

        // Nothing persisted: `zaino_db_tip_height` is set on every batch commit of the v1
        // write path, which ephemeral mode never spawns, so the family never appears.
        assert_eq!(
            indexer
                .read(SCRAPE)
                .await
                .map_err(anyhow::Error::msg)?
                .height_gauge(family("zaino_db_tip_height")),
            None,
            "ephemeral mode must open no finalised database, but the finalised writer \
             reported a committed tip"
        );

        let range = indexer
            .get_block_range(BlockHeight::from(1u32), BlockHeight::from(tip))
            .await?;
        assert_eq!(
            range.len(),
            tip as usize,
            "ephemeral index must serve every block over [1, {tip}]"
        );
        for (offset, block) in range.iter().enumerate() {
            assert_eq!(
                block.height,
                (offset + 1) as u64,
                "served blocks must be contiguous from height 1"
            );
        }
        for pair in range.windows(2) {
            assert_eq!(
                pair[1].prev_hash, pair[0].hash,
                "block {} does not link to block {} — the passthrough and the cache                  served two different chains",
                pair[1].height, pair[0].height
            );
        }

        let below_floor = BlockHeight::from(CACHE_FLOOR / 2);
        let by_height = indexer.get_block(below_floor).await?;
        let hash_bytes: [u8; 32] = by_height
            .hash
            .clone()
            .try_into()
            .ok()
            .context("served block hash must be 32 bytes")?;
        let by_hash = indexer.get_block_by_hash(BlockHash(hash_bytes)).await?;
        assert_eq!(
            by_hash, by_height,
            "a block below the cache floor must be the same fetched by height and by hash"
        );

        let vrpc = validator.json_rpc().await?;
        let hash = vrpc
            .call_value("getblockhash", json!([by_height.height]))
            .await?;
        let mut served = by_height.hash.clone();
        served.reverse();
        assert_eq!(
            zaino_testutils::hex::encode(&served),
            hash.as_str().context("getblockhash returns a hex string")?,
            "the passthrough served a different block than the validator holds at {}",
            by_height.height
        );
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
        roots
            .iter()
            .map(|r| (r.root_hash.clone(), r.completing_block_height))
            .collect()
    }

    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots<B: ValidatorConfig>(#[case] validator: Validator<B>) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let vrpc = validator.json_rpc().await?;

        let test_pools = [
            ("sapling", ShieldedProtocol::Sapling),
            ("orchard", ShieldedProtocol::Orchard),
        ];
        let valid_start_index: u32 = 0;
        let max_entries: u32 = 0;

        // *** Test valid requests ***
        for (pool_string, protocol) in test_pools {
            let indexer_roots = indexer
                .get_subtree_roots(valid_start_index, protocol, max_entries)
                .await?;
            let validator_reply = vrpc
                .call_value(
                    "z_getsubtreesbyindex",
                    json!([pool_string, valid_start_index, max_entries]),
                )
                .await?;
            assert_eq!(
                indexer_subtrees(&indexer_roots),
                validator_subtrees(&validator_reply)?,
                "chain-index subtree roots must match validator for pool {pool_string}"
            );
        }

        // *** Test invalid requests ***
        let invalid_start_index: u32 = 10000;
        let (orchard_string, orchard_protocol) = test_pools[1];
        let indexer_roots = indexer
            .get_subtree_roots(invalid_start_index, orchard_protocol, max_entries)
            .await?;
        let validator_reply = vrpc
            .call_value(
                "z_getsubtreesbyindex",
                json!([orchard_string, invalid_start_index, max_entries]),
            )
            .await?;
        assert_eq!(
            indexer_subtrees(&indexer_roots),
            validator_subtrees(&validator_reply)?,
            "chain-index subtree roots must match validator at invalid start index"
        );
        Ok(())
    }

    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest());
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

            let joined = tokio::time::timeout(Duration::from_secs(20), async {
                tokio::join!(stream, mine)
            })
            .await;
            let (stream_result, mine_result) = joined.unwrap_or_else(|_| {
                panic!(
                    "mempool stream did not close after chain tip changed on iteration {iteration}"
                )
            });
            stream_result.with_context(|| {
                format!("mempool stream yielded unexpected error on iteration {iteration}")
            })?;
            mine_result?;
        }
        Ok(())
    }

    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn zallet_like_steady_state_loop<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let mut prev_tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(prev_tip, READY).await?;

        for iteration in 0..5 {
            let current_tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(current_tip, READY).await?;

            assert_eq!(
                indexer.latest_block_height().await?,
                current_tip,
                "indexer must track the advancing tip on iteration {iteration}"
            );

            let prev = u32::from(prev_tip);
            let current = u32::from(current_tip);
            let applied = indexer
                .get_block_range(BlockHeight::from(prev + 1), BlockHeight::from(current))
                .await?;
            assert_eq!(
                applied.len(),
                (current - prev) as usize,
                "indexer must serve exactly the newly mined blocks on iteration {iteration}"
            );
            for (offset, block) in applied.iter().enumerate() {
                assert_eq!(
                    block.height,
                    (prev + 1 + offset as u32) as u64,
                    "applied blocks must be contiguous on iteration {iteration}"
                );
            }

            prev_tip = current_tip;
        }
        Ok(())
    }
}
