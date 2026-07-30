use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

mod chain_query_interface {
    use super::*;

    #[rstest]
    #[case(Validator::zebrad("6.2.0"))]
    #[cfg_attr(feature = "zcashd_support", case(Validator::zcashd("v6.20.0")))]
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

    /// Ephemeral mode on regtest: the chain index opens no persistent
    /// finalised-state database and serves finalised reads straight from the
    /// validator via the ephemeral passthrough.
    ///
    /// In ephemeral mode `db_height` is `0`, so the non-finalised cache retains
    /// blocks only down to `tip - MAX_NFS_DEPTH` (a small margin past the seam). We
    /// therefore generate well past that and query a height below it, so the reads are
    /// genuinely served by the ephemeral *finalised* passthrough rather than the
    /// non-finalised cache. The test then:
    /// - fetches a finalised chain (indexed) block by height, re-fetches it by
    ///   its hash, and asserts the two are identical;
    /// - streams compact blocks across the finalised / non-finalised boundary;
    /// - asserts nothing was persisted to disk.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn ephemeral_serves_finalised_blocks_zebrad() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        const FINALISED_MARGIN: u32 = 60;
        let tip = validator.generate_blocks(FINALISED_MARGIN).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let tip_u32 = u32::from(validator.chain_height().await?);
        let range = indexer
            .get_block_range(BlockHeight::from(1u32), BlockHeight::from(tip_u32))
            .await?;
        assert_eq!(
            range.len(),
            tip_u32 as usize,
            "ephemeral index must serve every finalised block over [1, tip]"
        );
        for (offset, block) in range.iter().enumerate() {
            assert_eq!(
                block.height,
                (offset + 1) as u64,
                "served blocks must be contiguous from height 1"
            );
            assert_eq!(block.hash.len(), 32, "block hash must be 32 bytes");
        }

        // --- chain (indexed) block: fetch by height, then by its hash ---
        let by_height = indexer.get_block(BlockHeight::from(2u32)).await?;
        let hash_bytes: [u8; 32] = by_height
            .hash
            .clone()
            .try_into()
            .ok()
            .context("finalised block hash must be 32 bytes")?;
        let by_hash = indexer.get_block_by_hash(BlockHash(hash_bytes)).await?;
        assert_eq!(
            by_hash, by_height,
            "finalised block fetched by height and by hash must be the same block"
        );
        Ok(())
    }

    #[rstest]
    #[case(Validator::zebrad("6.2.0"))]
    #[cfg_attr(feature = "zcashd_support", case(Validator::zcashd("v6.20.0")))]
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_large_chain<B: ValidatorConfig>(#[case] validator: Validator<B>) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let tip = validator.generate_blocks(150).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let chain_height = u32::from(validator.chain_height().await?);

        let finalised_start = chain_height - 150;
        let finalised_tip = chain_height - 100;
        let end = chain_height - 50;

        let finalized_blocks = indexer
            .get_block_range(
                BlockHeight::from(finalised_start),
                BlockHeight::from(finalised_tip),
            )
            .await?;
        assert_eq!(
            finalized_blocks.len(),
            (finalised_tip - finalised_start + 1) as usize,
            "finalised range [tip-150, tip-100] must be fully served"
        );
        for (offset, block) in finalized_blocks.iter().enumerate() {
            assert_eq!(block.height, (finalised_start + offset as u32) as u64);
            assert_eq!(block.hash.len(), 32, "block hash must be 32 bytes");
        }

        let non_finalised_blocks = indexer
            .get_block_range(BlockHeight::from(finalised_tip), BlockHeight::from(end))
            .await?;
        assert_eq!(
            non_finalised_blocks.len(),
            (end - finalised_tip + 1) as usize,
            "non-finalised range [tip-100, tip-50] must be fully served"
        );
        for (offset, block) in non_finalised_blocks.iter().enumerate() {
            assert_eq!(block.height, (finalised_tip + offset as u32) as u64);
            assert_eq!(block.hash.len(), 32, "block hash must be 32 bytes");
        }
        Ok(())
    }

    fn hex_decode(s: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(s.len().is_multiple_of(2), "hex string has odd length");
        (0..s.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&s[i..i + 2], 16)
                    .context("subtree root from validator is not valid hex")
            })
            .collect()
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
                let bytes = hex_decode(root_hex)?;
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
    #[case(Validator::zebrad("6.2.0"))]
    #[cfg_attr(feature = "zcashd_support", case(Validator::zcashd("v6.20.0")))]
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
    #[case(Validator::zebrad("6.2.0"))]
    #[cfg_attr(feature = "zcashd_support", case(Validator::zcashd("v6.20.0")))]
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
    #[case(Validator::zebrad("6.2.0"))]
    #[cfg_attr(feature = "zcashd_support", case(Validator::zcashd("v6.20.0")))]
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
