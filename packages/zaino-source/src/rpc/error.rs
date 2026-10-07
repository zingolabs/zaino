#[cfg(test)]
use super::client::MAX_RESPONSE_BYTES;

/// JSON-RPC transport failure (`Rpc` = server's error object, `NullResult` = neither result
/// nor error, `ResponseBodyTooLarge` = abandoned mid-read)
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("http status {0}")]
    Status(u16),
    #[error("null result without error")]
    NullResult,
    #[error("response body exceeded {max} bytes")]
    ResponseBodyTooLarge { max: usize },
    #[error("batch reply does not answer each call exactly once")]
    BatchMismatch,
}

impl RpcError {
    pub(crate) fn failure_mode(&self) -> crate::FailureMode {
        use crate::FailureMode;

        match self {
            RpcError::Http(inner) if inner.is_timeout() => FailureMode::Timeout,
            RpcError::Http(_) => FailureMode::Connection,
            RpcError::Status(401 | 403) => FailureMode::Auth,
            RpcError::Status(code) => FailureMode::HttpStatus(*code),
            RpcError::Rpc { code, .. } => FailureMode::RpcError(*code),
            // oversized body = `Parse`, not retryable (same request → same oversized reply)
            RpcError::Json(_)
            | RpcError::NullResult
            | RpcError::ResponseBodyTooLarge { .. }
            | RpcError::BatchMismatch => FailureMode::Parse,
        }
    }
}

impl From<RpcError> for crate::NonDomainError {
    fn from(e: RpcError) -> Self {
        let kind = e.failure_mode();

        match e {
            // coded refusal: message = the content (adapters build domain rejections from it)
            RpcError::Rpc { message, .. } => crate::NonDomainError::new(kind, message),
            // real error value: type + source() chain kept, never stringified
            other => crate::NonDomainError::from_cause(kind, other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never transient (a retry would buffer the same oversized body again: amplifier, not cap)
    #[test]
    fn an_oversized_body_is_not_retryable() {
        let fetch_error =
            crate::NonDomainError::from(RpcError::ResponseBodyTooLarge { max: MAX_RESPONSE_BYTES });

        assert_eq!(fetch_error.mode, crate::FailureMode::Parse);
        assert!(!fetch_error.mode.is_transient());
    }
}
