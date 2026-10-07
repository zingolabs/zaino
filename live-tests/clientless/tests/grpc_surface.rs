//! Smoke over zaino's block, tree-state and info gRPC RPCs against a regtest zebrad

use std::time::Duration;

use anyhow::{Context, Result};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(60);

/// Blocks each `get` test mines → a two-block chain (`TestEnv::build` mines the warm-up block)
const BASELINE: u32 = 1;

mod launch {
    use super::*;

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn regtest_no_cache() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let _validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let info = indexer.indexer_info().await?;
        assert!(!info.chain_name.is_empty(), "indexer chain_name must be set: {info:?}");
        Ok(())
    }
}

mod get {
    use super::*;

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn latest_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let indexer_tip = indexer.latest_block_height().await?;
        let validator_tip = validator.chain_height().await?;
        assert_eq!(indexer_tip, validator_tip, "indexer tip = validator tip");
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let by_height = indexer.get_block(BlockHeight::from(1u32)).await?;
        assert_eq!(by_height.height, 1, "get_block(1).height");
        let hash = BlockHash::from_wire(&by_height.hash).context("block hash must be 32 bytes")?;
        let (_, truth) = validator.get_block(BlockHeight::from(1u32)).await?;
        assert_eq!(hash, truth, "served hash, display order = validator's");
        let by_hash = indexer.get_block_by_hash(hash).await?;
        assert_eq!(by_height.height, by_hash.height, "by-hash height round-trip");
        assert_eq!(by_height.hash, by_hash.hash, "by-hash hash round-trip");
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_range() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 10).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let _blocks =
            indexer.get_block_range(BlockHeight::from(1u32), BlockHeight::from(10u32)).await?;
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn block_range_nullifiers() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 10).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let _blocks = indexer
            .get_block_range_nullifiers(BlockHeight::from(1u32), BlockHeight::from(10u32))
            .await?;
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn tree_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let chain_tip = validator.chain_height().await?;
        let _ts = indexer.get_tree_state(chain_tip).await?;
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn latest_tree_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let _ts = indexer.get_latest_tree_state().await?;
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn subtree_roots() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE + 1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let _roots = indexer.get_subtree_roots(0, ShieldedProtocol::Sapling, 0).await?;
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    pub(crate) async fn lightd_info() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(BASELINE).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let _info = indexer.indexer_info().await?;
        Ok(())
    }
}
