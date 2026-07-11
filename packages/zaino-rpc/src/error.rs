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

impl From<RpcError> for zaino_source::TransportError {
    fn from(e: RpcError) -> Self {
        use zaino_source::TransportFailure;

        let kind = match &e {
            RpcError::Http(inner) => {
                if inner.is_timeout() {
                    TransportFailure::Timeout
                } else {
                    TransportFailure::Connection
                }
            }
            RpcError::Status(401 | 403) => TransportFailure::Auth,
            RpcError::Status(code) => TransportFailure::HttpStatus(*code),
            RpcError::Rpc { code, .. } => TransportFailure::RpcError(*code),
            RpcError::WorkQueueExhausted { .. } => TransportFailure::RpcError(-1),
            RpcError::Json(_) | RpcError::NullResult => TransportFailure::Parse,
        };

        zaino_source::TransportError::new(kind, e.to_string())
    }
}
