//! Blocks from N validators: ordered, concurrent, retried
//!
//! - One spawned task per height = fetch + decode (decode spreads across cores); `concurrency`
//!   bounds how many are in flight, `buffered` keeps height order
//! - Per validator: transient failures retried with doubling backoff, then the next validator
//! - Each height to the least-loaded validator ([`TrafficBalancer`]), the rest its failover (a lagging
//!   node's heights fall to the others)
//!
//! TODO: data validation lives here, before any index sees a block: read the best chain's hash
//! set, re-hash each fetched block (header SHA-256d, txids → merkle root) and refuse a mismatch

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::stream::{self, Stream, StreamExt};
use zaino_primitives::types::{Block, BlockHash, Height};

use crate::balance::TrafficBalancer;
use crate::{ChainDataSource, GetBlockByHashError, GetBlockError, QueryError};

pub struct BlockFetchPool<S> {
    sources: TrafficBalancer<S>,
    concurrency: NonZeroUsize,
}

impl<S: ChainDataSource> BlockFetchPool<S> {
    pub fn new(sources: Vec<Arc<S>>, concurrency: NonZeroUsize) -> Self {
        Self { sources: TrafficBalancer::new(sources), concurrency }
    }

    /// Only the sources at `positions` (e.g. validators agreeing on one tip), loads shared
    pub fn among(&self, positions: impl IntoIterator<Item = usize>) -> Self {
        Self { sources: self.sources.among(positions), concurrency: self.concurrency }
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
                // picked as the height is dispatched (load as of now, not at stream creation)
                let sources = sources.clone();
                tokio::spawn(async move {
                    sources
                        .failover(|source| async move {
                            let block = source.get_block(height).await?;
                            match block.header().height == height {
                                true => Ok(block),
                                false => Err(misanswered(format!(
                                    "asked height {height:?}, got {:?}",
                                    block.header().height
                                ))),
                            }
                        })
                        .await
                })
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
        self.sources
            .failover(|source| async move {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockChain;
    use crate::FailureMode;
    use zaino_primitives::testing::Chain;

    /// Spread over three validators, one lagging at 4: every height arrives in order (the lagging
    /// node's heights fall back to the others); among the lagging node alone, the stream ends at
    /// its first missing height and sends nothing after it
    #[tokio::test]
    async fn spread_falls_back_past_a_lagging_validator_and_a_lone_one_stops_at_its_gap() {
        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 9);
        let path = chain.path(tip.hash);
        let serving = |tip: usize| Arc::new(MockChain::serving(path[..=tip].to_vec()));
        let sources = vec![serving(9), serving(4), serving(9)];
        let concurrency = NonZeroUsize::new(4).expect("nz");
        let height = |h: u32| Height::try_from(h).expect("h");

        let spread = BlockFetchPool::new(sources, concurrency);
        let got: Vec<_> = spread.blocks(height(0), height(9)).collect().await;
        let hashes: Vec<_> = got.iter().map(|b| b.as_ref().map(|b| b.header().hash).ok()).collect();
        let expected: Vec<_> = path.iter().map(|b| Some(b.header().hash)).collect();
        assert_eq!(hashes, expected, "every height, in order, the chain's own blocks");

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
        let chain = Chain::new();
        let genesis = || chain.path(chain.genesis().hash);

        let flaky = MockChain::serving(genesis()).fail_next(2, FailureMode::Timeout);
        let pool = BlockFetchPool::new(vec![Arc::new(flaky)], concurrency);
        let got: Vec<_> = pool.blocks(height, height).collect().await;
        let hash = chain.genesis().hash;
        assert!(matches!(&got[..], [Ok(block)] if block.header().hash == hash), "{got:?}");

        let refused = MockChain::serving(genesis()).fail_next(1, FailureMode::Auth);
        let pool = BlockFetchPool::new(vec![Arc::new(refused)], concurrency);
        let got: Vec<_> = pool.blocks(height, height).collect().await;
        let [Err(QueryError::NonDomain(e))] = &got[..] else { panic!("{got:?}") };
        assert_eq!(e.mode, FailureMode::Auth);
    }

    /// `get_block(h)` sleeps `(7 − h) × 5 ms` with every height in flight: completion order is
    /// fully reversed, delivery order still ascending (indexes assert contiguous heights)
    #[tokio::test]
    async fn reversed_completion_still_streams_in_height_order() {
        use crate::{
            BlockLinks, GetTransactionError, MempoolListed, NonDomainError, PollReading,
            RawMempoolTransactions, SendRawTransactionError, TransactionResponse,
        };
        use zaino_primitives::types::TransactionId;
        struct ReverseDelay(Vec<Block>);
        impl ChainDataSource for ReverseDelay {
            async fn get_block(&self, height: Height) -> Result<Block, QueryError<GetBlockError>> {
                let h = u32::from(height);
                let delay = std::time::Duration::from_millis(u64::from(7 - h) * 5);
                tokio::time::sleep(delay).await;
                Ok(self.0[h as usize].clone())
            }
            async fn get_block_by_hash(
                &self,
                _: BlockHash,
            ) -> Result<Block, QueryError<GetBlockByHashError>> {
                unimplemented!("by-height test")
            }
            async fn get_block_links(&self, _: &[Height]) -> Result<BlockLinks, NonDomainError> {
                unimplemented!("block fetch only")
            }
            async fn get_poll_reading(
                &self,
                _: bool,
                _: &[Height],
            ) -> Result<PollReading, NonDomainError> {
                unimplemented!("block fetch only")
            }
            async fn get_raw_mempool_transactions(
                &self,
                _: &[MempoolListed],
            ) -> Result<RawMempoolTransactions, NonDomainError> {
                unimplemented!("block fetch only")
            }
            async fn get_transaction(
                &self,
                _: TransactionId,
            ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
                unimplemented!("block fetch only")
            }
            async fn send_raw_transaction(
                &self,
                _: Vec<u8>,
            ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
                unimplemented!("block fetch only")
            }
        }

        let mut chain = Chain::new();
        let tip = chain.extend(chain.genesis().hash, 7);
        let path = chain.path(tip.hash);
        let source = Arc::new(ReverseDelay(path));
        let pool = BlockFetchPool::new(vec![source], NonZeroUsize::new(8).expect("nz"));
        let got: Vec<_> = pool
            .blocks(Height::try_from(0u32).expect("h"), Height::try_from(7u32).expect("h"))
            .map(|b| b.map(|b| u32::from(b.header().height)).ok())
            .collect()
            .await;
        assert_eq!(got, (0..=7).map(Some).collect::<Vec<_>>());
    }
}
