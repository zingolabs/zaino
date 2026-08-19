//! RPC client errors.

#[cfg(test)]
use crate::client::MAX_RESPONSE_BYTES;

/// Errors from the JSON-RPC transport layer.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// HTTP request failed.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization/deserialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The server returned a JSON-RPC error object.
    #[error("rpc error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// Error message from the server.
        message: String,
    },

    /// Non-success HTTP status code.
    #[error("http status {0}")]
    Status(u16),

    /// Retries exhausted on work-queue-full responses.
    #[error("work queue full after {attempts} attempts")]
    WorkQueueExhausted {
        /// Number of attempts made.
        attempts: u32,
    },

    /// Server returned null result without an error object.
    #[error("null result without error")]
    NullResult,

    /// The response body exceeded the size this client is willing to buffer,
    /// and was abandoned part-way rather than read into memory.
    #[error("response body exceeded {max} bytes")]
    ResponseBodyTooLarge {
        /// The cap that was exceeded, in bytes.
        max: usize,
    },
}

impl From<RpcError> for zaino_source::FetchError {
    fn from(e: RpcError) -> Self {
        use zaino_source::FailureMode;

        let kind = match &e {
            RpcError::Http(inner) => {
                if inner.is_timeout() {
                    FailureMode::Timeout
                } else {
                    FailureMode::Connection
                }
            }
            RpcError::Status(401 | 403) => FailureMode::Auth,
            RpcError::Status(code) => FailureMode::HttpStatus(*code),
            RpcError::Rpc { code, .. } => FailureMode::RpcError(*code),
            RpcError::WorkQueueExhausted { .. } => FailureMode::RpcError(-1),
            // An abandoned body is `Parse` rather than a retryable mode on
            // purpose: the same request would produce the same oversized
            // response, so retrying only repeats the cost.
            RpcError::Json(_) | RpcError::NullResult | RpcError::ResponseBodyTooLarge { .. } => {
                FailureMode::Parse
            }
        };

        zaino_source::FetchError::new(kind, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An oversized body must not be classified as retryable: `Resilient` would
    /// otherwise re-issue the request and buffer the same oversized response
    /// again, turning the cap into an amplifier rather than a bound.
    #[test]
    fn an_oversized_body_is_not_retryable() {
        let fetch_error = zaino_source::FetchError::from(RpcError::ResponseBodyTooLarge {
            max: MAX_RESPONSE_BYTES,
        });

        assert_eq!(fetch_error.mode, zaino_source::FailureMode::Parse);
    }
}
