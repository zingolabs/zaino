//! Zaino source — driven port traits for validator access.
//!
//! One trait per question a consumer can ask about the chain.
//! Implementations (adapters) bridge to a specific transport
//! (JSON-RPC, Zebra ReadState, mock).
//!
//! Consumers compose traits via bounds:
//! ```ignore
//! fn sync<V: GetBlockBytes + GetChainTip>(validator: &V) { ... }
//! ```

mod error;
mod get_block_bytes;
mod get_chain_tip;
mod get_treestate;

pub use error::{QueryError, TransportError};
pub use get_block_bytes::{GetBlockBytes, GetBlockBytesError};
pub use get_chain_tip::{GetChainTip, GetChainTipError};
pub use get_treestate::{GetTreestate, GetTreestateError, TreestateResponse};
