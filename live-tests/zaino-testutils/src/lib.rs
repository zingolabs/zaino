//! Shared helpers for Zaino live tests running on the ztest Kubernetes harness.
//!
//! The harness proper — validator / indexer / wallet lifecycle and the typed
//! RPC handles test code drives — is [`ztest`]; this crate adds the small
//! conveniences the ported live tests share: JSON-RPC parity assertions
//! (`rpc`/`json`), the address constants the validate-address parity tests
//! use, and the shielded funding-pool constant.

#![forbid(unsafe_code)]

pub use ztest;
pub use ztest::prelude::*;

pub mod json;
pub mod rpc;

pub use json::{assert_json_equal_ignoring, assert_json_shape_matches};
pub use rpc::assert_rpc_parity;

/// Generate one discoverable `#[tokio::test]` per `name => helper` pair, each
/// building a fresh validator from the `$ctor` expression (e.g.
/// `Validator::zebrad("6.2.0")`, re-evaluated per test) and awaiting the
/// helper. `$ctor` and `ztest` resolve in the invoking module's scope.
#[macro_export]
macro_rules! validator_tests {
    ($ctor:expr, $( $name:ident => $helper:path ),* $(,)?) => {
        $(
            #[ztest::qos::integration]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn $name() -> ::anyhow::Result<()> {
                $helper($ctor).await
            }
        )*
    };
}
