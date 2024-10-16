//! Hold error types for the JsonRpcConnector and related functionality.

/// General error type for handling JsonRpcConnector errors.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpcConnectorError {
    /// Uncatogorized Errors.
    #[error("{0}")]
    CustomError(String),

    /// Serialization/Deserialization Errors.
    #[error("Serialization/Deserialization Error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// Reqwest Based Errors.
    #[error("HTTP Request Error: {0}")]
    ReqwestError(#[from] reqwest::Error),

    ///HTTP Errors.
    #[error("HTTP Error: {0}")]
    HttpError(#[from] http::Error),

    /// Invalid URI Errors.
    #[error("Invalid URI: {0}")]
    InvalidUriError(#[from] http::uri::InvalidUri),

    /// Invalid URL Errors.
    #[error("Invalid URL: {0}")]
    InvalidUrlError(#[from] url::ParseError),

    /// UTF-8 Conversion Errors.
    #[error("UTF-8 Conversion Error")]
    Utf8Error(#[from] std::string::FromUtf8Error),

    /// Request Timeout Errors.
    #[error("Request Timeout Error")]
    TimeoutError(#[from] tokio::time::error::Elapsed),
}

impl JsonRpcConnectorError {
    /// Constructor for errors without an underlying source
    pub fn new(msg: impl Into<String>) -> Self {
        JsonRpcConnectorError::CustomError(msg.into())
    }

    /// Maps JsonRpcConnectorError to tonic::Status
    pub fn to_grpc_status(&self) -> tonic::Status {
        eprintln!("Error occurred: {}.", self);

        match self {
            JsonRpcConnectorError::SerdeJsonError(_) => {
                tonic::Status::invalid_argument(self.to_string())
            }
            JsonRpcConnectorError::ReqwestError(_) => tonic::Status::unavailable(self.to_string()),
            JsonRpcConnectorError::HttpError(_) => tonic::Status::internal(self.to_string()),
            _ => tonic::Status::internal(self.to_string()),
        }
    }
}

impl From<JsonRpcConnectorError> for tonic::Status {
    fn from(err: JsonRpcConnectorError) -> Self {
        err.to_grpc_status()
    }
}
