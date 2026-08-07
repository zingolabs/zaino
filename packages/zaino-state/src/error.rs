#![allow(deprecated)]
//! Holds error types for Zaino-state.

// Needs to be module level due to the thiserror::Error macro

use crate::BlockHash;

use std::fmt::Display;

use zaino_proto::proto::utils::GetBlockRangeError;

/// A rejection carrying a zcashd-compatible legacy RPC error code.
///
/// Zaino's *own* rejections — a malformed block identifier, an oversized raw
/// transaction — that must reach a client as the specific legacy code
/// lightwalletd-family clients key on, rather than a generic internal error.
/// Carried as a typed `source` through the error chain so the serving layer can
/// downcast to it — `ChainIndexError::internal_from` is what preserves it on
/// the way out, and `legacy_code_from_error_source` in
/// `zaino-serve/src/rpc/jsonrpc/service.rs` is what recovers it.
///
/// Distinct from [`FetchError`](zaino_source::FetchError), which carries a code
/// the *validator* produced. This one is Zaino's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct LegacyRpcError {
    /// The zcashd legacy error code.
    pub code: i64,
    /// Human-readable description.
    pub message: String,
}

impl LegacyRpcError {
    /// Construct a rejection from zebra's `LegacyCode` enum.
    pub fn new(code: zebra_rpc::server::error::LegacyCode, message: impl Into<String>) -> Self {
        Self {
            code: code as i64,
            message: message.into(),
        }
    }
}

/// Errors returned by the [`NodeBackedIndexerService`](crate::NodeBackedIndexerService)
/// subscriber's `ZcashIndexer` / `LightWalletIndexer` methods.
#[derive(Debug, thiserror::Error)]
#[allow(clippy::result_large_err)]
pub enum NodeBackedIndexerServiceError {
    /// Critical Errors, Restart Zaino.
    #[error("Critical error: {0}")]
    Critical(String),

    /// An rpc-specific error we haven't accounted for
    #[error("unhandled fallible RPC call {0}")]
    UnhandledRpcError(String),
    /// Custom Errors. *Remove before production.
    #[error("Custom error: {0}")]
    Custom(String),

