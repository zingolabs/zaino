//! Stalled across a reorg that then ran past the window: realign first, never finalize an orphan
//!
//! - Frozen (SIGSTOP, same process on thaw) → the next tip = another branch, > `DEPTH` ahead
//! - Contract C18 in `docs/design/non-finalized-state-tests.md`

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use ztest::backends::zainod::family;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);
const CONVERGE: Duration = Duration::from_secs(180);
const SCRAPE: Duration = Duration::from_secs(10);
const DEPTH: u32 = 10;
const CHAIN: u32 = 3 * DEPTH;
const ORPHANED: u32 = 4;
/// Past the window from zaino's frozen tip (`CHAIN`)
const REPLACEMENT: u32 = ORPHANED + DEPTH + 3;

#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn stalled_through_a_reorg_past_the_window_realigns_without_finalizing_orphans() -> Result<()>
{
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Orchard));
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile", features = ["prometheus"])
            .regtest()
            .finalised_depth(DEPTH)
            .restartable(),
    );
    env.build().await?;

    let (warm, _) = validator.tip().await?;
    validator.generate_blocks(CHAIN - u32::from(warm)).await?;
    let old_tip = validator.tip().await?;
    indexer.wait_for_tip(old_tip, CONVERGE).await?;

    let pod = indexer.pod().await?;
    let before = pod.sample().await?;
    pod.freeze().await?;
    let reorg = validator.reorg(ORPHANED, REPLACEMENT, Some(FILLER_ADDRESS)).await?;
    pod.thaw().await?;
    let tip = u32::from(reorg.tip.0);
    assert!(tip - u32::from(old_tip.0) > DEPTH, "fixture: new tip {tip} past the frozen window");
    indexer.wait_for_tip(reorg.tip, CONVERGE).await?;

    assert_eq!(pod.sample().await?.restarts, before.restarts, "followed live, no crash-restart");
    let vrpc = validator.json_rpc().await?;
    let served = indexer.get_block_range(1u32.into(), reorg.tip.0).await?;
    assert_eq!(served.len(), tip as usize, "one block per height in [1, {tip}]");
    for (block, height) in served.iter().zip(1u32..) {
        let truth = vrpc.call_value("getblockhash", json!([height])).await?;
        let hash = BlockHash::from_wire(&block.hash).context("served hash is 32 bytes")?;
        assert_eq!(Some(hash.to_string().as_str()), truth.as_str(), "block at {height}");
    }
    for (height, hash) in &reorg.orphaned {
        let code = indexer.get_block_by_hash(*hash).await.err().and_then(|e| e.grpc_code());
        assert_eq!(code, Some(tonic::Code::NotFound), "orphan {hash} at {height} never served");
    }
    for index in ZainoIndex::ALL {
        let durable = indexer.finalized_height(index).await?;
        assert_eq!(durable, Some(tip - DEPTH), "{index:?} durable = tip − DEPTH, on the winner");
        assert_eq!(indexer.synced(index).await?, Some(true), "{index:?} serving");
    }
    let scrape = indexer.read(SCRAPE).await?;
    assert_eq!(scrape.counter_total(family::REORGS), Some(1), "realign counted as one rollback");
    assert_eq!(scrape.height(family::BEST_TIP), Some(tip), "zaino_best_tip");
    Ok(())
}
