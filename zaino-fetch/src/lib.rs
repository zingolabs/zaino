//! A mempool-fetching, chain-fetching and transaction submission service that uses zebra's RPC interface.
//!
//! Used primarily as a backup and legacy option for backwards compatibility.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod jsonrpc;
