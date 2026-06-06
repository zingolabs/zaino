//! A mempool-fetching, chain-fetching and transaction submission service that uses zcashd's JsonRPC interface.
//!
//! Usable as a backwards-compatible, legacy option.

#![warn(missing_docs)]
// `u32::is_multiple_of` stabilised in 1.88; keep `% == 0` to avoid raising MSRV.
#![allow(clippy::manual_is_multiple_of)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod jsonrpsee;
