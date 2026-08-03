//! RPC client errors.

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
            RpcError::Json(_) | RpcError::NullResult => FailureMode::Parse,
        };

        zaino_source::FetchError::new(kind, e.to_string())
    }
}
