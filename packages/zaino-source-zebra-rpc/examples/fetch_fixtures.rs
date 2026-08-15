//! Fetch block data for test fixtures.
//!
//! Usage:
//!   cargo run -p zaino-source-zebra-rpc --example fetch_fixtures

use zaino_primitives::types::Height;
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::GetBlock;
use zaino_source_zebra_rpc::ZebraRpcAdapter;

const HEIGHTS: &[u32] = &[
    419_200,   // Sapling activation height — coinbase only
    1_000_000, // Mid-sapling era
    1_687_104, // NU5 activation height
    2_000_000, // Post-NU5 with orchard activity
    2_500_000, // More recent
];

#[tokio::main]
async fn main() {
    let rpc = RpcClient::new(RpcClientConfig {
        url: "http://127.0.0.1:8232".to_string(),
        auth: None,
        ..Default::default()
    })
    .expect("client creation");

    let adapter = ZebraRpcAdapter::new(rpc);

    for &h in HEIGHTS {
        let height = Height::try_from(h).expect("valid height");
        let block = adapter.get_block(height).await.expect("get_block failed");

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

        println!("Height {h}:");
        println!("  hash:       {}", block.header.hash);
        println!("  prev_hash:  {}", block.header.prev_hash);
        println!("  time:       {}", block.header.time);
        println!("  txs:        {}", block.transactions.len());
        println!("  transparent: {t_in} inputs, {t_out} outputs");
        println!("  sapling:     {s_spends} spends, {s_outputs} outputs");
        println!("  orchard:     {o_actions} actions");
        println!();
    }
}
