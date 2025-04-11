//! Holds error types for Zaino-state.

/// Errors related to the `StateService`.
#[derive(Debug, thiserror::Error)]
pub enum StateServiceError {
    /// Custom Errors. *Remove before production.
    #[error("Custom error: {0}")]
    Custom(String),

    /// Error from a Tokio JoinHandle.
    #[error("Join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError),

    /// RPC error in compatibility with zcashd.
    #[error("RPC error: {0:?}")]
    RpcError(#[from] zaino_fetch::jsonrpsee::connector::RpcError),

    /// Tonic gRPC error.
    #[error("Tonic status error: {0}")]
    TonicStatusError(#[from] tonic::Status),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    SerializationError(#[from] zebra_chain::serialization::SerializationError),

    /// Integer conversion error.
    #[error("Integer conversion error: {0}")]
    TryFromIntError(#[from] std::num::TryFromIntError),

    /// std::io::Error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    /// A generic boxed error.
    #[error("Generic error: {0}")]
    Generic(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl From<StateServiceError> for tonic::Status {
    fn from(error: StateServiceError) -> Self {
        match error {
            StateServiceError::Custom(message) => tonic::Status::internal(message),
            StateServiceError::JoinError(err) => {
                tonic::Status::internal(format!("Join error: {}", err))
            }
            StateServiceError::JsonRpcConnectorError(err) => {
                tonic::Status::internal(format!("JsonRpcConnector error: {}", err))
            }
            StateServiceError::RpcError(err) => {
                tonic::Status::internal(format!("RPC error: {:?}", err))
            }
            StateServiceError::TonicStatusError(err) => err,
            StateServiceError::SerializationError(err) => {
                tonic::Status::internal(format!("Serialization error: {}", err))
            }
            StateServiceError::TryFromIntError(err) => {
                tonic::Status::internal(format!("Integer conversion error: {}", err))
            }
            StateServiceError::IoError(err) => {
                tonic::Status::internal(format!("IO error: {}", err))
            }
            StateServiceError::Generic(err) => {
                tonic::Status::internal(format!("Generic error: {}", err))
            }
        }
    }
}

/// Errors related to the `FetchService`.
#[derive(Debug, thiserror::Error)]
pub enum FetchServiceError {
    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError),

    /// Error from the block cache.
    #[error("Mempool error: {0}")]
    BlockCacheError(#[from] BlockCacheError),

    /// Error from the mempool.
    #[error("Mempool error: {0}")]
    MempoolError(#[from] MempoolError),

    /// RPC error in compatibility with zcashd.
    #[error("RPC error: {0:?}")]
    RpcError(#[from] zaino_fetch::jsonrpsee::connector::RpcError),

    /// Tonic gRPC error.
    #[error("Tonic status error: {0}")]
    TonicStatusError(#[from] tonic::Status),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    SerializationError(#[from] zebra_chain::serialization::SerializationError),
}

impl From<FetchServiceError> for tonic::Status {
    fn from(error: FetchServiceError) -> Self {
        match error {
            FetchServiceError::Critical(message) => tonic::Status::internal(message),
            FetchServiceError::JsonRpcConnectorError(err) => {
                tonic::Status::internal(format!("JsonRpcConnector error: {}", err))
            }
            FetchServiceError::BlockCacheError(err) => {
                tonic::Status::internal(format!("BlockCache error: {}", err))
            }
            FetchServiceError::MempoolError(err) => {
                tonic::Status::internal(format!("Mempool error: {}", err))
            }
            FetchServiceError::RpcError(err) => {
                tonic::Status::internal(format!("RPC error: {:?}", err))
            }
            FetchServiceError::TonicStatusError(err) => err,
            FetchServiceError::SerializationError(err) => {
                tonic::Status::internal(format!("Serialization error: {}", err))
            }
        }
    }
}

/// Errors related to the `Mempool`.
#[derive(Debug, thiserror::Error)]
pub enum MempoolError {
    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError),

    /// Error from a Tokio Watch Receiver.
    #[error("Join error: {0}")]
    WatchRecvError(#[from] tokio::sync::watch::error::RecvError),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),
}

/// Errors related to the `BlockCache`.
#[derive(Debug, thiserror::Error)]
pub enum BlockCacheError {
    /// Custom Errors. *Remove before production.
    #[error("Custom error: {0}")]
    Custom(String),

    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Errors from the NonFinalisedState.
    #[error("NonFinalisedState Error: {0}")]
    NonFinalisedStateError(#[from] NonFinalisedStateError),

    /// Errors from the FinalisedState.
    #[error("FinalisedState Error: {0}")]
    FinalisedStateError(#[from] FinalisedStateError),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError),

    /// Chain parse error.
    #[error("Chain parse error: {0}")]
    ChainParseError(#[from] zaino_fetch::chain::error::ParseError),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    SerializationError(#[from] zebra_chain::serialization::SerializationError),

    /// UTF-8 conversion error.
    #[error("UTF-8 conversion error: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),

    /// Integer parsing error.
    #[error("Integer parsing error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    /// Integer conversion error.
    #[error("Integer conversion error: {0}")]
    TryFromIntError(#[from] std::num::TryFromIntError),
}

/// Errors related to the `NonFinalisedState`.
#[derive(Debug, thiserror::Error)]
pub enum NonFinalisedStateError {
    /// Custom Errors. *Remove before production.
    #[error("Custom error: {0}")]
    Custom(String),

    /// Required data is missing from the non-finalised state.
    #[error("Missing data: {0}")]
    MissingData(String),

    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),
}

/// Errors related to the `FinalisedState`.
#[derive(Debug, thiserror::Error)]
pub enum FinalisedStateError {
    /// Custom Errors. *Remove before production.
    #[error("Custom error: {0}")]
    Custom(String),

    /// Required data is missing from the non-finalised state.
    #[error("Missing data: {0}")]
    MissingData(String),

    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from the LMDB database.
    #[error("LMDB database error: {0}")]
    LmdbError(#[from] lmdb::Error),

    /// Serde Json serialisation / deserialisation errors.
    #[error("LMDB database error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::JsonRpSeeConnectorError),

    /// std::io::Error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// A general error type to represent error StatusTypes.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Unexpected status error: {0:?}")]
pub struct StatusError(pub crate::status::StatusType);
