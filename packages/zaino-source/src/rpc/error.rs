/// JSON-RPC transport failure (`ResponseBodyTooLarge` = abandoned mid-read)
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
            // coded refusal: its message = the error's Display (no cause to box)
            RpcError::Rpc { message, .. } => crate::NonDomainError::new(kind, message),
            // real error value: type + source() chain kept, never stringified
            other => crate::NonDomainError::from_cause(kind, other),
        }
    }
}
