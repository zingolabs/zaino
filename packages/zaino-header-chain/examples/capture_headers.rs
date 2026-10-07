//! Capture contiguous header ranges for the real-chain oracle tests
//!
//! Usage: `cargo run -p zaino-header-chain --example capture_headers [main|test] [rpc-url]`
//!
//! - `getblockheader <h> false` per height → `tests/fixtures/<network>_<start>.headers`
//! - file = the raw consensus headers, concatenated (1487 bytes each: 200-9 solution)
//! - testnet: no golden zebra on the tailnet (`kubectl -n golden-testnet port-forward svc/zebra`)
//! - NU7 range: only from a validator that activated NU7 (zebra ≥ 7; a 6.x follows the old rules)

use std::io::Write;

const MAINNET_RPC: &str = "http://golden-mainnet-zebra.vaquita-altair.ts.net:8232";
const TESTNET_RPC: &str = "http://127.0.0.1:28232";
const MAINNET_RANGES: [(u32, u32); 3] = [(0, 300), (653_500, 653_700), (3_508_500, 3_508_800)];
/// Min-difficulty start 299,188 · Blossom 584,000 · NU7 4,465,026 (113-header context before each)
const TESTNET_RANGES: [(u32, u32); 3] =
    [(299_000, 299_400), (583_800, 584_200), (4_464_900, 4_465_300)];
const HEADER_LEN: usize = 1487;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let network = std::env::args().nth(1).unwrap_or_else(|| "main".to_owned());
    let (name, default_rpc, ranges) = match network.as_str() {
        "main" => ("mainnet", MAINNET_RPC, MAINNET_RANGES),
        "test" => ("testnet", TESTNET_RPC, TESTNET_RANGES),
        other => return Err(format!("network {other}: main or test").into()),
    };
    let rpc = std::env::args().nth(2).unwrap_or_else(|| default_rpc.to_owned());
    let client = reqwest::blocking::Client::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    std::fs::create_dir_all(dir)?;

    for (start, end) in ranges {
        let calls: Vec<serde_json::Value> = (start..=end)
            .map(|h| {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": h, "method": "getblockheader",
                    "params": [h.to_string(), false],
                })
            })
            .collect();
        let request = client
            .post(&rpc)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&calls)?);
        let replies: Vec<serde_json::Value> = serde_json::from_slice(&request.send()?.bytes()?)?;
        let mut by_height: Vec<(u64, Vec<u8>)> = replies
            .iter()
            .map(|reply| {
                let id = reply["id"].as_u64().ok_or("reply without id")?;
                let hex = reply["result"].as_str().ok_or_else(|| format!("{id}: {reply}"))?;
                Ok((id, hex::decode(hex)?))
            })
            .collect::<Result<_, Box<dyn std::error::Error>>>()?;
        by_height.sort_by_key(|(height, _)| *height);

        let path = format!("{dir}/{name}_{start}.headers");
        let mut file = std::fs::File::create(&path)?;
        for (height, header) in &by_height {
            if header.len() != HEADER_LEN {
                return Err(format!("{height}: {} bytes, not {HEADER_LEN}", header.len()).into());
            }
            file.write_all(header)?;
        }
        println!("{path}: {} headers", by_height.len());
    }
    Ok(())
}
