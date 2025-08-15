//! Block parsing and validation.
//!
//! This module provides schema-driven, order-validated parsing of Zcash blocks
//! with proper separation between block context and transaction context.
//!
//! # Architecture
//!
//! The block parsing system eliminates the anti-patterns found in the old
//! ParseFromSlice trait by:
//!
//! - **Clean Context Separation**: Block context doesn't bleed into transaction parsing
//! - **Field Order Validation**: Block header fields must be read in the correct order
//! - **Type-Safe Field Access**: Each field type knows its size and validation rules
//! - **No Boolean Traps**: Different parsers don't share incompatible parameters
//!
//! # Usage
//!
//! ```rust
//! use zaino_fetch::chain::block::{BlockParser, BlockParsingContext};
//! use zebra_chain::parameters::Network;
//!
//! // Create parsing context (no transaction IDs as parameters!)
//! let context = BlockParsingContext::new(Network::Mainnet)
//!     .with_height(100000)
//!     .with_tx_count(5);
//!
//! // Transaction IDs come from external source (like RPC response)
//! let txids = vec![[1u8; 32], [2u8; 32], [3u8; 32], [4u8; 32], [5u8; 32]];
//!
//! // Parse block with clean separation of concerns
//! let block = BlockParser::parse_with_txids(block_data, context, txids)?;
//! ```

pub use context::{BlockParsingContext, BlockTransactionContext};
pub use reader::{BlockField, BlockFieldReader, BlockFieldSize};
pub use fields::{
    BlockVersion, PreviousBlockHash, MerkleRoot, FinalSaplingRoot,
    BlockTime, NBits, BlockNonce, EquihashSolution, TransactionCount
};
pub use parser::{
    BlockHeader, RawBlockHeader, BlockHeaderParser,
    Block, RawBlock, BlockParser
};

// Backward compatibility exports
pub use backward_compat::{FullBlock, BlockHeaderData};

pub mod context;
pub mod reader;
pub mod fields;
pub mod parser;
pub mod backward_compat;