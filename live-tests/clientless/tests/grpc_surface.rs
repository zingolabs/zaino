//! Smoke over zaino's block, tree-state and info gRPC reads against a regtest zebrad
//!
//! - Transparent miner → every coinbase carries a vout (pool filters observable on the wire)

use std::time::Duration;

use anyhow::{Context, Result};
use pretty_assertions::assert_eq;
use serde_json::json;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(60);

/// One chain through every read: info, tip, block by height / hash, ranges (pool filters,
/// nullifiers, out-of-range drains), tree state; validator = oracle wherever it answers
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn read_surface_serves_the_validators_chain() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator =
        env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(Pool::Transparent));
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(10).await?;
    indexer.wait_for_block_num(tip, READY).await?;
    let height = u64::from(u32::from(tip));

    let info = indexer.indexer_info().await?;
    assert!(!info.chain_name.is_empty(), "chain_name set: {info:?}");
    assert_eq!(info.block_height, height, "lightd info height = tip");
    assert_eq!(
        indexer.latest_block().await?,
        validator.tip().await?,
        "served tip: zaino vs zebrad"
    );

    let by_height = indexer.get_block(BlockHeight::from(1u32)).await?;
    let hash = BlockHash::from_wire(&by_height.hash).context("block hash must be 32 bytes")?;
    let (_, truth) = validator.get_block(BlockHeight::from(1u32)).await?;
    assert_eq!((by_height.height, hash), (1, truth), "block 1 (display-order hash) = validator's");
    let by_hash = indexer.get_block_by_hash(hash).await?;
    assert_eq!(by_hash, by_height, "block 1: GetBlock by hash {hash} vs by height");

    // From 1 (non-standard genesis coinbase script unservable: zingolabs/zaino#818)
    let start = BlockHeight::from(1u32);
    let shielded = indexer.get_block_range(start, tip).await?;
    assert_eq!(shielded.len() as u64, height, "every height in [1, {tip:?}]");
    for (offset, block) in shielded.iter().enumerate() {
        assert_eq!(block.height, offset as u64 + 1, "contiguous from 1");
        assert_eq!(block.hash.len(), 32, "block hash width at {}", block.height);
        let transparent: usize = block.vtx.iter().map(|tx| tx.vin.len() + tx.vout.len()).sum();
        assert_eq!(transparent, 0, "default filter = shielded only, at {}", block.height);
    }

    // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4
    let all_pools = vec![1, 2, 3, 4];
    let full = indexer.get_block_range_with_pools(start, tip, all_pools.clone()).await?;
    assert_eq!(full.len(), shielded.len(), "same heights, all pools");
    for block in &full {
        for tx in &block.vtx {
            let vout = tx.vout.first().with_context(|| {
                format!("all pools at {}: tx {} carries no vout", block.height, tx.index)
            })?;
            assert!(!vout.script_pub_key.is_empty(), "vout carries a script at {}", block.height);
        }
    }

    let nullifiers = indexer.get_block_range_nullifiers(start, tip).await?;
    let ids = |blocks: &[CompactBlock]| -> Vec<(u64, String)> {
        blocks.iter().map(|block| (block.height, hex::encode(&block.hash))).collect()
    };
    assert_eq!(ids(&nullifiers), ids(&shielded), "(height, hash): nullifier vs default range");
    for block in &nullifiers {
        let transparent: Vec<(usize, usize)> =
            block.vtx.iter().map(|tx| (tx.vin.len(), tx.vout.len())).collect();
        let stripped = transparent.iter().all(|&counts| counts == (0, 0));
        assert!(
            stripped,
            "nullifiers only at {}: (vin, vout) per tx {transparent:?}",
            block.height
        );
    }

    // Past the tip → served up to it, then an error (not a clean end)
    let past = BlockHeight::from(u32::from(tip) + 6);
    let (blocks, errored) = indexer.drain_block_range(start, past, all_pools.clone()).await?;
    let last = blocks.last().map(|block| block.height);
    assert_eq!((blocks.len() as u64, last), (height, Some(height)), "drain stops at the tip");
    assert!(errored, "drain past the tip ends in an error");
    let (blocks, errored) = indexer.drain_block_range(past, start, all_pools).await?;
    assert!(blocks.is_empty(), "descending from past the tip serves nothing: {blocks:?}");
    assert!(errored, "descending drain from past the tip ends in an error");

    let tree = indexer.get_tree_state(tip).await?;
    let best = validator.json_rpc().await?.call_value("getbestblockhash", json!([])).await?;
    let best = best.as_str().with_context(|| format!("getbestblockhash: {best}"))?;
    let served = (tree.height, tree.hash.as_str());
    assert_eq!(served, (height, best), "tip tree state (height, hash): zaino vs zebrad");
    let latest = indexer.get_latest_tree_state().await?;
    assert_eq!(latest, tree, "GetLatestTreeState vs GetTreeState({height})");
    Ok(())
}
