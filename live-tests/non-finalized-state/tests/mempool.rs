//! `GetMempoolStream` ends when a reorg moves the tip, including one that keeps its height
//!
//! - Stream = snapshot + arrivals until the tip moves (wallets re-subscribe per block)
//! - Contract C14 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::Result;
use rstest::rstest;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
/// Open-stream check: longer than a chainview poll (1 s) many times over
const HELD: Duration = Duration::from_secs(10);
/// Chainview polls every 1 s; generous for a loaded cluster
const CLOSE: Duration = Duration::from_secs(30);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;

#[rstest]
#[case::tip_swap_at_the_same_height(1, 1)]
#[case::retreat(2, 0)]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn mempool_stream_ends_when_a_reorg_moves_the_tip(
    #[case] orphaned: u32,
    #[case] replacement: u32,
) -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    indexer.wait_for_tip(validator.tip().await?, CONVERGE).await?;

    let stream = {
        let indexer = indexer.clone();
        tokio::spawn(async move { indexer.get_mempool_stream().await })
    };
    tokio::time::sleep(HELD).await;
    assert!(!stream.is_finished(), "stream held open while the tip stands still");

    let reorg = validator.reorg(orphaned, replacement, Some(FILLER_ADDRESS)).await?;
    let ended = tokio::time::timeout(CLOSE, stream).await;
    let tip = u32::from(reorg.tip.0);
    let ended =
        ended.map_err(|_| anyhow::anyhow!("stream still open {CLOSE:?} after tip → {tip}"))?;
    let drained = ended?;
    assert!(drained.is_ok(), "stream ended cleanly, not with an error: {drained:?}");
    Ok(())
}