    /// Error from a Tokio JoinHandle.
    #[error("Join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    /// A rejection carrying a zcashd-compatible legacy RPC error code.
    #[error("RPC error: {0:?}")]
    RpcError(#[from] LegacyRpcError),

    /// Chain index error.
    #[error("Chain index error: {0}")]
    ChainIndexError(#[from] ChainIndexError),

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
    #[error("zaino not yet synced")]
    /// Zaino has not yet synced.
    UnavailableNotSyncedEnough,
}

impl From<GetBlockRangeError> for NodeBackedIndexerServiceError {
    fn from(value: GetBlockRangeError) -> Self {
        match value {
            GetBlockRangeError::StartHeightOutOfRange => {
                Self::TonicStatusError(tonic::Status::out_of_range(
                    "Error: Start height out of range. Failed to convert to u32.",
                ))
            }
            GetBlockRangeError::NoStartHeightProvided => {
                Self::TonicStatusError(tonic::Status::out_of_range("Error: No start height given"))
            }
            GetBlockRangeError::EndHeightOutOfRange => {
                Self::TonicStatusError(tonic::Status::out_of_range(
                    "Error: End height out of range. Failed to convert to u32.",
                ))
            }
            GetBlockRangeError::NoEndHeightProvided => {
                Self::TonicStatusError(tonic::Status::out_of_range("Error: No end height given."))
            }
            GetBlockRangeError::PoolTypeArgumentError(_) => {
                Self::TonicStatusError(tonic::Status::invalid_argument("Error: invalid pool type"))
            }
        }
    }
}

#[allow(deprecated)]
impl From<NodeBackedIndexerServiceError> for tonic::Status {
    fn from(error: NodeBackedIndexerServiceError) -> Self {
        match error {
            NodeBackedIndexerServiceError::Critical(message) => tonic::Status::internal(message),
            NodeBackedIndexerServiceError::Custom(message) => tonic::Status::internal(message),
            NodeBackedIndexerServiceError::JoinError(err) => {
                tonic::Status::internal(format!("Join error: {err}"))
            }
            NodeBackedIndexerServiceError::RpcError(err) => {
                tonic::Status::internal(format!("RPC error: {err:?}"))
            }
            NodeBackedIndexerServiceError::ChainIndexError(err) => match err.kind {
                ChainIndexErrorKind::InternalServerError => tonic::Status::internal(err.message),
                ChainIndexErrorKind::InvalidSnapshot => {
                    tonic::Status::failed_precondition(err.message)
                }
                // `unavailable` rather than `failed_precondition`: gRPC clients
                // treat it as retryable, which is exactly the instruction here.
                ChainIndexErrorKind::Unavailable => tonic::Status::unavailable(err.message),
                ChainIndexErrorKind::InvalidArgument => {
                    tonic::Status::invalid_argument(err.message)
                }
            },
            NodeBackedIndexerServiceError::BlockCacheError(err) => {
                tonic::Status::internal(format!("BlockCache error: {err:?}"))
            }
            NodeBackedIndexerServiceError::MempoolError(err) => {
                tonic::Status::internal(format!("Mempool error: {err:?}"))
            }
            NodeBackedIndexerServiceError::TonicStatusError(err) => err,
            NodeBackedIndexerServiceError::SerializationError(err) => {
                tonic::Status::internal(format!("Serialization error: {err}"))
            }
            NodeBackedIndexerServiceError::TryFromIntError(err) => {
                tonic::Status::internal(format!("Integer conversion error: {err}"))
            }
            NodeBackedIndexerServiceError::IoError(err) => {
                tonic::Status::internal(format!("IO error: {err}"))
            }
            NodeBackedIndexerServiceError::Generic(err) => {
                tonic::Status::internal(format!("Generic error: {err}"))
            }
            ref err @ NodeBackedIndexerServiceError::ZebradVersionMismatch { .. } => {
                tonic::Status::internal(err.to_string())
            }
            NodeBackedIndexerServiceError::UnhandledRpcError(e) => {
                tonic::Status::internal(e.to_string())
            }
            NodeBackedIndexerServiceError::UnavailableNotSyncedEnough => {
                tonic::Status::failed_precondition("zaino not yet synced".to_string())
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

    /// Incorrect expected chain tip given from client.
    #[error(
        "Incorrect chain tip (expected {expected_chain_tip:?}, current {current_chain_tip:?})"
    )]
    IncorrectChainTip {
        expected_chain_tip: BlockHash,
        current_chain_tip: BlockHash,
    },

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

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),
}

/// Errors related to the `FinalisedState`.
// TODO: Update name to DbError when FinalisedState replaces legacy finalised state.
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
    // TODO: Remove when FinalisedState replaces legacy finalised state.
    #[error("LMDB database error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    /// Unexpected status-related error.
    #[error("Status error: {0:?}")]
    StatusError(StatusError),

    /// std::io::Error
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// A general error type to represent error StatusTypes.
#[derive(Debug, Clone, thiserror::Error)]
#[error("Unexpected status error: {server_status:?}")]
pub struct StatusError {
    pub server_status: zaino_status::StatusType,
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

#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
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
    /// The caller asked for something Zaino cannot serve *right now*, but
    /// could on a later attempt — a mempool read against a snapshot the
    /// mempool has since moved past, most often.
    ///
    /// Distinct from `InvalidSnapshot`: nothing about the request was wrong,
    /// and distinct from `InternalServerError`: nothing is broken. Retrying
    /// with a fresh snapshot is the correct response, and only a retryable
    /// status tells the caller so.
    Unavailable,
    /// The caller's request was malformed — an over-long exclude list, a txid
    /// suffix too short to identify anything. Retrying it unchanged will fail
    /// the same way.
    InvalidArgument,
}

impl Display for ChainIndexErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChainIndexErrorKind::InternalServerError => "internal server error",
            ChainIndexErrorKind::InvalidSnapshot => "invalid snapshot",
            ChainIndexErrorKind::Unavailable => "unavailable",
            ChainIndexErrorKind::InvalidArgument => "invalid argument",
        })
    }
}

impl ChainIndexError {
    /// The error kind
    pub fn kind(&self) -> ChainIndexErrorKind {
        self.kind
    }
    /// Constructs an `InternalServerError`-kind error with a free-form message.
    ///
    /// Intended for call sites that produce a string error and have no underlying
    /// `std::error::Error` source to attach.
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: message.into(),
            source: None,
        }
    }

    /// Constructs an `InternalServerError`-kind error from a typed error,
    /// preserving it as `source` so zaino-serve's RPC-error-code recovery
    /// walks can downcast to it (e.g. the legacy `-8` code carried by an
    /// [`LegacyRpcError`]).
    pub(crate) fn internal_from(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: error.to_string(),
            source: Some(Box::new(error)),
        }
    }

    /// Constructs an `Unavailable`-kind error: the request was fine, Zaino
    /// simply cannot serve it from its current view. Tells the caller to retry.
    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ChainIndexErrorKind::Unavailable,
            message: message.into(),
            source: None,
        }
    }

    /// Constructs an `InvalidArgument`-kind error: the request itself is wrong,
    /// and retrying it unchanged will fail identically.
    pub(crate) fn invalid_argument(message: impl Into<String>) -> Self {
        Self {
            kind: ChainIndexErrorKind::InvalidArgument,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn backing_validator(value: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: "InternalServerError: error receiving data from backing node".to_string(),
            source: Some(Box::new(value)),
        }
    }

    pub(crate) fn database_hole(
        missing_block: impl Display,
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: format!(
                "InternalServerError: hole in validator database, missing block {missing_block}"
            ),
            source,
        }
    }

    pub(crate) fn validator_data_error_block_coinbase_height_missing() -> Self {
        Self {
            kind: ChainIndexErrorKind::InternalServerError,
            message: "validator error: data error: block.coinbase_height() returned None"
                .to_string(),
            source: None,
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
