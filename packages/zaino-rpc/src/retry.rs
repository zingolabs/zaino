//! Retry policy for JSON-RPC calls.
//!
//! Pure decision logic: no IO, no sleep. The client owns the delay.

/// Work-queue-full error code from Zebra/zcashd.
const WORK_QUEUE_FULL_CODE: i64 = -1;

/// Should this RPC error be retried?
pub(crate) fn is_retryable(code: i64) -> bool {
    code == WORK_QUEUE_FULL_CODE
}

/// Should we attempt another retry?
pub(crate) fn should_retry(attempt: u32, max_retries: u32) -> bool {
    attempt <= max_retries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_queue_full_is_retryable() {
        assert!(is_retryable(-1));
    }

    #[test]
    fn block_not_found_is_not_retryable() {
        assert!(!is_retryable(-8));
    }

    #[test]
    fn other_errors_not_retryable() {
        assert!(!is_retryable(0));
        assert!(!is_retryable(-32600));
    }

    #[test]
    fn retry_within_limit() {
        assert!(should_retry(1, 5));
        assert!(should_retry(5, 5));
    }

    #[test]
    fn retry_exhausted() {
        assert!(!should_retry(6, 5));
    }
}
