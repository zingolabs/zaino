//! Blocks from N validators: ordered, concurrent, retried
//!
//! - One spawned task per height = fetch + decode (decode spreads across cores); `concurrency`
//!   bounds how many are in flight, `buffered` keeps height order
//! - Per validator: transient failures retried with doubling backoff, then the next validator
//! - Heights rotate over every validator (a lagging node's heights fall to the others)
//!
//! TODO: data validation lives here, before any index sees a block: read the best chain's hash
//! set, re-hash each fetched block (header SHA-256d, txids → merkle root) and refuse a mismatch

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, Stream, StreamExt};
use tracing::warn;
use zaino_primitives::types::{Block, BlockHash, Height};

use crate::{GetBlock, GetBlockByHash, GetBlockByHashError, GetBlockError, QueryError};

const ATTEMPTS_PER_VALIDATOR: u32 = 3;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(250);

pub struct BlockFetchPool<S> {
    sources: Vec<Arc<S>>,
    concurrency: NonZeroUsize,
}

impl<S: GetBlock + GetBlockByHash + 'static> BlockFetchPool<S> {
    pub fn new(sources: Vec<Arc<S>>, concurrency: NonZeroUsize) -> Self {
        assert!(!sources.is_empty(), "block fetch pool with no validator");
        Self { sources, concurrency }
    }

    /// Only the sources at `positions` (e.g. validators agreeing on one tip)
    pub fn among(&self, positions: impl IntoIterator<Item = usize>) -> Self {
        let sources = positions.into_iter().map(|position| Arc::clone(&self.sources[position]));
        Self::new(sources.collect(), self.concurrency)
    }

    /// Blocks `start` to `end`, both inclusive, ascending
    ///
    /// - First error ends the stream (nothing after it is sent)
    pub fn blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Block, QueryError<GetBlockError>>> + Send + 'static {
        assert!(start <= end, "empty fetch range {start:?}..={end:?}");
        let sources = self.sources.clone();
        let mut failed = false;

        stream::iter(start.up_to(end))
            .map(move |height| {
                let candidates = rotated(&sources, u32::from(height) as usize);
                tokio::spawn(first_answer(candidates, height, move |source| async move {
                    let block = source.get_block(height).await?;
                    match block.header().height == height {
                        true => Ok(block),
                        false => Err(misanswered(format!(
                            "asked height {height:?}, got {:?}",
                            block.header().height
                        ))),
                    }
                }))
            })
            .buffered(self.concurrency.get())
            .map(|joined| match joined {
                Ok(fetched) => fetched,
                Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
                Err(join) => panic!("block fetch task cancelled: {join}"),
            })
            .take_while(move |fetched| {
                let open = !failed;
                failed |= fetched.is_err();
                std::future::ready(open)
            })
    }

    /// Every validator in turn (a branch tip may be on only some of them)
    pub async fn block_by_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Block, QueryError<GetBlockByHashError>> {
        first_answer(rotated(&self.sources, 0), hash, |source| async move {
            let block = source.get_block_by_hash(hash).await?;
            match block.header().hash == hash {
                true => Ok(block),
                false => {
                    Err(misanswered(format!("asked block {hash}, got {}", block.header().hash)))
                }
            }
        })
        .await
    }
}

/// A block that is not the one asked for (validator bug: that validator's failure, next one tried)
fn misanswered<E: std::fmt::Debug + std::fmt::Display>(what: String) -> QueryError<E> {
    QueryError::NonDomain(crate::NonDomainError::new(crate::FailureMode::Parse, what))
}

/// Every source, starting at `start`
fn rotated<S>(sources: &[Arc<S>], start: usize) -> Vec<Arc<S>> {
    (0..sources.len())
        .map(|offset| Arc::clone(&sources[(start + offset) % sources.len()]))
        .collect()
}

