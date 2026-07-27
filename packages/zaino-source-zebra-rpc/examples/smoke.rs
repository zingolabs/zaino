//! Smoke test: connect to a live Zebra, fetch tip + one block.
//!
//! Usage:
//!   cargo run -p zaino-source-zebra-rpc --example smoke
//!
//! Requires a Zebra RPC at http://127.0.0.1:8232 (e.g. via port-forward).

use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source_zebra_rpc::ZebraRpcAdapter;

use zaino_source::{GetBlock, GetChainTip};

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(RpcClientConfig {
        url: "http://127.0.0.1:8232".to_string(),
        auth: None,
        ..Default::default()
    })
    .expect("client creation");

    let adapter = ZebraRpcAdapter::new(rpc);

    // Fetch chain tip.
    let (tip_hash, tip_height) = adapter.get_chain_tip().await.expect("get_chain_tip failed");

    println!("Chain tip: height={tip_height}, hash={tip_hash}");

    // Fetch the block at tip.
    let block = adapter
        .get_block(tip_height)
        .await
        .expect("get_block failed");

    println!("Block {tip_height}:");
    println!("  hash:       {}", block.header.hash);
    println!("  prev_hash:  {}", block.header.prev_hash);
    println!("  time:       {}", block.header.time);
    println!("  txs:        {}", block.transactions.len());

    // Show per-pool stats.
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
    println!("  transparent: {t_in} inputs, {t_out} outputs");
    println!("  sapling:     {s_spends} spends, {s_outputs} outputs");
    println!("  orchard:     {o_actions} actions");
}
