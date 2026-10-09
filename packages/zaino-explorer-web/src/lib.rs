//! Server-rendered web surface over the explorer's domain. See
//! `docs/superpowers/specs/2026-10-08-zaino-block-explorer-design.md`.
//!
//! `build_app` and its routes are generic over [`ChainReader`], not over any
//! transport detail — nothing in this module knows a concrete adapter or
//! jsonrpsee exists. The composition root that injects one lives in
//! `src/bin/web.rs` alone.
#![forbid(unsafe_code)]

use axum::extract::{Path, State};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use maud::html;
use zaino_explorer_domain::ChainReader;

/// The explorer's web router: every route is a live read through `reader`,
/// never a cached or locally re-derived value — that's the whole point of a
/// dogfood client.
pub fn build_app<C: ChainReader>(reader: C) -> Router {
    Router::new()
        .route("/", get(home::<C>))
        .route("/block/:id", get(block::<C>))
        .route("/tx/:txid", get(transaction::<C>))
        .route("/address/:address", get(address::<C>))
        .with_state(reader)
}

/// How many recent blocks the home page lists.
const RECENT_BLOCKS: u32 = 10;

/// `GET /`: the chain height, node/mempool status, and a recent-blocks
/// list, all fetched live and rendered independently of each other.
async fn home<C: ChainReader>(State(reader): State<C>) -> Html<String> {
    let height = reader.chain_height().await;
    let blocks = reader.recent_blocks(RECENT_BLOCKS).await;
    let status = reader.node_status().await;

    let body = html! {
        h1 { "zaino-block-explorer" }
        @match height {
            Ok(height) => p { "Chain height: " (height) },
            Err(e) => p { "RPC error: " (e.to_string()) },
        }
        @match status {
            Ok(status) => p {
                (status.subversion) " — " (status.connections) " peers — mempool: "
                (status.mempool_size) " tx, " (status.mempool_bytes) " bytes"
            },
            Err(e) => p { "Node status unavailable: " (e.to_string()) },
        }
        @match blocks {
            Ok(blocks) => ul {
                @for block in &blocks {
                    li {
                        a href=(format!("/block/{}", block.height)) { (block.height) }
                        " — " (block.hash) " — " (block.time) " — " (block.tx_count) " tx"
                    }
                }
            },
            Err(e) => p { "RPC error: " (e.to_string()) },
        }
    };
    Html(body.into_string())
}

/// `GET /block/{id}`: one block's detail — hash, time, and every
/// transaction id it contains, each linked to `/tx/{txid}` — by height or
/// hash (`getblock`'s own polymorphic id parameter). Linked from the home
/// page's block list.
async fn block<C: ChainReader>(State(reader): State<C>, Path(id): Path<String>) -> Html<String> {
    let body = match reader.block(id.clone()).await {
        Ok(detail) => html! {
            h1 { "Block " (detail.height) }
            p { "Hash: " (detail.hash) }
            p { "Time: " (detail.time) }
            h2 { "Transactions" }
            ul {
                @for txid in &detail.tx_ids {
                    li { a href=(format!("/tx/{txid}")) { (txid) } }
                }
            }
        },
        Err(e) => html! {
            h1 { "Block " (id) }
            p { "RPC error: " (e.to_string()) }
        },
    };
    Html(body.into_string())
}

/// `GET /tx/{txid}`: one transaction's detail, fetched live. Linked from a
/// block's transaction list and from an address's transaction history.
async fn transaction<C: ChainReader>(
    State(reader): State<C>,
    Path(txid): Path<String>,
) -> Html<String> {
    let body = match reader.transaction(txid.clone()).await {
        Ok(tx) => html! {
            h1 { "Transaction " (tx.txid) }
            p { "Size: " (tx.size) " bytes" }
            @if let Some(height) = tx.height {
                p { "Block height: " (height) }
            }
            @if let Some(confirmations) = tx.confirmations {
                p { "Confirmations: " (confirmations) }
            }
            h2 { "Outputs" }
            ul {
                @for (index, output) in tx.outputs.iter().enumerate() {
                    li {
                        (output.value_zat) " zat"
                        @if !output.addresses.is_empty() {
                            " — " (output.addresses.join(", "))
                        }
                        " — "
                        // getspentinfo answers "not found" for both a genuinely
                        // unspent output and an unknown one — the RPC does not
                        // distinguish them, so neither does this page. A real
                        // error (network, decode) renders the same way; telling
                        // those apart would need a typed error-kind this
                        // adapter's ChainReadError doesn't carry yet.
                        @match reader.spend_info(tx.txid.clone(), index as u32).await {
                            Ok(spend) => (format!("spent by {} in block {}", spend.spending_txid, spend.height)),
                            Err(_) => ("unspent (or unknown)".to_string()),
                        }
                    }
                }
            }
        },
        Err(e) => html! {
            h1 { "Transaction " (txid) }
            p { "RPC error: " (e.to_string()) }
        },
    };
    Html(body.into_string())
}

