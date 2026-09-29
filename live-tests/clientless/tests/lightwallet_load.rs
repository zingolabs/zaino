//! Light-wallet load harness on regtest: every stage kind once, every answer held to zebra.
//!
//! - Proves the harness (raw client, sessions, ledger, auditor) against a real zainod + zebrad
//!   in minutes, before a mainnet run spends days building the index it loads
//! - Coinbase pool = the axis: each pool's compact fields (and their byte orders) held to
//!   zebra's JSON

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rstest::rstest;
use ztest::backends::zainod::ServeCaps;
use ztest::loadtest::load::{self, LoadRun, Plan, PodCgroup, Target};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);

/// Past the smoke plan's restore span (40) + reorg margin, with room to spread birthdays
const BLOCKS: u32 = 120;

/// One driver pod = one client address to zainod
const CAPS: ServeCaps = ServeCaps {
    max_connections: NonZeroU32::new(1_024).expect("non-zero"),
    max_connections_per_ip: NonZeroU32::new(1_024).expect("non-zero"),
    max_streams: NonZeroU32::new(2_048).expect("non-zero"),
    max_subscriptions: NonZeroU32::new(1_024).expect("non-zero"),
};

#[rstest]
#[case::transparent(Pool::Transparent)]
#[case::sapling(Pool::Sapling)]
#[case::orchard(Pool::Orchard)]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn every_answer_under_load_holds_to_zebra(#[case] pool: Pool) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(pool));
    let indexer =
        env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest().serve_caps(CAPS));
    env.build().await?;
    let tip = validator.generate_blocks(BLOCKS).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let target = Target {
        uri: indexer.grpc_uri().await?,
        zebra: validator.json_rpc().await?,
        server: Arc::new(PodCgroup::new(indexer.pod().await?)),
        chain_name: "regtest".to_owned(),
    };
    let plan = Plan::smoke();
    let report = load::run(target, plan.clone(), Arc::new(LoadRun::default())).await?;
    println!("{report}");

    assert_eq!(report.ledger.violations, 0, "{report}");
    assert!(report.violations.is_empty(), "{report}");
    let calibrated = report.calibrated.len() as u64;
    assert!(calibrated > 0, "{report}");
    assert_eq!(report.calibration.audited, 2 * calibrated, "both shapes, every height: {report}");
    assert_eq!(report.ledger.dropped, 0, "smoke = every answer audited: {report}");
    assert!(report.ledger.audited > report.calibration.audited, "load answers audited: {report}");

    assert_eq!(report.stages.len(), plan.stages.len());
    for stage in &report.stages {
        let offered = match stage.load {
            load::Load::Steady { wallets, .. } => wallets,
            load::Load::Restore { sessions, .. } => sessions,
            load::Load::Mixed { wallets, sessions, .. } => wallets + sessions,
        };
        assert_eq!(stage.connected, offered, "every session held its connection: {stage}");
        assert_eq!(stage.failures(), 0, "{stage}");
        assert!(stage.requests_per_second() > 0.0, "{stage}");
        assert!(stage.server_cores.is_some(), "zainod's cgroup read: {stage}");
    }
    Ok(())
}
