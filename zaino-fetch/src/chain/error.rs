//! Hold error types for the BlockCache and related functionality.

/// Parser Error Type.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// Io Error.
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid Data Error
    #[error("Invalid Data Error: {0}")]
    InvalidData(String),

    /// UTF-8 conversion error.
    #[error("UTF-8 Error: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),

    /// UTF-8 conversion error.
    #[error("UTF-8 Conversion Error: {0}")]
    FromUtf8Error(#[from] std::string::FromUtf8Error),

    /// Hexadecimal parsing error.
    #[error("Hex Parse Error: {0}")]
    ParseIntError(#[from] std::num::ParseIntError),

    /// Errors originating from prost decodings.
    #[error("Prost Decode Error: {0}")]
    ProstDecodeError(#[from] prost::DecodeError),

    /// Integer conversion error.
    #[error("Integer conversion error: {0}")]
    TryFromIntError(#[from] std::num::TryFromIntError),

    /// Field order violation during parsing.
    #[error("Field order violation: expected position {expected}, got {actual} for field {field}. Fields read so far: {fields_read:?}")]
    FieldOrderViolation {
        expected: usize,
        actual: usize,
        field: &'static str,
        fields_read: Vec<&'static str>,
    },

    /// Field size mismatch during parsing.
    #[error("Field size mismatch for {field}: expected {expected} bytes, consumed {actual} bytes")]
    FieldSizeMismatch {
        field: &'static str,
        expected: usize,
        actual: usize,
    },

    /// Unsupported transaction version.
    #[error("Unsupported transaction version: {version}")]
    UnsupportedVersion { version: u32 },

    /// Missing transaction ID.
    #[error("Missing transaction ID for transaction at index {tx_index}")]
    MissingTxId { tx_index: usize },

    /// Validation error during parsing.
    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),
}

/// Validation errors for transaction parsing
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    /// Invalid version group ID.
    #[error("Invalid version group ID: expected {expected:#x}, found {found:#x}")]
    InvalidVersionGroupId { expected: u32, found: u32 },

    /// Empty transaction (no inputs or outputs).
    #[error("Transaction cannot be empty (no inputs and no outputs)")]
    EmptyTransaction,

    /// Version 1 transaction cannot have overwinter flag.
    #[error("Version 1 transaction cannot be overwinter")]
    V1CannotBeOverwinter,

    /// Coinbase transaction cannot be empty.
    #[error("Coinbase transaction cannot have empty inputs")]
    CoinbaseCannotBeEmpty,

    /// Version group ID not supported for this version.
    #[error("Version group ID validation not supported for this transaction version")]
    VersionGroupIdNotSupported,

    /// Sapling spend/output count mismatch.
    #[error("Sapling spend and output counts must match or both be zero")]
    SaplingSpendOutputMismatch,

    /// Value overflow in transaction.
    #[error("Value overflow in transaction calculations")]
    ValueOverflow,

    /// Field version mismatch.
    #[error("Field {field} version mismatch: version {version}, required range {required_range:?}")]
    FieldVersionMismatch {
        field: &'static str,
        version: u32,
        required_range: (u32, u32),
    },

    /// Field block height mismatch.
    #[error("Field {field} block height mismatch: height {height}, required range {required_range:?}")]
    FieldBlockHeightMismatch {
        field: &'static str,
        height: u32,
        required_range: (u32, u32),
    },

    /// Field not active at current height.
    #[error("Field {field} not active at height {height}, activation height: {activation_height}")]
    FieldNotActiveAtHeight {
        field: &'static str,
        height: u32,
        activation_height: u32,
    },

    /// Generic validation error.
    #[error("Validation error: {message}")]
    Generic { message: String },
}
