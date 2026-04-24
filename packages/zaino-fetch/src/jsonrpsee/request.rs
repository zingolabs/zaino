//! Request parameter types for jsonRPSeeConnector.
//!
//! These types model the inputs accepted by Zebra's JSON-RPC endpoints, kept
//! separate from [`crate::jsonrpsee::response`] so request and response
//! shapes do not end up colocated under a single module name.

pub mod block_selector;
