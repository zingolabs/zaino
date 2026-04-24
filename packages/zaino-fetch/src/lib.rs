//! A mempool-fetching, chain-fetching and transaction submission service that uses zcashd's JsonRPC interface.
//!
//! Usable as a backwards-compatible, legacy option.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod jsonrpsee;
