//! Resilience wrapper: retry-aware decorator for source adapters.
//!
//! Wraps any adapter implementing the query traits, adds configurable
//! retry with backoff, and classifies transport errors by kind.
//!
//! ```ignore
//! let adapter = ZebraRpcAdapter::new(rpc);
//! let source = Resilient::new(adapter, RetryPolicy::default());
//! let bytes = source.get_block_bytes(height).await?; // → ResilientError
//! ```
//!
//! `Resilient<V>` does NOT implement the query traits — it has its own
//! methods that return `ResilientError` instead of `QueryError`. The
//! consumer uses `Resilient<V>` directly.

use std::future::Future;
use std::time::Duration;

use crate::error::{FailureMode, QueryError, SourceError, UnavailableError};

/// Retry policy configuration.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first).
    pub max_attempts: u32,
    /// Initial delay between retries.
    pub initial_delay: Duration,
    /// Multiply delay by this factor after each retry.
    pub backoff_factor: f64,
    /// Maximum delay between retries.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            backoff_factor: 2.0,
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    fn delay_for(&self, attempt: u32) -> Duration {
        if attempt <= 1 {
            return self.initial_delay;
        }
        let exponent = (attempt - 1).min(30);
        let factor = self.backoff_factor.powi(exponent as i32);
        let millis = (self.initial_delay.as_millis() as f64 * factor) as u64;
        Duration::from_millis(millis).min(self.max_delay)
    }
}

/// The validator's work queue is full — it is up, but busy.
const WORK_QUEUE_FULL: i64 = -1;

/// The validator is still warming up and not yet serving.
///
/// A transient startup condition, not a rejection: it ends on its own once the
/// node finishes loading. Retrying is the only correct response — failing
/// immediately makes a validator that is merely slow to start look permanently
/// broken to everything downstream.
const IN_WARMUP: i64 = -28;

/// Whether a transport error kind is worth retrying.
fn is_retryable(kind: &FailureMode) -> bool {
    match kind {
        FailureMode::Connection => true,
        FailureMode::Timeout => true,
        FailureMode::HttpStatus(code) => *code >= 500,
        // Everything else the validator answers with a code is its considered
        // reply, and asking again will produce the same one.
        FailureMode::RpcError(code) => *code == WORK_QUEUE_FULL || *code == IN_WARMUP,
        FailureMode::Parse => false,
        FailureMode::Auth => false,
    }
}

/// Resilience wrapper around any source adapter.
///
/// Does NOT implement query traits. Exposes its own methods that
/// return [`ResilientError`] — the consumer uses this type directly.
pub struct Resilient<V> {
    inner: V,
    policy: RetryPolicy,
}

impl<V> Resilient<V> {
    /// Wrap an adapter with a retry policy.
    pub fn new(inner: V, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }

