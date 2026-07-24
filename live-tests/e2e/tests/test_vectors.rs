//! Holds code used to build test vector data for unit tests. These tests should not be run by default or in CI.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// The committed vectors encode a ~200-block regtest chain.
const CHAIN_LEN: u32 = 200;

#[ztest::qos::testnet]
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(feature = "devtool-incompatible"),
    ignore = "Not a test: builds test-vector data for zaino_state::chain_index unit tests. ztest gap: dev serialized the vectors via the in-process ReadStateService/state_subscriber, which has no pod surface, so this can only stand up and serve the chain — re-home vector generation to a packages/zaino-state unit test."
)]
async fn create_200_block_regtest_chain_vectors() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(
        Validator::zebrad("6.2.0")
            .regtest()
            .mine_to(Pool::Transparent)
            .mount(&vol),
    );
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::State)
            .mount(&vol),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    Ok(())
}
