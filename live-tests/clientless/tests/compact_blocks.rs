//! `GetBlockRange` end to end: validator JSON-RPC → NFS → compact-block index → gRPC
//!
//! - Shipped depth (1000): every regtest block non-final (served off snapshot layers)
//! - Heights + compact-block shape only (tree state, transactions, addresses: other files)

use std::time::Duration;

use anyhow::{Context, Result};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);

/// Contiguous from 1 to the mined tip, each block hashed
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn serves_compact_block_range() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);

    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer =
        env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest().finalised_depth(1000));
    env.build().await?;

    let tip = validator.generate_blocks(5).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let blocks = indexer.get_block_range(BlockHeight::from(1u32), tip).await?;
    assert!(!blocks.is_empty(), "no compact blocks served from the index");
    for (offset, block) in blocks.iter().enumerate() {
        let expected = 1 + u64::try_from(offset).expect("small range fits u64");
        assert_eq!(block.height, expected, "served heights must be contiguous");
        assert!(!block.hash.is_empty(), "served block must carry its hash");
    }
    let last = blocks.last().context("range is non-empty")?;
    assert_eq!(last.height, u64::from(tip), "the served range must reach the mined tip");

    Ok(())
}