    /// Core retry loop. Each public method delegates here.
    async fn with_retry<T, E, F, Fut>(&self, mut f: F) -> Result<T, SourceError<E>>
    where
        E: core::fmt::Debug + core::fmt::Display,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, QueryError<E>>>,
    {
        let mut attempt = 0u32;

        loop {
            attempt += 1;

            match f().await {
                Ok(value) => return Ok(value),

                Err(QueryError::Domain(e)) => return Err(SourceError::Domain(e)),

                Err(QueryError::Fetch(e)) => {
                    if !is_retryable(&e.mode) || attempt >= self.policy.max_attempts {
                        if is_retryable(&e.mode) {
                            return Err(SourceError::Unavailable(UnavailableError {
                                attempts: attempt,
                                last_error: e,
                            }));
                        }
                        return Err(SourceError::Fetch(e));
                    }

                    tokio::time::sleep(self.policy.delay_for(attempt)).await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public methods — one per query trait, returning ResilientError
// ---------------------------------------------------------------------------

use zaino_primitives::types::{BlockHash, Height, Treestate};

impl<V: crate::GetBlock> Resilient<V> {
    /// Fetch a parsed block with retry.
    pub async fn get_block(
        &self,
        height: Height,
    ) -> Result<zaino_primitives::types::Block, SourceError<crate::GetBlockError>> {
        self.with_retry(|| self.inner.get_block(height)).await
    }
}

impl<V: crate::GetChainTip> Resilient<V> {
    /// Fetch chain tip with retry.
    pub async fn get_chain_tip(
        &self,
    ) -> Result<(BlockHash, Height), SourceError<crate::GetChainTipError>> {
        self.with_retry(|| self.inner.get_chain_tip()).await
    }
}

impl<V: crate::GetTreestate> Resilient<V> {
    /// Fetch treestate with retry.
    pub async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, SourceError<crate::GetTreestateError>> {
        self.with_retry(|| self.inner.get_treestate(height)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Unit tests: classification + backoff
    // ---------------------------------------------------------------

    #[test]
    fn connection_is_retryable() {
        assert!(is_retryable(&FailureMode::Connection));
    }

    #[test]
    fn timeout_is_retryable() {
        assert!(is_retryable(&FailureMode::Timeout));
    }

    #[test]
    fn http_500_is_retryable() {
        assert!(is_retryable(&FailureMode::HttpStatus(500)));
        assert!(is_retryable(&FailureMode::HttpStatus(503)));
    }

    #[test]
    fn http_400_is_not_retryable() {
        assert!(!is_retryable(&FailureMode::HttpStatus(400)));
        assert!(!is_retryable(&FailureMode::HttpStatus(404)));
    }

    #[test]
    fn work_queue_full_is_retryable() {
        assert!(is_retryable(&FailureMode::RpcError(-1)));
    }

    /// A warming-up validator is starting, not broken. Failing immediately here
    /// makes a slow start look like a permanent fault to every consumer.
    #[test]
    fn in_warmup_is_retryable() {
        assert!(is_retryable(&FailureMode::RpcError(-28)));
    }

    #[test]
    fn block_not_found_rpc_is_not_retryable() {
        assert!(!is_retryable(&FailureMode::RpcError(-8)));
    }

    #[test]
    fn parse_is_not_retryable() {
        assert!(!is_retryable(&FailureMode::Parse));
    }

    #[test]
    fn auth_is_not_retryable() {
        assert!(!is_retryable(&FailureMode::Auth));
    }

    #[test]
    fn delay_exponential_backoff() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_millis(100),
            backoff_factor: 2.0,
            max_delay: Duration::from_secs(10),
            ..Default::default()
        };
        assert_eq!(policy.delay_for(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for(3), Duration::from_millis(400));
    }

    #[test]
    fn delay_capped_at_max() {
        let policy = RetryPolicy {
            initial_delay: Duration::from_secs(1),
            backoff_factor: 10.0,
            max_delay: Duration::from_secs(5),
            ..Default::default()
        };
        assert_eq!(policy.delay_for(3), Duration::from_secs(5));
    }

    // ---------------------------------------------------------------
    // Integration tests: Resilient<MockChain> with failure injection
    // ---------------------------------------------------------------

    // The mock is compiled for this crate's own tests as well as behind the
    // `testing` feature, so these need no gate of their own.
    mod integration {
        use super::*;
        use crate::mock::MockChain;
        use zaino_primitives::types::{Block, BlockHash, BlockHeader, ChainMetadata, Height};

        fn height(h: u32) -> Height {
            Height::try_from(h).expect("valid")
        }

        fn hash(b: u8) -> BlockHash {
            BlockHash::from([b; 32])
        }

        fn test_block(h: u32, hash_byte: u8) -> Block {
            Block {
                header: BlockHeader {
                    hash: hash(hash_byte),
                    prev_hash: BlockHash::ZERO,
                    height: height(h),
                    time: 0,
                    merkle_root: [0; 32].into(),
                    block_commitments: [0; 32].into(),
                    bits: 0,
                    nonce: [0; 32],
                },
                transactions: vec![],
                chain_metadata: ChainMetadata {
                    sapling_tree_size: 0,
                    orchard_tree_size: 0,
                    ironwood_tree_size: 0,
                },
            }
        }

        fn fast_policy(max_attempts: u32) -> RetryPolicy {
            RetryPolicy {
                max_attempts,
                initial_delay: Duration::from_millis(1),
                backoff_factor: 1.0,
                max_delay: Duration::from_millis(1),
            }
        }

        #[tokio::test]
        async fn retries_transient_then_succeeds() {
            let mock = MockChain::new()
                .with_block(test_block(0, 1))
                .fail_next(2, FailureMode::Timeout);

            let source = Resilient::new(mock, fast_policy(5));

            let block = source.get_block(height(0)).await.expect("succeeds");
            assert_eq!(block.header.hash, hash(1));
        }

        #[tokio::test]
        async fn exhausts_retries_returns_unavailable() {
            let mock = MockChain::new()
                .with_block(test_block(0, 1))
                .fail_next(10, FailureMode::Connection);

            let source = Resilient::new(mock, fast_policy(3));

            let err = source.get_block(height(0)).await.unwrap_err();
            assert!(
                matches!(err, SourceError::Unavailable(ref u) if u.attempts == 3),
                "expected Unavailable after 3 attempts, got: {err:?}"
            );
        }

        #[tokio::test]
        async fn fatal_error_not_retried() {
            let mock = MockChain::new()
                .with_block(test_block(0, 1))
                .fail_next(1, FailureMode::Auth);

            let source = Resilient::new(mock, fast_policy(5));

            let err = source.get_block(height(0)).await.unwrap_err();
            assert!(
                matches!(err, SourceError::Fetch(ref e) if e.mode == FailureMode::Auth),
                "expected Fetch(Auth), got: {err:?}"
            );
        }

        #[tokio::test]
        async fn domain_error_not_retried() {
            let mock = MockChain::new();

            let source = Resilient::new(mock, fast_policy(5));

            let err = source.get_block(height(99)).await.unwrap_err();
            assert!(
                matches!(err, SourceError::Domain(_)),
                "expected Domain, got: {err:?}"
            );
        }

        #[tokio::test]
        async fn chain_tip_retries_transient() {
            let mock = MockChain::new()
                .with_block(test_block(0, 1))
                .fail_next(1, FailureMode::Timeout);

            let source = Resilient::new(mock, fast_policy(3));

            let (tip_hash, tip_height) = source.get_chain_tip().await.expect("succeeds");
            assert_eq!(tip_hash, hash(1));
            assert_eq!(tip_height, height(0));
        }
    }
}
