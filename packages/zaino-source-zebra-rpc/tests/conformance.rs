//! Live conformance of the RPC source against the source-port conformance kit.
//!
//! [`ZebraRpcAdapter`] wrapped in [`ValidatorClient`] is put through
//! [`assert_chain_source_conformance`] (tip-consistent + contiguous-and-linked +
//! above-tip-is-a-domain-miss) and [`assert_follows_to_tip`] (the RPC path polls
//! the tip, so it *does* follow the chain). The conformance kit was previously
//! only ever exercised against a mock; this is the first time a real adapter is
//! run through it.
//!
//! These need a running Zebra JSON-RPC, so they are `#[ignore]`d. Point them at a
//! node and run explicitly:
//!
//! ```text
//! ZAINO_TEST_ZEBRA_JSONRPC=127.0.0.1:8232 \
//!   cargo test -p zaino-source-zebra-rpc --test conformance -- --ignored --nocapture
//! ```
//!
//! The static battery works against any synced node; `follows_to_tip` needs the
//! chain to advance within the timeout, so run that arm against a regtest node
//! you can mine on.
//!
//! Deliberately *not* covered here: the finalised-only ReadState adapter. It is a
//! boot-time snapshot sub-source that by design fails `assert_follows_to_tip` (it
//! never advances past its finalised tip) — conforming it standalone would be
//! wrong. The `ZebraValidator` *composite* (readstate ⊕ rpc) is what should
//! conform end-to-end; this test covers its RPC arm, the one that must serve the
//! volatile top.

use std::time::Duration;

use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::conformance::{assert_chain_source_conformance, assert_follows_to_tip};
use zaino_source::{GetChainTip, RetryPolicy, ValidatorClient};
use zaino_source_zebra_rpc::ZebraRpcAdapter;

/// A resilient RPC source over the validator at `ZAINO_TEST_ZEBRA_JSONRPC`
/// (default local mainnet port). `ValidatorClient` supplies the canonical
/// (retrying) `GetChainTip`/`GetBlock`/`GetBlockByHash` the conformance kit binds.
fn source() -> ValidatorClient<ZebraRpcAdapter> {
    let addr =
        std::env::var("ZAINO_TEST_ZEBRA_JSONRPC").unwrap_or_else(|_| "127.0.0.1:8232".to_string());
    let rpc = RpcClient::new(RpcClientConfig {
        url: format!("http://{addr}"),
        ..RpcClientConfig::default()
    })
    .expect("rpc client config is valid");
    ValidatorClient::new(ZebraRpcAdapter::new(rpc), RetryPolicy::default())
}

#[tokio::test]
#[ignore = "needs a running Zebra JSON-RPC (set ZAINO_TEST_ZEBRA_JSONRPC)"]
async fn rpc_adapter_conforms_to_the_chain_source_contract() {
    assert_chain_source_conformance(&source()).await;
}

#[tokio::test]
#[ignore = "needs a Zebra whose chain advances within the timeout (regtest: mine first)"]
async fn rpc_adapter_follows_to_tip() {
    let source = source();
    // The tip observed now; a polling source must reach at least it, and the range
    // up to it must be contiguous and linked. On a live chain the tip only
    // advances, so this is a lower bound a follower trivially meets.
    let (_, tip) = source.get_chain_tip().await.expect("source reports a tip");
    assert_follows_to_tip(&source, tip, Duration::from_secs(30)).await;
}
