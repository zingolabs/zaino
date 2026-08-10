//! Offline parity test: replay canned Zebra RPC responses through the adapter
//! and assert the deserialized block matches known-good block explorer data.
//!
//! Fixtures in `tests/fixtures/block_<height>.hex` were captured from a synced
//! Zebra node via `examples/capture_fixtures.rs` and cross-checked against
//! Blockchair on 2026-07-11.

use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use zaino_primitives::types::Height;
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::GetBlock;
use zaino_source_zebra_rpc::ZebraRpcAdapter;

struct Expected {
    height: u32,
    hash: &'static str,
    prev_hash: &'static str,
    time: u32,
    tx_count: usize,
    transparent_inputs: usize,
    transparent_outputs: usize,
    sapling_spends: usize,
    sapling_outputs: usize,
    orchard_actions: usize,
}

const FIXTURES: &[Expected] = &[
    Expected {
        height: 419_200,
        hash: "00000000025a57200d898ac7f21e26bf29028bbe96ec46e05b2c17cc9db9e4f3",
        prev_hash: "00000000025c3b19eb08bbc0d74c0c5e798fcd58b38ecdcdda6b83e5c5945295",
        time: 1_540_779_337,
        tx_count: 1,
        transparent_inputs: 0,
        transparent_outputs: 2,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 0,
    },
    Expected {
        height: 1_000_000,
        hash: "000000000062eff9ae053020017bfef24e521a2704c5ec9ead2a4608ac70fc7a",
        prev_hash: "0000000001512dea98f49c5890d45f03b755bfea6adb4b284aa3eb3aa46af377",
        time: 1_602_206_541,
        tx_count: 6,
        transparent_inputs: 6,
        transparent_outputs: 11,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 0,
    },
    Expected {
        height: 1_687_104,
        hash: "0000000000d723156d9b65ffcf4984da7a19675ed7e2f06d9e5d5188af087bf8",
        prev_hash: "000000000162bdf56998ca7703adad22cdce21252f6354c077e82ccf434e761f",
        time: 1_654_019_405,
        tx_count: 1,
        transparent_inputs: 0,
        transparent_outputs: 4,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 0,
    },
    Expected {
        height: 2_000_000,
        hash: "00000000010accaf2f87934765dc2e0bf4823a2b1ae2c1395b334acfce52ad68",
        prev_hash: "0000000000648d155ec973abb1bc0aa876cdc19b5ede56f32a3546a7eca4425d",
        time: 1_677_602_242,
        tx_count: 44,
        transparent_inputs: 53,
        transparent_outputs: 35,
        sapling_spends: 5,
        sapling_outputs: 12,
        orchard_actions: 44,
    },
    Expected {
        height: 2_500_000,
        hash: "00000000000a18c24ade5fe0a0a6dd2639fea4552098cb1181f016110d3b3ef6",
        prev_hash: "00000000011f1ded32bb36f0ec2a6a6e2912808380078b88430bfc5c00b8dd10",
        time: 1_715_296_781,
        tx_count: 2,
        transparent_inputs: 0,
        transparent_outputs: 4,
        sapling_spends: 0,
        sapling_outputs: 0,
        orchard_actions: 2,
    },
];

/// Load hex fixtures into a height→hex-string map.
fn load_fixtures() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for expected in FIXTURES {
        let path = format!(
            "{}/tests/fixtures/block_{}.hex",
            env!("CARGO_MANIFEST_DIR"),
            expected.height,
        );
        let hex = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {path}: {e}"))
            .trim()
            .to_string();
        map.insert(expected.height.to_string(), hex);
    }
    map
}

/// Minimal HTTP server that responds to JSON-RPC `getblock` requests with
/// canned hex fixtures. Runs until the sender is dropped.
async fn mock_zebra_rpc(listener: TcpListener, fixtures: HashMap<String, String>) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => return,
        };
        let fixtures = fixtures.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1024 * 1024];
            let n = match stream.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };
            let body_str = String::from_utf8_lossy(&buf[..n]);

            // Find the JSON body after the blank line.
            let json_body = body_str.split("\r\n\r\n").nth(1).unwrap_or(&body_str);

            let req: serde_json::Value =
                serde_json::from_str(json_body).expect("invalid JSON-RPC request");

            let method = req["method"].as_str().unwrap_or("");
            let id = &req["id"];

            let result = match method {
                "getblock" => {
                    let height = req["params"][0].as_str().unwrap_or("");
                    match fixtures.get(height) {
                        Some(hex) => serde_json::Value::String(hex.clone()),
                        None => {
                            let err_resp = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": null,
                                "error": {"code": -8, "message": "Block not found"}
                            });
                            let err_body = serde_json::to_string(&err_resp).expect("json");
                            let resp = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                                err_body.len(),
                                err_body,
                            );
                            let _ = stream.write_all(resp.as_bytes()).await;
                            return;
                        }
                    }
                }
                _ => serde_json::Value::Null,
            };

            let resp_json = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
                "error": null,
            });
            let resp_body = serde_json::to_string(&resp_json).expect("json");
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                resp_body.len(),
                resp_body,
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    }
}

#[tokio::test]
async fn block_parity_with_explorer_offline() {
    let fixtures = load_fixtures();

    // Bind to a random port.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();

    // Start mock server.
    tokio::spawn(mock_zebra_rpc(listener, fixtures));

    // Point the adapter at the mock.
    let rpc = RpcClient::new(RpcClientConfig {
        url: format!("http://127.0.0.1:{port}"),
        auth: None,
        ..Default::default()
    })
    .expect("RPC client creation");
    let adapter = ZebraRpcAdapter::new(rpc);

    for expected in FIXTURES {
        let height = Height::try_from(expected.height).expect("fixture height");
        let block = adapter
            .get_block(height)
            .await
            .unwrap_or_else(|e| panic!("get_block({}) failed: {e}", expected.height));

        assert_eq!(
            block.header.hash.to_string(),
            expected.hash,
            "hash mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            block.header.prev_hash.to_string(),
            expected.prev_hash,
            "prev_hash mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            block.header.time, expected.time,
            "time mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            block.transactions.len(),
            expected.tx_count,
            "tx count mismatch at height {}",
            expected.height,
        );

        let mut t_in = 0usize;
        let mut t_out = 0usize;
        let mut s_spends = 0usize;
        let mut s_outputs = 0usize;
        let mut o_actions = 0usize;
        for tx in &block.transactions {
            t_in += tx.transparent.inputs.len();
            t_out += tx.transparent.outputs.len();
            s_spends += tx.sapling.spends.len();
            s_outputs += tx.sapling.outputs.len();
            o_actions += tx.orchard.actions.len();
        }

        assert_eq!(
            t_in, expected.transparent_inputs,
            "transparent inputs mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            t_out, expected.transparent_outputs,
            "transparent outputs mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            s_spends, expected.sapling_spends,
            "sapling spends mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            s_outputs, expected.sapling_outputs,
            "sapling outputs mismatch at height {}",
            expected.height,
        );
        assert_eq!(
            o_actions, expected.orchard_actions,
            "orchard actions mismatch at height {}",
            expected.height,
        );
    }
}
