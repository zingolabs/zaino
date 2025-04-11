//! Hold error types for the JsonRpSeeConnector and related functionality.

/// General error type for handling JsonRpSeeConnector errors.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpSeeConnectorError {
    /// Type for errors without an underlying source.
    #[error("Error: {0}")]
    JsonRpSeeClientError(String),

    /// Serialization/Deserialization Errors.
    #[error("Error: Serialization/Deserialization Error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// Reqwest Based Errors.
    #[error("Error: HTTP Request Error: {0}")]
    ReqwestError(#[from] reqwest::Error),

    /// Invalid URI Errors.
    #[error("Error: Invalid URI: {0}")]
    InvalidUriError(#[from] http::uri::InvalidUri),

    /// URL Parse Errors.
    #[error("Error: Invalid URL:{0}")]
    UrlParseError(#[from] url::ParseError),

    /// std::io::Error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

impl JsonRpSeeConnectorError {
    /// Constructor for errors without an underlying source
    pub fn new(msg: impl Into<String>) -> Self {
        JsonRpSeeConnectorError::JsonRpSeeClientError(msg.into())
    }

    /// Converts JsonRpSeeConnectorError to tonic::Status
    ///
    /// TODO: This impl should be changed to return the correct status [https://github.com/zcash/lightwalletd/issues/497] before release,
    ///       however propagating the server error is useful during development.
    pub fn to_grpc_status(&self) -> tonic::Status {
        // TODO: Hide server error from clients before release. Currently useful for dev purposes.
        tonic::Status::internal(format!("Error: JsonRpSee Client Error: {}", self))
    }
}

impl From<JsonRpSeeConnectorError> for tonic::Status {
    fn from(err: JsonRpSeeConnectorError) -> Self {
        err.to_grpc_status()
    }
}
