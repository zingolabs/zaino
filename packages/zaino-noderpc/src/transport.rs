//! JSON-RPC transport: a real jsonrpsee server exposed as a [`RunLoop`].
//!
//! Holds a [`NodeRpc`] handler and binds a jsonrpsee server over its
//! [`NodeRpcApiServer`](crate::rpc::NodeRpcApiServer) surface. Implements
//! [`RunLoop`] so the runtime supervises it as a component: `build` binds the
//! socket (a bind failure is the `RunLoop::Error`, not swallowed), then it serves
//! until the cancellation token fires.

use std::net::SocketAddr;
use std::sync::Arc;

use jsonrpsee::server::ServerBuilder;
use tower::layer::util::{Identity, Stack};
use tower::ServiceBuilder;
use zaino_component::{CancellationToken, Lifecycle, RunLoop, RunReporter};
use zaino_consensus::MAX_BLOCK_BYTES;
use zaino_service::NodeRpcService;

use crate::rpc::NodeRpcApiServer;
use crate::transport::dialect::ZcashdDialectLayer;
use crate::NodeRpc;

mod dialect;

/// The maximum request body this server accepts.
///
/// The largest request the explorer sends is `sendrawtransaction` of a large
/// transaction, so the limit is two maximum blocks plus header slack — the
/// bound legacy zaino used. Computed from the [`u64`] protocol constant and
/// saturated into `usize`: on every supported (64-bit) target the product is
/// exact, and the saturating fallback keeps a hypothetical 32-bit build to a
/// safe ceiling rather than overflowing.
fn max_request_body_size() -> usize {
    let bytes = MAX_BLOCK_BYTES.saturating_mul(2).saturating_add(1024);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// The HTTP middleware stack installed on the jsonrpsee server: the
/// zcashd-dialect bridge. Shared by [`JsonRpcServer::run`] and the tests so both
/// exercise the same wiring.
fn dialect_middleware() -> ServiceBuilder<Stack<ZcashdDialectLayer, Identity>> {
    ServiceBuilder::new().layer(ZcashdDialectLayer::new(max_request_body_size()))
}

/// A jsonrpsee server over a [`NodeRpc`] handler.
pub struct JsonRpcServer<S: NodeRpcService + Clone + 'static> {
    handler: NodeRpc<S>,
    bind: SocketAddr,
}

/// Why the JSON-RPC server could not start.
#[derive(Debug, thiserror::Error)]
pub enum JsonRpcServeError {
    /// The server could not bind / build on the configured address.
    #[error("failed to start JSON-RPC server: {0}")]
    Start(String),
}

impl<S: NodeRpcService + Clone + 'static> JsonRpcServer<S> {
    /// A server exposing `handler`'s node-RPC surface, bound to `bind`.
    pub fn new(handler: NodeRpc<S>, bind: SocketAddr) -> Self {
        Self { handler, bind }
    }
}