/// First validator to answer `call`; the last validator's error when none does
async fn first_answer<S, T, E, Fut>(
    candidates: Vec<Arc<S>>,
    what: impl std::fmt::Debug,
    call: impl Fn(Arc<S>) -> Fut,
) -> Result<T, QueryError<E>>
where
    E: std::fmt::Debug + std::fmt::Display,
    Fut: Future<Output = Result<T, QueryError<E>>>,
{
    let last = candidates.len().checked_sub(1).expect("at least one validator");
    for (index, source) in candidates.into_iter().enumerate() {
        let mut delay = FIRST_RETRY_DELAY;
        let mut attempt = 1;
        let error = loop {
            match call(Arc::clone(&source)).await {
                Ok(answer) => return Ok(answer),
                Err(QueryError::NonDomain(error))
                    if error.mode.is_transient() && attempt < ATTEMPTS_PER_VALIDATOR =>
                {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                    attempt += 1;
                }
                Err(error) => break error,
            }
        };
        if index == last {
            return Err(error);
        }
        warn!(?what, %error, "Block fetch failed, trying next validator");
    }
    unreachable!("the last validator returns")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{test_block, MockChain};
    use crate::FailureMode;

    /// Spread over three validators, one lagging at 4: every height arrives in order (the lagging
    /// node's heights fall back to the others); among the lagging node alone, the stream ends at
    /// its first missing height and sends nothing after it
    #[tokio::test]
    async fn spread_falls_back_past_a_lagging_validator_and_a_lone_one_stops_at_its_gap() {
        let chain =
            |tip: u32| {
                Arc::new((0..=tip).fold(MockChain::new(), |chain, h| {
                    chain.with_block(test_block(h, h as u8 + 1))
                }))
            };
        let sources = vec![chain(9), chain(4), chain(9)];
        let concurrency = NonZeroUsize::new(4).expect("nz");
        let height = |h: u32| Height::try_from(h).expect("h");

        let spread = BlockFetchPool::new(sources, concurrency);
        let got: Vec<_> = spread.blocks(height(0), height(9)).collect().await;
        let heights: Vec<_> =
            got.iter().map(|b| b.as_ref().map(|b| u32::from(b.header().height)).ok()).collect();
        assert_eq!(heights, (0..=9).map(Some).collect::<Vec<_>>());

        let lone = spread.among([1]);
        let got: Vec<_> = lone.blocks(height(3), height(9)).collect().await;
        assert_eq!(got.len(), 3, "nothing after the first failure: {got:?}");
        use {GetBlockError::HeightNotFound, QueryError::Domain};
        assert!(matches!(&got[2], Err(Domain(HeightNotFound(h))) if *h == height(5)));
    }

    /// Transient failures are retried on the same validator (two timeouts, then the block); a
    /// permanent one (auth) is not, and with no other validator it is the stream's error
    #[tokio::test(start_paused = true)]
    async fn transient_failures_retry_and_permanent_ones_do_not() {
        let height = Height::try_from(0u32).expect("h");
        let concurrency = NonZeroUsize::new(1).expect("nz");

        let flaky =
            MockChain::new().with_block(test_block(0, 1)).fail_next(2, FailureMode::Timeout);
        let pool = BlockFetchPool::new(vec![Arc::new(flaky)], concurrency);
        let got: Vec<_> = pool.blocks(height, height).collect().await;
        assert!(matches!(&got[..], [Ok(block)] if block.header().height == height), "{got:?}");

        let refused = MockChain::new().with_block(test_block(0, 1)).fail_next(1, FailureMode::Auth);
        let pool = BlockFetchPool::new(vec![Arc::new(refused)], concurrency);
        let got: Vec<_> = pool.blocks(height, height).collect().await;
        let [Err(QueryError::NonDomain(e))] = &got[..] else { panic!("{got:?}") };
        assert_eq!(e.mode, FailureMode::Auth);
    }

    /// `get_block(h)` sleeps `(7 − h) × 5 ms` with every height in flight: completion order is
    /// fully reversed, delivery order still ascending (indexes assert contiguous heights)
    #[tokio::test]
    async fn reversed_completion_still_streams_in_height_order() {
        struct ReverseDelay;
        impl GetBlock for ReverseDelay {
            async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
                let h = u32::from(height);
                tokio::time::sleep(Duration::from_millis(u64::from(7 - h) * 5)).await;
                Ok(test_block(h, h as u8 + 1))
            }
        }
        impl GetBlockByHash for ReverseDelay {
            async fn get_block_by_hash(
                &self,
                hash: BlockHash,
            ) -> Result<Block, QueryError<GetBlockByHashError>> {
                Err(QueryError::Domain(GetBlockByHashError::NotFound(hash)))
            }
        }

        let pool =
            BlockFetchPool::new(vec![Arc::new(ReverseDelay)], NonZeroUsize::new(8).expect("nz"));
        let got: Vec<_> = pool
            .blocks(Height::try_from(0u32).expect("h"), Height::try_from(7u32).expect("h"))
            .map(|b| b.map(|b| u32::from(b.header().height)).ok())
            .collect()
            .await;
        assert_eq!(got, (0..=7).map(Some).collect::<Vec<_>>());
    }
}
