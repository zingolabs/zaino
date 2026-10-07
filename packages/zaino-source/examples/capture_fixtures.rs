//! `getblock <h> 0` hex per fixture height → stdout (`tests/fixtures/block_<height>.hex`)
//!
//! - `cargo run -p zaino-source --example capture_fixtures` (zebrad at 127.0.0.1:8232)

use zaino_source::{Lane, RpcClient, RpcClientConfig};

const HEIGHTS: &[u32] = &[419_200, 1_000_000, 1_687_104, 2_000_000, 2_500_000];

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(RpcClientConfig {
        url: "http://127.0.0.1:8232".to_string(),
        auth: None,
        ..Default::default()
    })
    .expect("client creation");

    for &h in HEIGHTS {
        let params =
            vec![serde_json::Value::String(h.to_string()), serde_json::Value::Number(0.into())];
        let value = rpc.call(Lane::Sync, "getblock", params).await.expect("getblock failed");
        let hex_str = value.as_str().expect("expected string response");

        println!("--- HEIGHT {h} ---");
        println!("{hex_str}");
        println!();
    }
}
