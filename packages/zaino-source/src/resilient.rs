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

use crate::error::{QueryError, ResilientError, TransportFailure, UnavailableError};

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

/// Whether a transport error kind is worth retrying.
fn is_retryable(kind: &TransportFailure) -> bool {
    match kind {
        TransportFailure::Connection => true,
        TransportFailure::Timeout => true,
        TransportFailure::HttpStatus(code) => *code >= 500,
        TransportFailure::RpcError(code) => *code == -1, // work-queue-full
        TransportFailure::Parse => false,
        TransportFailure::Auth => false,
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
    async fn with_retry<T, E, F, Fut>(&self, mut f: F) -> Result<T, ResilientError<E>>
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

                Err(QueryError::Domain(e)) => return Err(ResilientError::Domain(e)),

                Err(QueryError::Transport(e)) => {
                    if !is_retryable(&e.kind) || attempt >= self.policy.max_attempts {
                        if is_retryable(&e.kind) {
                            return Err(ResilientError::Unavailable(UnavailableError {
                                attempts: attempt,
                                last_error: e,
                            }));
                        }
                        return Err(ResilientError::Transport(e));
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

impl<V: crate::GetBlockBytes> Resilient<V> {
    /// Fetch raw block bytes with retry.
    pub async fn get_block_bytes(
        &self,
        height: Height,
    ) -> Result<Vec<u8>, ResilientError<crate::GetBlockBytesError>> {
        self.with_retry(|| self.inner.get_block_bytes(height)).await
    }
}

impl<V: crate::GetChainTip> Resilient<V> {
    /// Fetch chain tip with retry.
    pub async fn get_chain_tip(
        &self,
    ) -> Result<(BlockHash, Height), ResilientError<crate::GetChainTipError>> {
        self.with_retry(|| self.inner.get_chain_tip()).await
    }
}

impl<V: crate::GetTreestate> Resilient<V> {
    /// Fetch treestate with retry.
    pub async fn get_treestate(
        &self,
        height: Height,
    ) -> Result<Treestate, ResilientError<crate::GetTreestateError>> {
        self.with_retry(|| self.inner.get_treestate(height)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_is_retryable() {
        assert!(is_retryable(&TransportFailure::Connection));
    }

    #[test]
    fn timeout_is_retryable() {
        assert!(is_retryable(&TransportFailure::Timeout));
    }

    #[test]
    fn http_500_is_retryable() {
        assert!(is_retryable(&TransportFailure::HttpStatus(500)));
        assert!(is_retryable(&TransportFailure::HttpStatus(503)));
    }

    #[test]
    fn http_400_is_not_retryable() {
        assert!(!is_retryable(&TransportFailure::HttpStatus(400)));
        assert!(!is_retryable(&TransportFailure::HttpStatus(404)));
    }

    #[test]
    fn work_queue_full_is_retryable() {
        assert!(is_retryable(&TransportFailure::RpcError(-1)));
    }

    #[test]
    fn block_not_found_rpc_is_not_retryable() {
        assert!(!is_retryable(&TransportFailure::RpcError(-8)));
    }

    #[test]
    fn parse_is_not_retryable() {
        assert!(!is_retryable(&TransportFailure::Parse));
    }

    #[test]
    fn auth_is_not_retryable() {
        assert!(!is_retryable(&TransportFailure::Auth));
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
}
