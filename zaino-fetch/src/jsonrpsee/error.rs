//! Hold error types for the JsonRpSeeConnector and related functionality.

use std::io;

/// Error type for JSON-RPC responses.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    /// The JSON-RPC error code
    pub code: i32,

    /// The JSON-RPC error message
    pub message: String,

    /// The JSON-RPC error data
    pub data: Option<serde_json::Value>,
}

/// General error type for handling JsonRpSeeConnector errors.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The cookie file used to authenticate with zebra could not be read
    #[error("could not read zebra authentication cookie file: {0}")]
    CookieReadError(io::Error),

    /// Reqwest Based Errors.
    #[error("Error: HTTP Request Error: {0}")]
    ReqwestError(#[from] reqwest::Error),

    /// Invalid URI Errors.
    #[error("Error: Invalid URI: {0}")]
    InvalidUriError(#[from] http::uri::InvalidUri),

    /// URL Parse Errors.
    #[error("Error: Invalid URL:{0}")]
    UrlParseError(#[from] url::ParseError),

    // Above this line, zaino failed to connect to a node
    // -----------------------------------
    // below this line, zaino connected to a node which returned a bad response
    /// Node returned a non-canonical status code
    #[error("validator returned invalid status code: {0}")]
    InvalidStatusCode(u16),

    /// Node returned a status code we don't expect
    #[error("validator returned unexpected status code: {0}")]
    UnexpectedStatusCode(u16),

    /// Node returned a status code we don't expect
    #[error("validator returned error code: {0}")]
    ErrorStatusCode(u16),

    /// The data returned by the validator was invalid.
    #[error("validator returned invalid {1} data: {0}")]
    BadNodeData(
        Box<dyn std::error::Error + Send + Sync + 'static>,
        &'static str,
    ),

    /// Validator returned empty response body
    #[error("no response body")]
    EmptyResponseBody,
}

impl TransportError {
    /// Converts TransportError to tonic::Status
    ///
    /// TODO: This impl should be changed to return the correct status [per this issue](https://github.com/zcash/lightwalletd/issues/497) before release,
    ///       however propagating the server error is useful during development.
    pub fn to_grpc_status(&self) -> tonic::Status {
        // TODO: Hide server error from clients before release. Currently useful for dev purposes.
        tonic::Status::internal(format!("Error: JsonRpSee Client Error: {self}"))
    }
}

impl From<TransportError> for tonic::Status {
    fn from(err: TransportError) -> Self {
        err.to_grpc_status()
    }
}
