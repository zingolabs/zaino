use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);
/// `max_entries = 0` = no limit (lightwalletd protocol)
const ALL_ENTRIES: u32 = 0;
/// Repeats past the first stream (a stale-tip stream shows up on a later subscribe)
const RESUBSCRIPTIONS: u32 = 5;
/// Wallet-like gap between one stream's end and the next subscribe
const RESUBSCRIBE_DELAY: Duration = Duration::from_millis(500);
/// One block mined + served, many times over (past it = stream outlived the tip)
const STREAM_END: Duration = Duration::from_secs(20);
const FOLLOWED_BLOCKS: u32 = 5;

mod chain_query_interface {
    use super::*;

    /// `(root hex, end height)` per subtree in a `z_getsubtreesbyindex` reply
    fn validator_subtrees(reply: &Value) -> Result<Vec<(String, u64)>> {
        reply
            .get("subtrees")
            .and_then(Value::as_array)
            .with_context(|| format!("z_getsubtreesbyindex reply missing `subtrees`: {reply}"))?
            .iter()
            .map(|subtree| {
                let root = subtree
                    .get("root")
                    .and_then(Value::as_str)
                    .with_context(|| format!("subtree root not a string: {subtree}"))?;
                let root = hex::decode(root).with_context(|| format!("subtree root `{root}`"))?;
                let end_height = subtree
                    .get("end_height")
                    .and_then(Value::as_u64)
                    .with_context(|| format!("subtree missing end_height: {subtree}"))?;
                Ok((hex::encode(root), end_height))
            })
            .collect()
    }

    struct SubtreeQuery {
        pool: &'static str,
        protocol: ShieldedProtocol,
        start: u32,
    }

    /// Subtree = 2^16 notes, never reached on regtest → zebrad's answer = `[]` (premise asserted)
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn no_subtree_roots_served_before_any_subtree_completes() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let vrpc = validator.json_rpc().await?;

        #[rustfmt::skip]
        let queries = [
            SubtreeQuery { pool: "sapling", protocol: ShieldedProtocol::Sapling, start: 0 },
            SubtreeQuery { pool: "orchard", protocol: ShieldedProtocol::Orchard, start: 0 },
            SubtreeQuery { pool: "orchard", protocol: ShieldedProtocol::Orchard, start: 10_000 },
        ];
        for SubtreeQuery { pool, protocol, start } in queries {
            let reply =
                vrpc.call_value("z_getsubtreesbyindex", json!([pool, start, ALL_ENTRIES])).await?;
            let zebrad = validator_subtrees(&reply)?;
            let tip = u32::from(tip);
            assert!(zebrad.is_empty(), "premise: zebrad completes no {pool} subtree by {tip}");

            let zaino: Vec<(String, u64)> = indexer
                .get_subtree_roots(start, protocol, ALL_ENTRIES)
                .await
                .with_context(|| format!("GetSubtreeRoots {pool} from {start}"))?
                .iter()
                .map(|root| (hex::encode(&root.root_hash), root.completing_block_height))
                .collect();
            assert_eq!(zaino, zebrad, "{pool} subtrees from {start}: zaino vs zebrad");
        }
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn each_fresh_mempool_stream_ends_cleanly_at_the_next_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        for subscription in 0..RESUBSCRIPTIONS {
            tokio::time::sleep(RESUBSCRIBE_DELAY).await;

            let stream = indexer.get_mempool_stream();
            let mine = async {
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await.map(|()| tip)
            };
            let joined = tokio::time::timeout(STREAM_END, async { tokio::join!(stream, mine) });
            let (stream, mined) = joined.await.with_context(|| {
                format!("subscription {subscription}: stream open {STREAM_END:?} past a block")
            })?;
            let mined = mined.with_context(|| format!("subscription {subscription}: mine 1"))?;
            stream.with_context(|| {
                format!("subscription {subscription}: stream ended in an error at tip {mined:?}")
            })?;
        }
        Ok(())
    }

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn served_tip_and_block_range_follow_each_mined_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(Validator::zebrad("6.2.3").regtest());
        let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
        env.build().await?;

        let mut prev_tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(prev_tip, READY).await?;

        for _ in 0..FOLLOWED_BLOCKS {
            let current_tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(current_tip, READY).await?;

            let latest = indexer.latest_block_height().await?;
            assert_eq!(latest, current_tip, "served tip: zaino vs zebrad, mined onto {prev_tip:?}");

            let new = u32::from(prev_tip) + 1..=u32::from(current_tip);
            let applied = indexer
                .get_block_range(BlockHeight::from(*new.start()), BlockHeight::from(*new.end()))
                .await
                .with_context(|| format!("GetBlockRange {new:?}"))?;
            let served: Vec<u64> = applied.iter().map(|block| block.height).collect();
            let expected: Vec<u64> = new.clone().map(u64::from).collect();
            assert_eq!(served, expected, "heights served for the new blocks {new:?}");

            prev_tip = current_tip;
        }
        Ok(())
    }
}
