//! Light-wallet load harness on regtest: both scenarios ramped to their ceiling and soaked, every
//! answer held to zebra.
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
use ztest::loadtest::load::{self, LoadRun, Plan, PodCgroup, Scenario, Target};
use ztest::loadtest::measure::Method;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);

/// Past the reorg margin with room to spread birthdays (each sync then runs birthday → tip)
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
    let report = load::run(target, Plan::smoke(), Arc::new(LoadRun::default())).await?;
    println!("{report}");

    assert_eq!(report.ledger.violations, 0, "{report}");
    assert!(report.violations.is_empty(), "{report}");
    let calibrated = report.calibrated.len() as u64;
    assert!(calibrated > 0, "{report}");
    assert_eq!(report.calibration.audited, 2 * calibrated, "both shapes, every height: {report}");
    assert_eq!(report.ledger.dropped, 0, "smoke = every answer audited: {report}");
    assert!(report.ledger.audited > report.calibration.audited, "load answers audited: {report}");

    let scenarios: Vec<Scenario> = report.scenarios.iter().map(|s| s.ramp.scenario).collect();
    assert_eq!(scenarios, [Scenario::Incremental, Scenario::Fresh], "{report}");
    for scenario in &report.scenarios {
        let ceiling = scenario.ramp.ceiling;
        assert_eq!(scenario.capacity, Some(ceiling), "regtest serves every level: {report}");
        assert_eq!((scenario.stopped, scenario.soak_breach), (None, None), "{report}");
        for level in scenario.levels.iter().chain(&scenario.soak) {
            assert_eq!(
                level.connected, level.wallets,
                "every wallet held its connection: {level:#}"
            );
            assert!(level.server_cores.is_some(), "zainod's cgroup read: {level:#}");
            let ok = |method| level.methods.iter().find(|m| m.method == method).map_or(0, |m| m.ok);
            if level.scenario == Scenario::Fresh {
                assert!(
                    ok(Method::GetTaddressTxids) > 0,
                    "pepper-sync's gap-limit scan: {level:#}"
                );
            }
        }
    }
    Ok(())
}
