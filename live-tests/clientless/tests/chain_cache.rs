//! Chain-index engine tests.
//!
//! Migration note: dev drove Zaino's **in-process** `NodeBackedChainIndex` /
//! `NodeBackedChainIndexSubscriber` library API directly. Under ztest Zaino
//! runs in a pod and is only reachable over its lightwallet gRPC / JSON-RPC
//! surface. The tests split into two groups:
//!
//! * Behaviour that is *observable* over the pod boundary — block-range reads,
//!   subtree roots, large-chain range reads, and the mempool-stream
//!   fresh-snapshot loop — is ported 1:1 as real ztest tests, preserving dev's
//!   exact mine counts, ranges, pools, indices and count/contiguity assertions
//!   (re-expressed against the gRPC surface, e.g. `indexer.get_block_range`
//!   yields compact blocks rather than raw block bytes, so "each block
//!   deserializes" becomes "each block has a 32-byte hash at a contiguous
//!   height").
//!
//! * Behaviour that is *only* the in-process `ChainIndex` API
//!   (`snapshot_nonfinalized_state`, `find_fork_point`, the ephemeral
//!   no-persistence-directory filesystem assertion) has no gRPC/JSON-RPC
//!   surface and is therefore NOT reachable from a pod test. Those three tests
//!   are left as documented `#[ignore]` stubs recording dev's exact
//!   assertions/constants so they can be re-homed to an in-process
//!   `packages/zaino-state` unit test later.
//!
//! dev's `#[cfg(feature = "zcashd_support")]` gating and `#[ignore]` attributes
//! are mirrored.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

mod chain_query_interface {
    use super::*;

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_zebrad() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
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

    #[cfg(feature = "zcashd_support")]
    #[ztest::qos::integration]
    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_zcashd() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
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

    /// BLOCKED-STUB. See origin/dev
    /// `live-tests/clientless/tests/chain_cache.rs:296`.
    ///
    /// dev's invariants (in-process `NodeBackedChainIndex` ephemeral mode only —
    /// no gRPC/JSON-RPC surface, so not reachable over the ztest pod boundary):
    /// - ephemeral chain index (`ephemeral: true`), so `db_height == 0` and the
    ///   NFS cache retains blocks only down to `tip - MAX_NFS_DEPTH` (a small
    ///   margin past the seam);
    /// - mine `seam + 50` blocks (past the retention window) so low heights are
    ///   evicted, where `seam = zaino_common::consensus::FAST_TEST_MAX_NONFINALISED_DEPTH`;
    /// - with `start_height = tip - (seam + 20)` (below the NFS floor, served by
    ///   the ephemeral finalised passthrough) and `end_height = tip - seam / 2`
    ///   (non-finalised):
    ///   - `get_indexed_block_by_height(start)` then
    ///     `get_indexed_block_by_hash(that block's hash)` must be the *same*
    ///     block (fetch-by-height == fetch-by-hash);
    ///   - `get_compact_block_stream(start..=end, PoolTypeFilter::includes_all())`
    ///     must yield exactly `end - start + 1` blocks, at contiguous heights
    ///     `start + offset`;
    /// - ephemeral mode must persist nothing: the `chain-index-zaino` DB
    ///   directory must NOT exist on disk after the run.
    ///
    /// Re-home target: an in-process `packages/zaino-state` unit test.
    #[ignore = "in-process ChainIndex API has no pod surface"]
    #[test]
    fn ephemeral_serves_finalised_blocks_zebrad() {}

    #[ztest::qos::integration]
    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_large_chain_zebrad() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
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

    #[cfg(feature = "zcashd_support")]
    #[ztest::qos::integration]
    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_large_chain_zcashd() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
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
        anyhow::ensure!(s.len() % 2 == 0, "hex string has odd length");
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

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots_zebrad() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
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

    #[cfg(feature = "zcashd_support")]
    #[ztest::qos::integration]
    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots_zcashd() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
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

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated_zebrad() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
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

    #[cfg(feature = "zcashd_support")]
    #[ztest::qos::integration]
    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated_zcashd() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zcashd("v6.20.0").regtest());
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

    /// BLOCKED-STUB. See origin/dev
    /// `live-tests/clientless/tests/chain_cache.rs:625`.
    ///
    /// dev's invariants (in-process `NodeBackedChainIndex` `find_fork_point` /
    /// `snapshot_nonfinalized_state` / `best_chaintip` — no gRPC/JSON-RPC
    /// surface, so not reachable over the ztest pod boundary):
    /// - mine 5 blocks; snapshot; `best_chaintip` → `prev_tip`;
    /// - over 5 iterations, emulating zallet's steady state:
    ///   - fresh snapshot; `best_chaintip` → `current_tip`;
    ///   - `find_fork_point(snapshot, prev_tip.hash)` must be `Some`, and
    ///     `fork_point.1 <= current_tip.height`;
    ///   - if `fork_point.1 < current_tip.height`, apply the block range
    ///     `[fork_point.1 + 1, current_tip.height]` and assert
    ///     `applied_blocks.len() == current_tip.height - fork_point.1`;
    ///   - open a mempool stream on the snapshot, mine 1 block, and assert the
    ///     stream closes within 20s after the tip changes;
    ///   - `prev_tip = current_tip`.
    ///
    /// Re-home target: an in-process `packages/zaino-state` unit test.
    #[ignore = "in-process ChainIndex API has no pod surface"]
    #[test]
    fn zallet_like_steady_state_loop_zebrad() {}

    /// BLOCKED-STUB. zcashd variant of [`zallet_like_steady_state_loop_zebrad`].
    /// Same in-process `find_fork_point`/`snapshot_nonfinalized_state` invariants
    /// (origin/dev `live-tests/clientless/tests/chain_cache.rs:625`), gated
    /// `#[cfg(feature = "zcashd_support")]` and `#[ignore]`d on dev.
    ///
    /// Re-home target: an in-process `packages/zaino-state` unit test.
    #[cfg(feature = "zcashd_support")]
    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[test]
    fn zallet_like_steady_state_loop_zcashd() {}
}
