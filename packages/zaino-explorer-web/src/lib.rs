//! Server-rendered web surface over the explorer's domain. See
//! `docs/superpowers/specs/2026-10-08-zaino-block-explorer-design.md`.
//!
//! `build_app` and its routes are generic over [`ChainReader`], not over any
//! transport detail — nothing in this module knows a concrete adapter or
//! jsonrpsee exists. The composition root that injects one lives in
//! `src/bin/web.rs` alone.
#![forbid(unsafe_code)]

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use maud::html;
use zaino_explorer_domain::ChainReader;

/// The explorer's web router: every route is a live read through `reader`,
/// never a cached or locally re-derived value — that's the whole point of a
/// dogfood client.
pub fn build_app<C: ChainReader>(reader: C) -> Router {
    Router::new().route("/", get(home::<C>)).with_state(reader)
}

/// `GET /`: the chain height, fetched live.
async fn home<C: ChainReader>(State(reader): State<C>) -> Html<String> {
    let body = match reader.chain_height().await {
        Ok(height) => html! {
            h1 { "zaino-block-explorer" }
            p { "Chain height: " (height) }
        },
        Err(e) => html! {
            h1 { "zaino-block-explorer" }
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
        let chain = MockChain {
            tip: Some(zaino_primitives::types::BlockRef {
                height: zaino_primitives::types::Height::try_from(291).expect("valid height"),
                hash: zaino_primitives::types::BlockHash::from([0xCDu8; 32]),
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

    /// The home page renders the mock chain's height (291) fetched live through
    /// the generated client — not a cached or hand-rolled value. The router
    /// here is generic over `ChainReader`; this test is what proves that
    /// generic code actually works end-to-end with the one real implementor.
    //
    // multi_thread required: the test drives a live jsonrpsee server (its own
    // accept loop) concurrently with the router's outbound RPC call to it, on
    // one runtime — see the identical justification in zaino-noderpc's own
    // `transport.rs` server tests.
    #[tokio::test(flavor = "multi_thread")]
    async fn home_page_renders_live_chain_height() {
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
        assert!(
            text.contains("291"),
            "home page should render the live chain height 291: {text}"
        );

        let _ = handle.stop();
    }
}