impl<S: NodeRpcService + Clone + 'static> RunLoop for JsonRpcServer<S> {
    type Error = JsonRpcServeError;
    const LABEL: &'static str = "serve loop";
    const RUNNING: Lifecycle = Lifecycle::Spawning;

    async fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> Result<(), JsonRpcServeError> {
        // build() binds the socket, so a bind failure surfaces here as the
        // error rather than being swallowed inside the serve loop.
        let server = ServerBuilder::default()
            .set_http_middleware(dialect_middleware())
            .build(self.bind)
            .await
            .map_err(|e| JsonRpcServeError::Start(e.to_string()))?;
        // Bound — safe to report Ready.
        reporter.ready();
        let handle = server.start(self.handler.clone().into_rpc());
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = handle.stop();
            }
            _ = handle.clone().stopped() => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::server::ServerHandle;
    use serde_json::Value;
    use std::net::{SocketAddr, TcpListener};
    use zaino_primitives::types::{BlockHash, BlockRef, Height};
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zcash_protocol::consensus::Network;

    /// A real jsonrpsee server binds and shuts down cleanly when the token fires.
    #[tokio::test]
    async fn binds_and_shuts_down_on_cancel() {
        let handler = NodeRpc::new(
            MockIndexerService::new(MockChain::default()),
            Network::MainNetwork,
        );
        let server = Arc::new(JsonRpcServer::new(
            handler,
            "127.0.0.1:0".parse().expect("valid addr"),
        ));
        let cancel = CancellationToken::new();

        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move { server.run(cancel, RunReporter::new(|_| {})).await }
        });
        // Let the server bind, then ask it to stop.
        tokio::task::yield_now().await;
        cancel.cancel();

        let result = task.await.expect("join serve task");
        assert!(result.is_ok(), "clean shutdown: {result:?}");
    }

    /// Boot a real jsonrpsee server with the production dialect middleware on an
    /// ephemeral port, returning its address and handle. A pre-bound listener
    /// closes the pick-a-port race. The mock chain's tip is height 291, so
    /// `getblockcount` answers 291.
    fn spawn_dialect_server() -> (SocketAddr, ServerHandle) {
        spawn_server(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(291).expect("valid height"),
                hash: BlockHash::from([0xCDu8; 32]),
            }),
            ..Default::default()
        })
    }

    /// Boot a real jsonrpsee server over the given mock chain, with the production
    /// dialect middleware, on an ephemeral port.
    fn spawn_server(chain: MockChain) -> (SocketAddr, ServerHandle) {
        let handler = NodeRpc::new(MockIndexerService::new(chain), Network::MainNetwork);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.set_nonblocking(true).expect("set nonblocking");
        let addr = listener.local_addr().expect("local addr");
        let server = ServerBuilder::default()
            .set_http_middleware(dialect_middleware())
            .build_from_tcp(listener)
            .expect("build server from listener");
        let handle = server.start(handler.into_rpc());
        (addr, handle)
    }

    /// Send a raw HTTP POST to the server with the given content-type, a basic-auth
    /// header (as `zcashex` does), and `body`, returning the HTTP status and the
    /// parsed response body.
    async fn post(
        addr: SocketAddr,
        content_type: &str,
        body: &str,
    ) -> (reqwest::StatusCode, Value) {
        // The workspace builds reqwest with rustls' `rustls-no-provider` feature,
        // so the process crypto provider must be installed before a client is
        // constructed, even for plaintext HTTP (first-install-wins).
        zaino_common::crypto::ensure_default_crypto_provider();
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/"))
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .basic_auth("zcashex", Some("password"))
            .body(body.to_owned())
            .send()
            .await
            .expect("send request");
        let status = response.status();
        let text = response.text().await.expect("read response body");
        let json = serde_json::from_str(&text).unwrap_or_else(|_| {
            panic!("response body is not JSON: {text:?}");
        });
        (status, json)
    }

    // Each HTTP round-trip test drives a live server that accepts a TCP
    // connection concurrently with the reqwest round trip, so the runtime is
    // `multi_thread`: on a current-thread runtime the client future can park
    // while the server's accept loop is starved, deadlocking the test.

    /// The zcashex-shaped 1.0 request (text/plain, basic auth) calling
    /// `getblockcount` gives 200 and the exact legacy envelope, with `error: null`
    /// present — the key zcashex strict-matches.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getblockcount_succeeds() {
        let (addr, handle) = spawn_dialect_server();
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getblockcount","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert_eq!(obj.get("id").and_then(Value::as_str), Some("zcashex"));
        assert_eq!(obj.get("result").and_then(Value::as_u64), Some(291));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// A 1.0 request for an unknown method gives 404, with an `error.message` and
    /// `result: null`.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_point_zero_unknown_method_is_not_found() {
        let (addr, handle) = spawn_dialect_server();
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"nosuchmethod","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
        let obj = body.as_object().expect("a JSON object");
        assert!(
            obj.get("result").is_some_and(Value::is_null),
            "result is present and null: {obj:?}"
        );
        assert!(
            obj.get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .is_some(),
            "the error carries a message: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// A 1.0 request with invalid params gives 500. zcashd maps invalid-request
    /// (-32600) → 400 and method-not-found (-32601) → 404; every other error,
    /// including invalid-params (-32602), is 500.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_point_zero_invalid_params_is_internal_error() {
        let (addr, handle) = spawn_dialect_server();
        // `getblock` requires a block id; an empty params array is invalid params.
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getblock","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        let obj = body.as_object().expect("a JSON object");
        assert!(
            obj.get("result").is_some_and(Value::is_null),
            "result is present and null: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// A Bitcoind no-version request (absent `jsonrpc`) gives 200, with no
    /// `jsonrpc` key, plus `result` and `error: null`.
    #[tokio::test(flavor = "multi_thread")]
    async fn bitcoind_no_version_request_succeeds() {
        let (addr, handle) = spawn_dialect_server();
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"id":1,"method":"getblockcount","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert!(
            !obj.contains_key("jsonrpc"),
            "the bitcoind dialect omits jsonrpc: {obj:?}"
        );
        assert_eq!(obj.get("result").and_then(Value::as_u64), Some(291));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// A 2.0 request is unchanged from today: 200 with no `error` key.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_point_zero_success_is_unchanged() {
        let (addr, handle) = spawn_dialect_server();
        let (status, body) = post(
            addr,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("2.0"));
        assert_eq!(obj.get("result").and_then(Value::as_u64), Some(291));
        assert!(
            !obj.contains_key("error"),
            "a 2.0 success omits the error key: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `getblockhashes` request — the explorer's block-list
    /// call, `[high, low, {noOrphans, logicalTimes}]` — gives 200 and the exact
    /// legacy envelope: `jsonrpc: "1.0"`, the echoed `id`, `error: null`, and a
    /// `result` array of the block's display-order hash string. A real round trip,
    /// end to end through the dialect layer.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getblockhashes_succeeds() {
        use zaino_service::BlockHashAt;
        // One scripted block, at nTime 1_600_000_000, with an asymmetric hash so the
        // display-order render differs from the internal bytes.
        let mut hash_bytes = [0u8; 32];
        hash_bytes[0] = 0x11;
        hash_bytes[31] = 0xaa;
        let expected_display = format!("aa{}11", "00".repeat(30));
        let (addr, handle) = spawn_server(MockChain {
            block_hashes: vec![BlockHashAt {
                height: Height::try_from(100).expect("valid height"),
                hash: BlockHash::from(hash_bytes),
                time: 1_600_000_000,
            }],
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getblockhashes","params":[1600001000,0,{"noOrphans":true,"logicalTimes":false}]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert_eq!(obj.get("id").and_then(Value::as_str), Some("zcashex"));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        assert_eq!(
            obj.get("result"),
            Some(&Value::from(vec![expected_display])),
            "the result is the block hash in display order: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `getaddresstxids` request — a single object param
    /// carrying the addresses and height window — gives 200 and the legacy
    /// envelope, with `result` the scripted txid in display order.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getaddresstxids_succeeds() {
        use zaino_primitives::types::TransactionId;
        let (addr, handle) = spawn_server(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(100).expect("valid height"),
                hash: BlockHash::from([0x11u8; 32]),
            }),
            txids: vec![TransactionId::from([0xABu8; 32])],
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getaddresstxids","params":[{"addresses":["t1abc"],"start":1,"end":100}]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        assert_eq!(
            obj.get("result"),
            Some(&Value::from(vec!["ab".repeat(32)])),
            "the result is the txid in display order: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `getaddressutxos` request — a single `{addresses}`
    /// object param — gives 200 and a `result` array of the scripted output in
    /// zcashd's insight-explorer shape.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getaddressutxos_succeeds() {
        use zaino_primitives::types::{Script, TransactionId, TransparentAddress, Utxo, Zatoshis};
        let (addr, handle) = spawn_server(MockChain {
            tip: Some(BlockRef {
                height: Height::try_from(100).expect("valid height"),
                hash: BlockHash::from([0x11u8; 32]),
            }),
            utxos: vec![Utxo {
                address: TransparentAddress::new("t1abc".to_string()),
                txid: TransactionId::from([0xABu8; 32]),
                output_index: 0,
                script: Script::new(vec![0x51]),
                satoshis: Zatoshis::new(777).expect("valid amount"),
                height: Height::try_from(90).expect("valid height"),
            }],
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getaddressutxos","params":[{"addresses":["t1abc"]}]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        let entry = obj
            .get("result")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .expect("one utxo entry");
        assert_eq!(
            entry.get("txid").and_then(Value::as_str),
            Some("ab".repeat(32).as_str())
        );
        assert_eq!(entry.get("satoshis").and_then(Value::as_u64), Some(777));
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `getblockhash` request — positional `[height]` —
    /// gives 200 and the legacy envelope, with `result` the block hash in display
    /// order.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getblockhash_succeeds() {
        use zaino_primitives::types::{BlockHeader, CompactDifficulty, EquihashSolution};
        let mut hash_bytes = [0u8; 32];
        hash_bytes[0] = 0x11;
        hash_bytes[31] = 0xaa;
        let expected_display = format!("aa{}11", "00".repeat(30));
        let header = BlockHeader {
            hash: BlockHash::from(hash_bytes),
            version: 4,
            prev_hash: BlockHash::from([0u8; 32]),
            height: Height::try_from(100).expect("valid height"),
            time: 1_600_000_000,
            merkle_root: [0u8; 32].into(),
            block_commitments: [0u8; 32].into(),
            bits: CompactDifficulty::try_from_bits(0x1f07_ffff).expect("valid nBits"),
            nonce: [0u8; 32],
            solution: EquihashSolution::Regtest([0; 36]),
        };
        let (addr, handle) = spawn_server(MockChain {
            block_header: Some(header),
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getblockhash","params":[100]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert!(obj.get("error").is_some_and(Value::is_null));
        assert_eq!(
            obj.get("result").and_then(Value::as_str),
            Some(expected_display.as_str())
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `gettxout` request — positional `[txid, n]` — for a
    /// spent or unknown outpoint gives 200 and `result: null`, matching
    /// zcashd/zebra. The default mock scripts no output.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_gettxout_unknown_is_null() {
        let (addr, handle) = spawn_server(MockChain::default());
        let (status, body) = post(
            addr,
            "text/plain",
            &format!(
                r#"{{"jsonrpc":"1.0","id":"zcashex","method":"gettxout","params":["{}",0]}}"#,
                "ab".repeat(32)
            ),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert!(
            obj.get("result").is_some_and(Value::is_null),
            "a spent or unknown outpoint is null: {obj:?}"
        );
        assert!(obj.get("error").is_some_and(Value::is_null));
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `getdifficulty` request gives 200 and the legacy
    /// envelope, with `result` the relayed difficulty.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getdifficulty_succeeds() {
        let (addr, handle) = spawn_server(MockChain {
            difficulty: Some(322_008_416.553_987_15),
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getdifficulty","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert!(obj.get("error").is_some_and(Value::is_null));
        assert_eq!(
            obj.get("result").and_then(Value::as_f64),
            Some(322_008_416.553_987_15)
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `getnetworkinfo` request gives 200 and the legacy
    /// envelope, with `result` the network-info object.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_getnetworkinfo_succeeds() {
        use zaino_primitives::types::rpc::NetworkInfo;
        use zaino_primitives::types::Zatoshis;
        let (addr, handle) = spawn_server(MockChain {
            network_info: Some(NetworkInfo {
                version: 6_040_200,
                subversion: "/Zebra:6.4.2/".to_string(),
                protocol_version: 170_160,
                local_services: "0000000000000001".to_string(),
                time_offset: 0,
                connections: 44,
                networks: Vec::new(),
                relay_fee: Zatoshis::new(100).expect("valid amount"),
                local_addresses: Vec::new(),
                warnings: String::new(),
            }),
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"getnetworkinfo","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert!(obj.get("error").is_some_and(Value::is_null));
        let result = obj
            .get("result")
            .and_then(Value::as_object)
            .expect("result object");
        assert_eq!(
            result.get("version").and_then(Value::as_u64),
            Some(6_040_200)
        );
        assert_eq!(result.get("connections").and_then(Value::as_u64), Some(44));
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `ping` request gives 200 and the legacy envelope,
    /// with `result: null` on success — the shape zcashd/zebra return.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_ping_succeeds() {
        let (addr, handle) = spawn_dialect_server();
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"ping","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert!(
            obj.get("result").is_some_and(Value::is_null),
            "ping result is present and null: {obj:?}"
        );
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `z_gettreestate` request — a single height string
    /// param — gives 200 and the legacy envelope, with `result` the nested
    /// treestate object carrying the active pool's commitments.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_z_gettreestate_succeeds() {
        use zaino_primitives::types::{PoolTreestate, TreeRoot, Treestate};
        let (addr, handle) = spawn_server(MockChain {
            treestate: Some(Treestate {
                block_hash: BlockHash::from([0x11u8; 32]),
                height: Height::try_from(100).expect("valid height"),
                time: 1_600_000_000,
                sapling: Some(PoolTreestate {
                    final_root: Some(TreeRoot::from([0x22u8; 32])),
                    final_state: vec![0xde, 0xad],
                }),
                orchard: None,
                ironwood: None,
            }),
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"z_gettreestate","params":["100"]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        let result = obj
            .get("result")
            .and_then(Value::as_object)
            .expect("result object");
        assert_eq!(result.get("height").and_then(Value::as_u64), Some(100));
        assert!(
            result
                .get("sapling")
                .and_then(|p| p.get("commitments"))
                .and_then(|c| c.get("finalState"))
                .and_then(Value::as_str)
                == Some("dead"),
            "the sapling tree state renders as hex: {result:?}"
        );
        let _ = handle.stop();
    }

    /// The zcashex-shaped 1.0 `z_getsubtreesbyindex` request — positional
    /// `[pool, startIndex]` — gives 200 and the legacy envelope, with `result` the
    /// `{pool, start_index, subtrees}` object.
    #[tokio::test(flavor = "multi_thread")]
    async fn zcashex_one_point_zero_z_getsubtreesbyindex_succeeds() {
        use zaino_primitives::types::{SubtreeRoot, TreeRoot};
        let (addr, handle) = spawn_server(MockChain {
            subtree_roots: vec![SubtreeRoot {
                root: TreeRoot::from([0xABu8; 32]),
                end_height: Height::try_from(558_822).expect("valid height"),
            }],
            ..Default::default()
        });
        let (status, body) = post(
            addr,
            "text/plain",
            r#"{"jsonrpc":"1.0","id":"zcashex","method":"z_getsubtreesbyindex","params":["sapling",0]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        let obj = body.as_object().expect("a JSON object");
        assert_eq!(obj.get("jsonrpc").and_then(Value::as_str), Some("1.0"));
        assert!(
            obj.get("error").is_some_and(Value::is_null),
            "error is present and null: {obj:?}"
        );
        let result = obj
            .get("result")
            .and_then(Value::as_object)
            .expect("result object");
        assert_eq!(result.get("pool").and_then(Value::as_str), Some("sapling"));
        assert_eq!(result.get("start_index").and_then(Value::as_u64), Some(0));
        let subtree = result
            .get("subtrees")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .expect("one subtree");
        assert_eq!(
            subtree.get("root").and_then(Value::as_str),
            Some("ab".repeat(32).as_str())
        );
        assert_eq!(
            subtree.get("end_height").and_then(Value::as_u64),
            Some(558_822)
        );
        let _ = handle.stop();
    }

    /// A 2.0 error stays 200, unchanged from today.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_point_zero_error_stays_200() {
        let (addr, handle) = spawn_dialect_server();
        let (status, body) = post(
            addr,
            "application/json",
            r#"{"jsonrpc":"2.0","id":1,"method":"nosuchmethod","params":[]}"#,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "2.0 errors stay 200");
        let obj = body.as_object().expect("a JSON object");
        assert!(
            obj.get("error")
                .and_then(|error| error.get("message"))
                .is_some(),
            "the 2.0 error carries a message: {obj:?}"
        );
        let _ = handle.stop();
    }
}
