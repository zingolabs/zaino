//! Transaction parsing and validation.
//!
//! This module provides schema-driven, order-validated parsing
//! of Zcash transactions across all versions.
//!
//! # Backward Compatibility
//!
//! For compatibility with existing code, this module provides a `FullTransaction`
//! type that wraps the new `Transaction` enum and maintains the old API.

pub use context::{BlockContext, TransactionContext, TxId, ActivationHeights};
pub use reader::{TransactionField, FieldReader, FieldSize};
pub use version_reader::{TransactionVersionReader, peek_transaction_version};
pub use dispatcher::{Transaction, TransactionDispatcher, TransactionVersionSpecific, V4Fields};

// Full transaction exports
pub use full_transaction::FullTransaction;

// Re-export commonly used field types
pub use fields::{TxIn, TxOut};

// Re-export version-specific types
pub use versions::v1::{TransactionV1, RawTransactionV1, TransactionV1Reader};
pub use versions::v4::{TransactionV4, RawTransactionV4, TransactionV4Reader};

pub mod context;
pub mod reader;
pub mod version_reader;
pub mod fields;
pub mod dispatcher;
pub mod full_transaction;

pub mod versions {
    pub mod v1;
    pub mod v4;
}