/// `GET /address/{address}`: one transparent address's balance and
/// transaction history, fetched live. Not yet linked from anywhere — the
/// explorer has no search form yet, so this is reachable only by direct URL.
async fn address<C: ChainReader>(
    State(reader): State<C>,
    Path(address): Path<String>,
) -> Html<String> {
    let body = match reader.address(address.clone()).await {
        Ok(summary) => html! {
            h1 { "Address " (summary.address) }
            p { "Balance: " (summary.balance_zat) " zat" }
            p { "Lifetime received: " (summary.received_zat) " zat" }
            h2 { "Transactions" }
            ul {
                @for txid in &summary.txids {
                    li { a href=(format!("/tx/{txid}")) { (txid) } }
                }
            }
        },
        Err(e) => html! {
            h1 { "Address " (address) }
            p { "RPC error: " (e.to_string()) }
        },
    };
    Html(body.into_string())
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use jsonrpsee::http_client::HttpClientBuilder;
    use std::net::TcpListener;
    use tower::ServiceExt;
    use zaino_explorer_zaino_client::ZainoClient;
    use zaino_noderpc::{NodeRpc, NodeRpcApiServer};
    use zaino_service::testing::{MockChain, MockIndexerService};
    use zcash_protocol::consensus::Network;

    /// Boot a real `NodeRpc` jsonrpsee server on an ephemeral port, over a mock
    /// chain tipped at height 291, and return its address and a handle the test
    /// stops when done.
    async fn spawn_mock_server() -> (std::net::SocketAddr, jsonrpsee::server::ServerHandle) {
        use zaino_primitives::types::{BlockHash, Height};

        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: Height::try_from(291).expect("valid height"),
                hash: BlockHash::from([0xCDu8; 32]),
            }),
            ..Default::default()
        };
        let handler = NodeRpc::new(MockIndexerService::new(chain), Network::MainNetwork);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        listener.set_nonblocking(true).expect("set nonblocking");
        let addr = listener.local_addr().expect("local addr");
        let server = jsonrpsee::server::ServerBuilder::default()
            .build_from_tcp(listener)
            .expect("build server from listener");
        let handle = server.start(handler.into_rpc());
        (addr, handle)
    }

    /// The home page renders the mock chain's height (291) fetched live
    /// through the generated client — not a cached or hand-rolled value. The
    /// router here is generic over `ChainReader`; this test is what proves
    /// that generic code actually works end-to-end with the one real
    /// implementor. `getblock` isn't scripted on this mock (only the tip
    /// is), so the blocks section is expected to report an RPC error rather
    /// than panic or render silently — proving height and blocks render
    /// independently of each other, and that the error path is wired, not
    /// just the happy path.
    //
    // multi_thread required: the test drives a live jsonrpsee server (its own
    // accept loop) concurrently with the router's outbound RPC call to it, on
    // one runtime — see the identical justification in zaino-noderpc's own
    // `transport.rs` server tests.
    #[tokio::test(flavor = "multi_thread")]
    async fn home_page_renders_block_rpc_error_without_losing_the_height() {
        let (addr, handle) = spawn_mock_server().await;
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let app = crate::build_app(ZainoClient::new(client));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router does not error");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf8 body");
        assert!(text.contains("291"), "height still renders: {text}");
        assert!(
            text.contains("RPC error"),
            "unscripted getblock should render as an RPC error, not silence: {text}"
        );

        let _ = handle.stop();
    }

    /// `/block/{id}` for an unscripted height renders a graceful RPC error,
    /// not a panic or a 500 — the route is reachable and its error path is
    /// wired. The happy-path mapping (`BlockDetail` field-for-field,
    /// including txids) is covered in `zaino-explorer-zaino-client`'s own
    /// pure-function and mock-server tests; this test is about the route.
    #[tokio::test(flavor = "multi_thread")]
    async fn block_route_renders_rpc_error_for_an_unscripted_height() {
        let (addr, handle) = spawn_mock_server().await;
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let app = crate::build_app(ZainoClient::new(client));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/block/1")
                    .body(axum::body::Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router does not error");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf8 body");
        assert!(
            text.contains("RPC error"),
            "an unscripted height should render as an RPC error, not a panic: {text}"
        );

        let _ = handle.stop();
    }

    /// `/tx/{txid}` for an unscripted txid renders a graceful RPC error
    /// (zcashd's own "not found" for an unknown transaction), not a panic
    /// or a 500 — the route is reachable and its error path is wired. The
    /// happy-path mapping (`TransactionDetail` field-for-field) is covered
    /// in `zaino-explorer-zaino-client`'s own pure-function tests, which
    /// don't need a server; this test is about the route, not the mapping.
    #[tokio::test(flavor = "multi_thread")]
    async fn transaction_route_renders_rpc_error_for_an_unknown_txid() {
        let (addr, handle) = spawn_mock_server().await;
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let app = crate::build_app(ZainoClient::new(client));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/tx/{}", "ab".repeat(32)))
                    .body(axum::body::Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router does not error");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf8 body");
        assert!(
            text.contains("RPC error"),
            "an unscripted txid should render as an RPC error, not a panic: {text}"
        );

        let _ = handle.stop();
    }

    /// `/address/{address}` for an address with no scripted history renders
    /// a zero balance, not an error — explorer policy reads "nothing found"
    /// as zero, distinct from an actual RPC failure. Proves the route is
    /// reachable and composes both underlying calls (balance + txids)
    /// without crashing; the field-mapping itself is covered by
    /// `zaino-explorer-zaino-client`'s own test against a scripted balance.
    #[tokio::test(flavor = "multi_thread")]
    async fn address_route_renders_zero_balance_for_an_unknown_address() {
        let (addr, handle) = spawn_mock_server().await;
        let client = HttpClientBuilder::default()
            .build(format!("http://{addr}"))
            .expect("build http client");
        let app = crate::build_app(ZainoClient::new(client));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/address/t1unknown")
                    .body(axum::body::Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("router does not error");

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("utf8 body");
        assert!(
            text.contains("Balance: 0 zat"),
            "an address with no history should read as zero, not an error: {text}"
        );

        let _ = handle.stop();
    }
}
