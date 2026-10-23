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

mod chain_query_interface {
    use super::*;

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
}
