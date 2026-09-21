//! Compact-block serving against the new runtime stack, sourced over JSON-RPC.
//!
//! The new zainod boots the runtime Orchestra (validator → indexer → store →
//! lightserve gRPC) and serves the compact-block triad from its own index. This
//! walk proves that path end to end over the wire: the indexer builds the
//! current-zaino index from the validator's JSON-RPC, and `GetBlockRange` serves
//! the composed compact blocks the harness streams back.
//!
//! It boots zainod through the **`ztest-fixture`** bridge: ztest mounts a
//! legacy-schema `zainod.toml` the greenfield `DaemonConfig` can't parse, so the
//! image is built `--features ztest-fixture` and the env var makes zainod ignore
//! the mounted `--config` and boot an in-process regtest config, pointed at the
//! validator's in-cluster JSON-RPC via `ZAINO_TEST_ZEBRA_JSONRPC`.
//!
//! The fixture indexes to the tip (`finalised_depth = 0`), so served blocks come
//! from the finalised store (FS) and the chain-head (NFS) over the one source.
//!
//! Scope is the served slice only — heights and compact-block shape. Treestate,
//! transactions, address queries and per-tx ironwood actions are out of the
//! index-only serving path and are not asserted here.

use std::time::Duration;

use anyhow::{Context, Result};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);

/// zainod serves a contiguous compact-block range, sourced over JSON-RPC, up to
/// the mined tip.
///
/// multi_thread required: the harness spawns the validator and indexer services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn serves_compact_block_range() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);

    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = ["ztest-fixture"]
        )
        .regtest()
        .env("ZAINO_TEST_REGTEST_FIXTURE", "1")
        // The validator pod's DNS name (default "zebrad") on the regtest RPC
        // port; localhost would be this pod, not the validator.
        .env(
            "ZAINO_TEST_ZEBRA_JSONRPC",
            format!("zebrad:{}", ztest::ports::ZEBRAD_RPC),
        ),
    );
    env.build().await?;

    let tip = validator.generate_blocks(5).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(
        !blocks.is_empty(),
        "no compact blocks served from the index"
    );
    for (offset, block) in blocks.iter().enumerate() {
        let expected = 1 + u64::try_from(offset).expect("small range fits u64");
        assert_eq!(block.height, expected, "served heights must be contiguous");
        assert!(!block.hash.is_empty(), "served block must carry its hash");
    }
    let last = blocks.last().context("range is non-empty")?;
    assert_eq!(
        last.height,
        u64::from(tip),
        "the served range must reach the mined tip"
    );

    Ok(())
}
