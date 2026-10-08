//! Composition root for the web surface: the one place that constructs the
//! concrete [`ZainoClient`] adapter and injects it into the generic router.
//!
//! Config: two env vars, nothing fancier. `ZAINO_RPC_URL` (default
//! `http://127.0.0.1:8232`) is the zainod node-rpc endpoint; `EXPLORER_ADDR`
//! (default `127.0.0.1:3000`) is where this binary listens.

use jsonrpsee::http_client::HttpClientBuilder;
use zaino_explorer_zaino_client::ZainoClient;

#[tokio::main]
async fn main() {
    let rpc_url =
        std::env::var("ZAINO_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8232".to_string());
    let listen_addr =
        std::env::var("EXPLORER_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());

    let client = HttpClientBuilder::default()
        .build(&rpc_url)
        .unwrap_or_else(|e| panic!("invalid ZAINO_RPC_URL {rpc_url:?}: {e}"));
    let app = zaino_explorer_web::build_app(ZainoClient::new(client));

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .unwrap_or_else(|e| panic!("bind {listen_addr:?}: {e}"));
    println!("zaino-block-explorer listening on http://{listen_addr}, talking to {rpc_url}");
    axum::serve(listener, app)
        .await
        .unwrap_or_else(|e| panic!("serve: {e}"));
}
