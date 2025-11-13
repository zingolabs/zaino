#![allow(deprecated)]
//! Holds error types for Zaino-state.

// Needs to be module level due to the thiserror::Error macro

use crate::BlockHash;

use std::{any::type_name, fmt::Display};

use zaino_fetch::jsonrpsee::connector::RpcRequestError;

impl<T: ToString> From<RpcRequestError<T>> for StateServiceError {
    fn from(value: RpcRequestError<T>) -> Self {
        match value {
            RpcRequestError::Transport(transport_error) => {
                Self::JsonRpcConnectorError(transport_error)
            }
            RpcRequestError::Method(e) => Self::UnhandledRpcError(format!(
                "{}: {}",
                std::any::type_name::<T>(),
                e.to_string()
            )),
            RpcRequestError::JsonRpc(error) => Self::Custom(format!("bad argument: {error}")),
            RpcRequestError::InternalUnrecoverable(e) => Self::Custom(e.to_string()),
            RpcRequestError::ServerWorkQueueFull => {
                Self::Custom("Server queue full. Handling for this not yet implemented".to_string())
            }
            RpcRequestError::UnexpectedErrorResponse(error) => Self::Custom(format!("{error}")),
        }
    }
}

/// Errors related to the `StateService`.
#[deprecated]
#[derive(Debug, thiserror::Error)]
pub enum StateServiceError {
    /// An rpc-specific error we haven't accounted for
    #[error("unhandled fallible RPC call {0}")]
    UnhandledRpcError(String),
    /// Custom Errors. *Remove before production.
    #[error("Custom error: {0}")]
    Custom(String),

    /// Error from a Tokio JoinHandle.
    #[error("Join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::TransportError),

