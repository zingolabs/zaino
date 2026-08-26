//! Shared helpers for Zaino live tests running on the ztest Kubernetes harness.
//!
//! The harness proper — validator / indexer / wallet lifecycle and the typed
//! RPC handles test code drives — is [`ztest`]; this crate adds the small
//! conveniences the live tests share: JSON-RPC parity assertions
//! (`rpc`/`json`), hex conversion across the JSON-RPC / gRPC boundary
//! (`hex`), gating on the finalised index (`finalised`), and the independent
//! second transaction parser (`legacy_parser`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use ztest;
pub use ztest::prelude::*;

pub mod legacy_parser;

pub mod finalised;
pub mod hex;
pub mod json;
pub mod rpc;

pub use finalised::wait_for_finalised;
pub use json::{
    assert_json_equal_ignoring, assert_json_shape_matches, json_equal_ignoring, json_shape_matches,
    sort_json_array,
};
pub use rpc::assert_rpc_parity;
