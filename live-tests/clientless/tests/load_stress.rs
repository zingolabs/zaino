//! Stress / correctness-under-load: N connections hammer one zaino indexer's
//! `GetBlockRange` while the chain-link invariant is asserted on every streamed
//! block. Migrated from `hhanh00/zaino`'s `zaino-admin concurrent-test` + `check`,
//! reshaped into a single ztest test that fans out internally.
//!
//! The hard gate is correctness ([`LoadReport::assert_correct`]): zero chain-link
//! violations, zero request errors. Latency percentiles are printed for humans
//! but **not** gated — an absolute SLO is only trustworthy on a calibrated
//! cluster (see `ztest/docs/design-load-testing.md`).

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

#[ztest::qos::testnet]
#[tokio::test(flavor = "multi_thread")]
async fn block_range_stays_consistent_under_load() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest());
    let zaino = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::Fetch)
            .named("zaino"),
    );
    env.build().await?;

    let tip = validator.generate_blocks(120).await?;
    zaino.wait_for_block_num(tip, READY).await?;
    let tip = u64::from(u32::from(tip));

    let report = LoadDriver::new(zaino.grpc_client().await?)
        .label("block-range-sweep")
        .connections(32)
        .conn_mode(ConnMode::PerTask)
        .spawn_stagger(Duration::from_millis(1))
        .scenario(Scenario::BlockRangeSweep {
            pool: 1..tip,
            blocks: 40,
            dist: Distribution::Even,
        })
        .oracle(ChainLinkOracle)
        .until(Until::Duration(Duration::from_secs(15)))
        .run()
        .await?;

    report.print();
    report.assert_correct()?;
    Ok(())
}