    /// RPC error in compatibility with zcashd.
    #[error("RPC error: {0:?}")]
    RpcError(#[from] zaino_fetch::jsonrpsee::connector::RpcError),

    /// Error from the block cache.
    #[error("Mempool error: {0}")]
    BlockCacheError(#[from] BlockCacheError),

    /// Error from the mempool.
    #[error("Mempool error: {0}")]
    MempoolError(#[from] MempoolError),

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

    /// The zebrad version and zebra library version do not align
    #[error(
        "zebrad version mismatch. this build of zaino requires a \
        version of {expected_zebrad_version}, but the connected zebrad \
        is version {connected_zebrad_version}"
    )]
    ZebradVersionMismatch {
        /// The version string or commit hash we specify in Cargo.lock
        expected_zebrad_version: String,
        /// The version string of the zebrad, plus its git describe
        /// information if applicable
        connected_zebrad_version: String,
    },
}

#[allow(deprecated)]
impl From<StateServiceError> for tonic::Status {
    fn from(error: StateServiceError) -> Self {
        match error {
            StateServiceError::Custom(message) => tonic::Status::internal(message),
            StateServiceError::JoinError(err) => {
                tonic::Status::internal(format!("Join error: {err}"))
            }
            StateServiceError::JsonRpcConnectorError(err) => {
                tonic::Status::internal(format!("JsonRpcConnector error: {err}"))
            }
            StateServiceError::RpcError(err) => {
                tonic::Status::internal(format!("RPC error: {err:?}"))
            }
            StateServiceError::BlockCacheError(err) => {
                tonic::Status::internal(format!("BlockCache error: {err:?}"))
            }
            StateServiceError::MempoolError(err) => {
                tonic::Status::internal(format!("Mempool error: {err:?}"))
            }
            StateServiceError::TonicStatusError(err) => err,
            StateServiceError::SerializationError(err) => {
                tonic::Status::internal(format!("Serialization error: {err}"))
            }
            StateServiceError::TryFromIntError(err) => {
                tonic::Status::internal(format!("Integer conversion error: {err}"))
            }
            StateServiceError::IoError(err) => tonic::Status::internal(format!("IO error: {err}")),
            StateServiceError::Generic(err) => {
                tonic::Status::internal(format!("Generic error: {err}"))
            }
            ref err @ StateServiceError::ZebradVersionMismatch { .. } => {
                tonic::Status::internal(err.to_string())
            }
            StateServiceError::UnhandledRpcError(e) => tonic::Status::internal(e.to_string()),
        }
    }
}

impl<T: ToString> From<RpcRequestError<T>> for FetchServiceError {
    fn from(value: RpcRequestError<T>) -> Self {
        match value {
            RpcRequestError::Transport(transport_error) => {
                FetchServiceError::JsonRpcConnectorError(transport_error)
            }
            RpcRequestError::JsonRpc(error) => {
                FetchServiceError::Critical(format!("argument failed to serialze: {error}"))
            }
            RpcRequestError::InternalUnrecoverable(e) => {
                FetchServiceError::Critical(format!("Internal unrecoverable error: {e}"))
            }
            RpcRequestError::ServerWorkQueueFull => FetchServiceError::Critical(
                "Server queue full. Handling for this not yet implemented".to_string(),
            ),
            RpcRequestError::Method(e) => FetchServiceError::Critical(format!(
                "unhandled rpc-specific {} error: {}",
                type_name::<T>(),
                e.to_string()
            )),
            RpcRequestError::UnexpectedErrorResponse(error) => {
                FetchServiceError::Critical(format!(
                    "unhandled rpc-specific {} error: {}",
                    type_name::<T>(),
                    error
                ))
            }
        }
    }
}

/// Errors related to the `FetchService`.
#[deprecated]
#[derive(Debug, thiserror::Error)]
pub enum FetchServiceError {
    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::TransportError),

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

#[allow(deprecated)]
impl From<FetchServiceError> for tonic::Status {
    fn from(error: FetchServiceError) -> Self {
        match error {
            FetchServiceError::Critical(message) => tonic::Status::internal(message),
            FetchServiceError::JsonRpcConnectorError(err) => {
                tonic::Status::internal(format!("JsonRpcConnector error: {err}"))
            }
            FetchServiceError::BlockCacheError(err) => {
                tonic::Status::internal(format!("BlockCache error: {err}"))
            }
            FetchServiceError::MempoolError(err) => {
                tonic::Status::internal(format!("Mempool error: {err}"))
            }
            FetchServiceError::RpcError(err) => {
                tonic::Status::internal(format!("RPC error: {err:?}"))
            }
            FetchServiceError::TonicStatusError(err) => err,
            FetchServiceError::SerializationError(err) => {
                tonic::Status::internal(format!("Serialization error: {err}"))
            }
        }
    }
}
/// These aren't the best conversions, but the MempoolError should go away
/// in favor of a new type with the new chain cache is complete
impl<T: ToString> From<RpcRequestError<T>> for MempoolError {
    fn from(value: RpcRequestError<T>) -> Self {
        match value {
            RpcRequestError::Transport(transport_error) => {
                MempoolError::JsonRpcConnectorError(transport_error)
            }
            RpcRequestError::JsonRpc(error) => {
                MempoolError::Critical(format!("argument failed to serialze: {error}"))
            }
            RpcRequestError::InternalUnrecoverable(e) => {
                MempoolError::Critical(format!("Internal unrecoverable error: {e}"))
            }
            RpcRequestError::ServerWorkQueueFull => MempoolError::Critical(
                "Server queue full. Handling for this not yet implemented".to_string(),
            ),
            RpcRequestError::Method(e) => MempoolError::Critical(format!(
                "unhandled rpc-specific {} error: {}",
                type_name::<T>(),
                e.to_string()
            )),
            RpcRequestError::UnexpectedErrorResponse(error) => MempoolError::Critical(format!(
                "unhandled rpc-specific {} error: {}",
                type_name::<T>(),
                error
            )),
        }
    }
}

/// Errors related to the `Mempool`.
#[derive(Debug, thiserror::Error)]
pub enum MempoolError {
    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Incorrect expected chain tip given from client.
    #[error(
        "Incorrect chain tip (expected {expected_chain_tip:?}, current {current_chain_tip:?})"
    )]
    IncorrectChainTip {
        expected_chain_tip: BlockHash,
        current_chain_tip: BlockHash,
    },

    /// Error from JsonRpcConnector.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::TransportError),

    /// Errors originating from the BlockchainSource in use.
    #[error("blockchain source error: {0}")]
    BlockchainSourceError(#[from] crate::chain_index::source::BlockchainSourceError),

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
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::TransportError),

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
/// These aren't the best conversions, but the NonFinalizedStateError should go away
/// in favor of a new type with the new chain cache is complete
impl<T: ToString> From<RpcRequestError<T>> for NonFinalisedStateError {
    fn from(value: RpcRequestError<T>) -> Self {
        match value {
            RpcRequestError::Transport(transport_error) => {
                NonFinalisedStateError::JsonRpcConnectorError(transport_error)
            }
            RpcRequestError::JsonRpc(error) => {
                NonFinalisedStateError::Custom(format!("argument failed to serialze: {error}"))
            }
            RpcRequestError::InternalUnrecoverable(e) => {
                NonFinalisedStateError::Custom(format!("Internal unrecoverable error: {e}"))
            }
            RpcRequestError::ServerWorkQueueFull => NonFinalisedStateError::Custom(
                "Server queue full. Handling for this not yet implemented".to_string(),
            ),
            RpcRequestError::Method(e) => NonFinalisedStateError::Custom(format!(
                "unhandled rpc-specific {} error: {}",
                type_name::<T>(),
                e.to_string()
            )),
            RpcRequestError::UnexpectedErrorResponse(error) => {
                NonFinalisedStateError::Custom(format!(
                    "unhandled rpc-specific {} error: {}",
                    type_name::<T>(),
                    error
                ))
            }
        }
    }
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
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::TransportError),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),
}
/// These aren't the best conversions, but the FinalizedStateError should go away
/// in favor of a new type with the new chain cache is complete
impl<T: ToString> From<RpcRequestError<T>> for FinalisedStateError {
    fn from(value: RpcRequestError<T>) -> Self {
        match value {
            RpcRequestError::Transport(transport_error) => {
                FinalisedStateError::JsonRpcConnectorError(transport_error)
            }
            RpcRequestError::JsonRpc(error) => {
                FinalisedStateError::Custom(format!("argument failed to serialze: {error}"))
            }
            RpcRequestError::InternalUnrecoverable(e) => {
                FinalisedStateError::Custom(format!("Internal unrecoverable error: {e}"))
            }
            RpcRequestError::ServerWorkQueueFull => FinalisedStateError::Custom(
                "Server queue full. Handling for this not yet implemented".to_string(),
            ),
            RpcRequestError::Method(e) => FinalisedStateError::Custom(format!(
                "unhandled rpc-specific {} error: {}",
                type_name::<T>(),
                e.to_string()
            )),
            RpcRequestError::UnexpectedErrorResponse(error) => {
                FinalisedStateError::Custom(format!(
                    "unhandled rpc-specific {} error: {}",
                    type_name::<T>(),
                    error
                ))
            }
        }
    }
}

