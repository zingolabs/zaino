//! Differential parity under load: the `fetch` and `state` zaino backends serve
//! the *same* validator, and every `GetBlockRange` is issued to both in the same
//! task and diffed field-by-field. Migrated from `hhanh00/zaino`'s
//! `zaino-admin compare`, but reproducible (a deterministic regtest chain, not a
//! public server) and gating.
//!
//! Hard gates: [`LoadReport::assert_parity`] (A ≡ B, field-identical — this is
//! what catches indexer-correctness drift like a StateService/fetch desync, with
//! an exact `height / field` fingerprint) and [`LoadReport::assert_correct`]
//! (chain-link holds, no errors). The relative-perf bound is deliberately
//! generous: the `state` backend reads Zebra's RocksDB directly while `fetch`
//! goes over JSON-RPC, so their latencies legitimately differ; the ratio gate
//! only catches a catastrophic regression until on-cluster baselines are
//! collected.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn fetch_and_state_agree_under_load() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(Validator::zebrad("6.2.0").regtest().mount(&vol));
    let fetch = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::Fetch)
            .named("zaino-fetch"),
    );
    let state = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::State)
            .mount(&vol)
            .named("zaino-state"),
    );
    env.build().await?;

    let tip = validator.generate_blocks(120).await?;
    fetch.wait_for_block_num(tip, READY).await?;
    state.wait_for_block_num(tip, READY).await?;
    let tip = u64::from(u32::from(tip));

    let report = DiffLoadDriver::pair(fetch.grpc_client().await?, state.grpc_client().await?)
        .label("fetch-vs-state")
        .connections(16)
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
    report.assert_parity()?;
    report.assert_correct()?;
    report.assert_relative(Rel {
        p99_ratio_max: 5.0,
        throughput_ratio_min: 0.1,
    })?;
    Ok(())
}
