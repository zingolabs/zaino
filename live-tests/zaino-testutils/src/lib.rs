//! Shared helpers for Zaino live tests running on the ztest Kubernetes harness.
//!
//! The harness proper — validator / indexer / wallet lifecycle and the typed
//! RPC handles test code drives — is [`ztest`]; this crate adds the small
//! conveniences the live tests share: hex conversion across the JSON-RPC / gRPC
//! boundary (`hex`) and gating on the finalised index (`finalised`).

#![forbid(unsafe_code)]

pub use ztest;
pub use ztest::prelude::*;

pub mod finalised;
pub mod hex;

pub use finalised::wait_for_finalised;
