#![allow(deprecated)]
//! Holds error types for Zaino-state.

// Needs to be module level due to the thiserror::Error macro

use crate::BlockHash;

use std::fmt::Display;

use zaino_proto::proto::utils::GetBlockRangeError;

/// A rejection carrying a legacy-compatible legacy RPC error code.
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
    /// The legacy full-node legacy error code.
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

    /// A rejection carrying a legacy-compatible legacy RPC error code.
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
                // `unimplemented` rather than `unavailable`: this deployment
                // does not build the index, so there is no later attempt that
                // succeeds and a retryable status would only cost the caller
                // one.
                ChainIndexErrorKind::Unimplemented => tonic::Status::unimplemented(err.message),
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

/// Errors from the finalised chain store.
///
/// Not defined here: it belongs to the backend that produces it, and is
/// re-exported under its old name so this crate's callers do not have to be
/// rewritten while they are being moved onto
/// [`zaino_chain_store::ChainStoreError`], which is what the ports return and
/// what a consumer should eventually see.
pub use zaino_chain_store_zainodb::error::{StatusError, StoreError as FinalisedStateError};

#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
/// The set of errors that can occur during the public API calls
/// of a NodeBackedChainIndex
pub struct ChainIndexError {
    pub(crate) kind: ChainIndexErrorKind,
    pub(crate) message: String,
    pub(crate) source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
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
    /// The caller asked for something this deployment does not offer at all —
    /// a query needing an index it is not configured to build.
    ///
    /// Distinct from `Unavailable`, which says "not right now": there is no
    /// later attempt that succeeds, so telling the caller to retry wastes it.
    /// Distinct from `InternalServerError`: nothing is broken, and the request
    /// was well-formed.
    Unimplemented,
}

impl Display for ChainIndexErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChainIndexErrorKind::InternalServerError => "internal server error",
            ChainIndexErrorKind::InvalidSnapshot => "invalid snapshot",
            ChainIndexErrorKind::Unavailable => "unavailable",
            ChainIndexErrorKind::InvalidArgument => "invalid argument",
            ChainIndexErrorKind::Unimplemented => "unimplemented",
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
}

/// A chain-head query that could not be answered.
impl From<zaino_chain_head::ChainHeadError> for ChainIndexError {
    fn from(value: zaino_chain_head::ChainHeadError) -> Self {
        ChainIndexError::internal(format!("chain head query failed: {value}"))
    }
}

/// A chain-head block that cannot be expressed in this crate's shape means the
/// two disagree about a block both are holding — an internal inconsistency,
/// not anything the caller did.
impl From<crate::chain_index::chain_head::ChainHeadConversionError> for ChainIndexError {
    fn from(value: crate::chain_index::chain_head::ChainHeadConversionError) -> Self {
        ChainIndexError::internal(format!("chain head block is unusable: {value}"))
    }
}

/// An error occurred during a ChainIndex sync iteration.
///
/// One variant, because the loop now drives one thing: the finalised state.
/// Whatever goes wrong — an unreachable validator, a database that will not
/// advance — the worker's answer is the same, to back off and retry, and to
/// escalate only after a run of them.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The sync iteration failed. Retryable.
    #[error("sync iteration failed: {0}")]
    ErrorFromSource(Box<dyn std::error::Error + Send>),
}