/// Errors related to the `FinalisedState`.
// TODO: Update name to DbError when ZainoDB replaces legacy finalised state.
#[derive(Debug, thiserror::Error)]
pub enum FinalisedStateError {
    /// Custom Errors.
    // TODO: Remove before production
    #[error("Custom error: {0}")]
    Custom(String),

    /// Requested data is missing from the finalised state.
    ///
    /// This could be due to the databae not yet being synced or due to a bad request input.
    ///
    /// We could split this into 2 distinct types if needed.
    #[error("Missing data: {0}")]
    DataUnavailable(String),

    /// A block is present on disk but failed internal validation.
    ///
    /// *Typically means: checksum mismatch, corrupt CBOR, Merkle check
    /// failed, etc.*  The caller should fetch the correct data and
    /// overwrite the faulty block.
    #[error("invalid block @ height {height} (hash {hash}): {reason}")]
    InvalidBlock {
        height: u32,
        hash: BlockHash,
        reason: String,
    },

    /// Returned when a caller asks for a feature that the
    /// currently-opened database version does not advertise.
    #[error("feature unavailable: {0}")]
    FeatureUnavailable(&'static str),

    /// Errors originating from the BlockchainSource in use.
    #[error("blockchain source error: {0}")]
    BlockchainSourceError(#[from] crate::chain_index::source::BlockchainSourceError),

    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// Error from the LMDB database.
    // NOTE: Should this error type be here or should we handle all LMDB errors internally?
    #[error("LMDB database error: {0}")]
    LmdbError(#[from] lmdb::Error),

    /// Serde Json serialisation / deserialisation errors.
    // TODO: Remove when ZainoDB replaces legacy finalised state.
    #[error("LMDB database error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),

    /// Error from JsonRpcConnector.
    // TODO: Remove when ZainoDB replaces legacy finalised state.
    #[error("JsonRpcConnector error: {0}")]
    JsonRpcConnectorError(#[from] zaino_fetch::jsonrpsee::error::TransportError),

    /// std::io::Error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// A general error type to represent error StatusTypes.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Unexpected status error: {server_status:?}")]
pub struct StatusError {
    pub server_status: crate::status::StatusType,
}

#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
/// The set of errors that can occur during the public API calls
/// of a NodeBackedChainIndex
pub struct ChainIndexError {
    pub(crate) kind: ChainIndexErrorKind,
    pub(crate) message: String,
    pub(crate) source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

#[derive(Debug)]
/// The high-level kinds of thing that can fail
pub enum ChainIndexErrorKind {
    /// Zaino is in some way nonfunctional
    InternalServerError,
    /// The given snapshot contains invalid data.
    // This variant isn't used yet...it should indicate
    // that the provided snapshot contains information unknown to Zebra
    // Unlike an internal server error, generating a new snapshot may solve
    // whatever went wrong
    #[allow(dead_code)]
    InvalidSnapshot,
}

impl Display for ChainIndexErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChainIndexErrorKind::InternalServerError => "internal server error",
            ChainIndexErrorKind::InvalidSnapshot => "invalid snapshot",
        })
    }
}

impl ChainIndexError {
    pub(crate) fn backing_validator(value: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: "InternalServerError: error receiving data from backing node".to_string(),
            source: Some(Box::new(value)),
        }
    }

    pub(crate) fn database_hole(missing_block: impl Display) -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: format!(
                "InternalServerError: hole in validator database, missing block {missing_block}"
            ),
            source: None,
        }
    }

    pub(crate) fn child_process_status_error(process: &str, status_err: StatusError) -> Self {
        use crate::status::StatusType;

        let message = match status_err.server_status {
            StatusType::Spawning => format!("{process} status: Spawning (not ready yet)"),
            StatusType::Syncing => format!("{process} status: Syncing (not ready yet)"),
            StatusType::Ready => format!("{process} status: Ready (unexpected error path)"),
            StatusType::Busy => format!("{process} status: Busy (temporarily unavailable)"),
            StatusType::Closing => format!("{process} status: Closing (shutting down)"),
            StatusType::Offline => format!("{process} status: Offline (not available)"),
            StatusType::RecoverableError => {
                format!("{process} status: RecoverableError (retry may succeed)")
            }
            StatusType::CriticalError => {
                format!("{process} status: CriticalError (requires operator action)")
            }
        };

        ChainIndexError {
            kind: ChainIndexErrorKind::InternalServerError,
            message,
            source: Some(Box::new(status_err)),
        }
    }
}
impl From<FinalisedStateError> for ChainIndexError {
    fn from(value: FinalisedStateError) -> Self {
        let message = match &value {
            FinalisedStateError::DataUnavailable(err) => format!("unhandled missing data: {err}"),
            FinalisedStateError::FeatureUnavailable(err) => {
                format!("unhandled missing feature: {err}")
            }
            FinalisedStateError::InvalidBlock {
                height,
                hash: _,
                reason,
            } => format!("invalid block at height {height}: {reason}"),
            FinalisedStateError::Custom(err) | FinalisedStateError::Critical(err) => err.clone(),
            FinalisedStateError::LmdbError(error) => error.to_string(),
            FinalisedStateError::SerdeJsonError(error) => error.to_string(),
            FinalisedStateError::StatusError(status_error) => status_error.to_string(),
            FinalisedStateError::JsonRpcConnectorError(transport_error) => {
                transport_error.to_string()
            }
            FinalisedStateError::IoError(error) => error.to_string(),
            FinalisedStateError::BlockchainSourceError(blockchain_source_error) => {
                blockchain_source_error.to_string()
            }
        };
        ChainIndexError {
            kind: ChainIndexErrorKind::InternalServerError,
            message,
            source: Some(Box::new(value)),
        }
    }
}

impl From<MempoolError> for ChainIndexError {
    fn from(value: MempoolError) -> Self {
        // Construct a user-facing message depending on the variant
        let message = match &value {
            MempoolError::Critical(msg) => format!("critical mempool error: {msg}"),
            MempoolError::IncorrectChainTip {
                expected_chain_tip,
                current_chain_tip,
            } => {
                format!(
                    "incorrect chain tip (expected {expected_chain_tip:?}, current {current_chain_tip:?})"
                )
            }
            MempoolError::JsonRpcConnectorError(err) => {
                format!("mempool json-rpc connector error: {err}")
            }
            MempoolError::BlockchainSourceError(err) => {
                format!("mempool blockchain source error: {err}")
            }
            MempoolError::WatchRecvError(err) => format!("mempool watch receiver error: {err}"),
            MempoolError::StatusError(status_err) => {
                format!("mempool status error: {status_err:?}")
            }
        };

        ChainIndexError {
            kind: ChainIndexErrorKind::InternalServerError,
            message,
            source: Some(Box::new(value)),
        }
    }
}
