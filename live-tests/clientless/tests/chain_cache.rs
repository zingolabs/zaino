use std::time::Duration;

use anyhow::{Context, Result};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(90);
/// Repeats past the first stream (a stale-tip stream shows up on a later subscribe)
const RESUBSCRIPTIONS: u32 = 5;
/// Wallet-like gap between one stream's end and the next subscribe
const RESUBSCRIBE_DELAY: Duration = Duration::from_millis(500);
/// One block mined + served, many times over (past it = stream outlived the tip)
const STREAM_END: Duration = Duration::from_secs(20);
/// Mined → served ceiling per block (under `STREAM_END`: a slow serve fails here, not as a hang)
const SERVED_WITHIN: Duration = Duration::from_secs(15);

mod chain_query_interface {
    use super::*;

    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn each_mined_block_is_served_promptly_and_ends_a_fresh_mempool_stream() -> Result<()> {
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
                let mined_at = std::time::Instant::now();
                indexer.wait_for_block_num(tip, READY).await.map(|()| (tip, mined_at.elapsed()))
            };
            let joined = tokio::time::timeout(STREAM_END, async { tokio::join!(stream, mine) });
            let (stream, mined) = joined.await.with_context(|| {
                format!("subscription {subscription}: stream open {STREAM_END:?} past a block")
            })?;
            let (mined, served_after) =
                mined.with_context(|| format!("subscription {subscription}: mine 1"))?;
            assert!(
                served_after < SERVED_WITHIN,
                "subscription {subscription}: block {mined:?} served {served_after:?} after \
                 mining, ceiling {SERVED_WITHIN:?}"
            );
            stream.with_context(|| {
                format!("subscription {subscription}: stream ended in an error at tip {mined:?}")
            })?;
        }
        Ok(())
    }
}