/// An error occurred while constructing a ChainIndex.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    /// The connected node returned data that could not be used.
    #[error("validator returned invalid data: {0}")]
    InvalidNodeData(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The mempool failed to initialise.
    #[error(transparent)]
    MempoolInitialzationError(#[from] crate::error::MempoolError),
    /// The finalised state failed to initialise.
    #[error(transparent)]
    FinalisedStateInitialzationError(#[from] FinalisedStateError),
    /// The chain head could not build its first window.
    ///
    /// Fatal by design: a chain head with no window has nothing to serve, and
    /// it holds no persistent state to fall back on.
    #[error(transparent)]
    ChainHeadInitialisationError(#[from] zaino_chain_head_service::ChainHeadInitError),
}

impl From<FinalisedStateError> for ChainIndexError {
    fn from(value: FinalisedStateError) -> Self {
        let message = match &value {
            FinalisedStateError::DataUnavailable(err) => format!("unhandled missing data: {err}"),
            FinalisedStateError::FeatureUnavailable(err) => {
                format!("unhandled missing feature: {err}")
            }
            FinalisedStateError::V1BackendUnavailable(handle) => {
                format!("v1 backend unavailable: {handle}")
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
            FinalisedStateError::Source(source_error) => source_error.to_string(),
        };
        ChainIndexError {
            kind: ChainIndexErrorKind::InternalServerError,
            message,
            source: Some(Box::new(value)),
        }
    }
}

/// A chain-store query that could not be answered.
///
/// Each variant is named, and the match is exhaustive on purpose. Routing by
/// exclusion — one arm for the retryable case and a catch-all for everything
/// else — reads as a decision but is the absence of one: every variant the
/// author did not think about becomes an internal server error, and so does
/// every variant added afterwards, without anything failing to compile. A
/// caller fault and a broken database are not the same answer, and the type
/// already distinguishes them.
///
/// [`ChainStoreError::AboveWatermark`] maps to internal deliberately. It is not
/// an error a caller should ever see — it means the finalised half was asked
/// about a height the recent half owns, which is a routing mistake in
/// ChainIndex rather than anything the caller did. The read helpers in
/// [`chain_store`](crate::chain_index::chain_store) turn it into "not here"
/// before it reaches this conversion; reaching it means one did not.
impl From<zaino_chain_store::ChainStoreError> for ChainIndexError {
    fn from(value: zaino_chain_store::ChainStoreError) -> Self {
        use zaino_chain_store::ChainStoreError as Error;

        let kind = match &value {
            // Transient. It resolves once the store finishes opening, so the
            // caller is told to come back.
            Error::NotReady => ChainIndexErrorKind::Unavailable,

            // The caller handed over a range that runs backwards. Nothing is
            // broken and retrying it unchanged fails identically.
            Error::InvalidRange { .. } => ChainIndexErrorKind::InvalidArgument,

            // The store is healthy and the request well-formed; this
            // deployment does not build that index. Not a fault, and not
            // something a later attempt fixes.
            Error::Unavailable(_) => ChainIndexErrorKind::Unimplemented,

            // ChainIndex's own routing mistake, as the note above explains.
            Error::AboveWatermark { .. } => ChainIndexErrorKind::InternalServerError,

            // A broken store. Neither is the caller's to fix.
            Error::MissingRow(_) | Error::CorruptRow { .. } | Error::Backend { .. } => {
                ChainIndexErrorKind::InternalServerError
            }
        };

        ChainIndexError {
            kind,
            message: value.to_string(),
            // Kept in every arm: the typed store error is what an operator
            // needs, and the kind above says nothing about which LMDB failure
            // produced it.
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

#[cfg(test)]
mod tests {
    use super::{ChainIndexError, ChainIndexErrorKind};
    use zaino_chain_store::{ChainStoreError, StoreCapability};
    use zaino_primitives::types::Height;

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("a valid height")
    }

    fn kind_of(error: ChainStoreError) -> ChainIndexErrorKind {
        ChainIndexError::from(error).kind()
    }

    /// A caller fault is reported as one, not as a server fault.
    ///
    /// A backwards range is the caller's doing and retrying it unchanged fails
    /// identically. The catch-all this replaces called it an internal server
    /// error, which tells the caller Zaino is broken and invites exactly that
    /// futile retry.
    #[test]
    fn a_backwards_range_is_the_callers_fault() {
        assert_eq!(
            kind_of(ChainStoreError::InvalidRange {
                start: h(2),
                end: h(1)
            }),
            ChainIndexErrorKind::InvalidArgument
        );
    }

    /// An index this deployment does not build is not a fault and not retryable.
    #[test]
    fn an_unbuilt_index_is_neither_a_fault_nor_retryable() {
        assert_eq!(
            kind_of(ChainStoreError::Unavailable(StoreCapability::TxOutSet)),
            ChainIndexErrorKind::Unimplemented
        );
    }

    /// A store that has not opened yet is retryable.
    #[test]
    fn a_store_still_opening_is_retryable() {
        assert_eq!(
            kind_of(ChainStoreError::NotReady),
            ChainIndexErrorKind::Unavailable
        );
    }

    /// A broken store is a server fault, and so is a routing mistake.
    #[test]
    fn a_broken_store_is_a_server_fault() {
        for error in [
            ChainStoreError::MissingRow("txid".into()),
            ChainStoreError::corrupt_row("in-range height"),
            ChainStoreError::backend("lmdb"),
            ChainStoreError::AboveWatermark {
                requested: h(2),
                watermark: h(1),
            },
        ] {
            assert_eq!(kind_of(error), ChainIndexErrorKind::InternalServerError);
        }
    }

    /// Every conversion keeps the store's typed error as its source.
    ///
    /// The kind says how to respond; it says nothing about which failure
    /// produced it, and that is the part an operator needs.
    #[test]
    fn a_converted_store_error_keeps_its_source() {
        use std::error::Error as _;

        for error in [
            ChainStoreError::NotReady,
            ChainStoreError::Unavailable(StoreCapability::TxOutSet),
            ChainStoreError::backend("lmdb"),
        ] {
            assert!(ChainIndexError::from(error).source().is_some());
        }
    }
}
