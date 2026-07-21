//! Per-block consistency between served compact-block content and its chain metadata.
//!
//! A compact block's `chainMetadata` commitment-tree sizes are cumulative counts of the
//! note commitments the chain has produced. A scanning wallet advances its trees by the
//! actions/outputs each served block carries, so whenever a served block's tree-size
//! delta disagrees with its served commitment count the wallet observes a tree-size
//! discontinuity and treats it as a chain reorg. This walk pins that invariant per
//! block, per pool, for the request shape unfiltered light clients send: an empty
//! `poolTypes` filter.
//!
//! Scope note (ztest port): the sapling/orchard walk runs today. The Ironwood / NU6.3
//! walk ([`compact_blocks_carry_ironwood_after_nu6_3_zebrad`]) is fully ported against
//! ztest's NU6.3 topology and ironwood proto, but is `#[ignore]`d until an NU6.3-capable
//! validator image is wired into ztest (a real version filled into `ZEBRAD_NU6_3_RELEASE`
//! and the zainod NU6.3 release). See <https://github.com/zingolabs/zaino/issues/1368>.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(120);

/// The validator's own `(sapling, orchard, ironwood)` commitment-tree sizes at `height`,
/// read from its verbose `getblock` `trees` field. This is the independent oracle:
/// zebrad's answer, computed without reference to what zaino serves, so the comparison is
/// not circular. zebra omits a pool's key when its tree is empty, so a missing key reads
/// as size 0 — which is also how `ironwood` reads on a pre-NU6.3 node.
async fn oracle_trees(validator: &impl ValidatorBackend, height: u64) -> Result<(u64, u64, u64)> {
    let block = validator
        .json_rpc()
        .await?
        .call_value("getblock", json!([height.to_string(), 1]))
        .await?;
    let trees = block
        .get("trees")
        .context("verbose getblock must carry a trees field")?;
    let size = |pool: &str| {
        trees
            .get(pool)
            .and_then(|t| t.get("size"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Ok((size("sapling"), size("orchard"), size("ironwood")))
}

/// integration tier: spawns a zebrad validator + a zainod indexer and mines shielded
/// coinbases (see [[ztest-validator-tests-need-integration-tier]] — the Basic tier
/// OOM-kills zebrad).
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn unfiltered_compact_blocks_match_chain_metadata_zebrad() -> Result<()> {
    // Orchard-receiver coinbase: from NU5 every generated block carries orchard actions
    // for the walk to check. A transparent miner would leave the orchard assertions
    // vacuous.
    //
    // Pin the chain to NU6.1: the `zfnd/zebra:5.2.0` image bundles a zcash_protocol
    // that predates the NU6.2 consensus branch-id, so its `coinbase_outputs_are_decryptable`
    // check (`to_librustzcash(Nu6_2)` → `BranchId::try_from`) rejects a *shielded* coinbase
    // at the NU6.2 activation height (7) — "block was rejected: Rejected". Transparent
    // coinbases skip that check, so only shielded-coinbase walks hit it. Capping the ceiling
    // at NU6.1 keeps every mined height on a branch-id the image understands. Drop the cap
    // once the zebrad image is bumped to one linking zcash_protocol ≥ 0.9.0.
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(
        Validator::zebrad("6.2.0")
            .regtest()
            .mine_to(Pool::Orchard)
            .activate_through(NetworkUpgrade::Nu6_1),
    );
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    let tip = validator.generate_blocks(8).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    // The empty pool filter is what unfiltered clients send; the served stream must
    // include every shielded pool's commitments.
    let start = BlockHeight::from(1u32);
    let blocks = indexer.get_block_range(start, tip).await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    // Seed the running totals from the validator's own trees at the height below the
    // first served block, so the per-block delta check stays oracle-independent even
    // though the pod gRPC serves from height 1 rather than genesis.
    let (mut prev_sapling, mut prev_orchard, _) = oracle_trees(&validator, 0).await?;
    let mut total_orchard_actions = 0u64;
    for (index, block) in blocks.iter().enumerate() {
        // The served range starts at height 1, so the block at offset `index` must be
        // at height `index + 1` for the walk's running totals to line up.
        assert_eq!(
            block.height,
            index as u64 + 1,
            "served blocks must be contiguous for the walk's running totals"
        );

        let metadata = block
            .chain_metadata
            .as_ref()
            .context("every served compact block carries chain metadata")?;
        let sapling_outputs: u64 = block.vtx.iter().map(|tx| tx.outputs.len() as u64).sum();
        let orchard_actions: u64 = block.vtx.iter().map(|tx| tx.actions.len() as u64).sum();
        total_orchard_actions += orchard_actions;

        let sapling_size = u64::from(metadata.sapling_commitment_tree_size);
        let orchard_size = u64::from(metadata.orchard_commitment_tree_size);

        // The regression this walk exists for: a served block whose metadata counts
        // commitments from actions the block omits reads to a scanning wallet as a
        // phantom chain reorg.
        assert_eq!(
            sapling_size,
            prev_sapling + sapling_outputs,
            "sapling tree-size delta must equal the served output count at height {}",
            block.height
        );
        assert_eq!(
            orchard_size,
            prev_orchard + orchard_actions,
            "orchard tree-size delta must equal the served action count at height {}",
            block.height
        );

        // Clientless-exclusive predicate — oracle parity: zebrad's verbose getblock
        // reports the validator's own per-block tree sizes, an independent
        // implementation's answer to compare zaino's served metadata against. Package
        // tests cannot express this: their source of truth is the object being served,
        // so any such comparison is circular.
        let (oracle_sapling, oracle_orchard, _) = oracle_trees(&validator, block.height).await?;
        assert_eq!(
            sapling_size, oracle_sapling,
            "served sapling tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            orchard_size, oracle_orchard,
            "served orchard tree size must match the validator's own at height {}",
            block.height
        );

        prev_sapling = sapling_size;
        prev_orchard = orchard_size;
    }

    assert!(
        total_orchard_actions > 0,
        "the orchard-receiver fixture produced no orchard actions; the walk asserted \
         nothing about orchard"
    );

    Ok(())
}

/// Ironwood / NU6.3 companion to the sapling/orchard walk above. Ports dev's
/// `ironwood_*` coinbase-routing coverage and the ironwood tree-size delta onto ztest.
///
/// With NU6.3 active at ztest's canonical regtest height (8) and an orchard-receiver
/// miner, the coinbase reward routes to Orchard actions through NU6.2 (heights 2..=7) and
/// to Ironwood actions from NU6.3 (heights >= 8), its Orchard component then empty. So one
/// `activate_through(Nu6_3)` chain exercises both eras and the flip at the boundary — the
/// [#1368] disambiguator — without needing a caller-chosen activation height. This pins,
/// per block: the ironwood tree-size delta equals the served ironwood-action count; oracle
/// parity for the ironwood tree size against the validator's own `getblock` `.trees`; and
/// the coinbase pool-routing flip.
///
/// `#[ignore]`d until an NU6.3-capable validator image is wired into ztest:
/// `activate_through(Nu6_3)` errors at `env.build()` while every component's
/// `*_NU6_3_RELEASE` is an unreachable sentinel (no image reports an NU6.3 ceiling, so the
/// resolver floors the topology below NU6.3). Un-ignore once a real NU6.3 zebra image
/// version is filled into ztest's `ZEBRAD_NU6_3_RELEASE` (and the zainod NU6.3 release),
/// and the zebra `"NU6.3"` activation-heights render key is confirmed against the fork.
/// See <https://github.com/zingolabs/zaino/issues/1368>.
///
/// [#1368]: https://github.com/zingolabs/zaino/issues/1368
#[ignore = "needs an NU6.3-capable validator image wired into ztest — see #1368 and this fn's doc"]
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn compact_blocks_carry_ironwood_after_nu6_3_zebrad() -> Result<()> {
    // NU6.3 active (canonical height 8) + orchard-receiver coinbase: heights 2..=7 carry
    // Orchard actions, heights >= 8 carry Ironwood actions.
    let mut env = TestEnv::builder().ready_timeout(READY);
    let validator = env.add_validator(
        Validator::zebrad("6.2.0")
            .regtest()
            .mine_to(Pool::Orchard)
            .activate_through(NetworkUpgrade::Nu6_3),
    );
    let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
    env.build().await?;

    // Mine past the NU6.3 activation height (8) so both eras carry more than one block.
    let tip = validator.generate_blocks(10).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    let start = BlockHeight::from(1u32);
    let blocks = indexer.get_block_range(start, tip).await?;
    assert!(!blocks.is_empty(), "no compact blocks served");

    let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) =
        oracle_trees(&validator, 0).await?;
    let mut total_orchard_actions = 0u64;
    let mut total_ironwood_actions = 0u64;
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height,
            index as u64 + 1,
            "served blocks must be contiguous for the walk's running totals"
        );

        let metadata = block
            .chain_metadata
            .as_ref()
            .context("every served compact block carries chain metadata")?;
        let sapling_outputs: u64 = block.vtx.iter().map(|tx| tx.outputs.len() as u64).sum();
        let orchard_actions: u64 = block.vtx.iter().map(|tx| tx.actions.len() as u64).sum();
        let ironwood_actions: u64 = block
            .vtx
            .iter()
            .map(|tx| tx.ironwood_actions.len() as u64)
            .sum();
        total_orchard_actions += orchard_actions;
        total_ironwood_actions += ironwood_actions;

        let sapling_size = u64::from(metadata.sapling_commitment_tree_size);
        let orchard_size = u64::from(metadata.orchard_commitment_tree_size);
        let ironwood_size = u64::from(metadata.ironwood_commitment_tree_size);

        // Per-pool tree-size delta must equal the served commitment count (the
        // phantom-reorg regression) — for all three shielded pools, including ironwood.
        assert_eq!(
            sapling_size,
            prev_sapling + sapling_outputs,
            "sapling tree-size delta must equal the served output count at height {}",
            block.height
        );
        assert_eq!(
            orchard_size,
            prev_orchard + orchard_actions,
            "orchard tree-size delta must equal the served action count at height {}",
            block.height
        );
        assert_eq!(
            ironwood_size,
            prev_ironwood + ironwood_actions,
            "ironwood tree-size delta must equal the served action count at height {}",
            block.height
        );

        // Oracle parity against the validator's own tree sizes, including ironwood.
        let (oracle_sapling, oracle_orchard, oracle_ironwood) =
            oracle_trees(&validator, block.height).await?;
        assert_eq!(
            sapling_size, oracle_sapling,
            "served sapling tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            orchard_size, oracle_orchard,
            "served orchard tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            ironwood_size, oracle_ironwood,
            "served ironwood tree size must match the validator's own at height {}",
            block.height
        );

        // Coinbase pool-routing flip across the NU6.3 boundary. Blocks are coinbase-only,
        // so the block's action totals are the coinbase's.
        if (2..=7).contains(&block.height) {
            assert!(
                orchard_actions > 0,
                "NU5..NU6.2 coinbase must carry Orchard actions at height {}",
                block.height
            );
            assert_eq!(
                ironwood_actions, 0,
                "no Ironwood actions before NU6.3 at height {}",
                block.height
            );
        } else if block.height >= 8 {
            assert!(
                ironwood_actions > 0,
                "NU6.3 coinbase must carry Ironwood actions at height {}",
                block.height
            );
            assert_eq!(
                orchard_actions, 0,
                "an NU6.3 orchard-receiver coinbase must have an empty Orchard component \
                 at height {}",
                block.height
            );
        }

        prev_sapling = sapling_size;
        prev_orchard = orchard_size;
        prev_ironwood = ironwood_size;
    }

    assert!(
        total_orchard_actions > 0,
        "the orchard era (NU5..NU6.2) produced no orchard actions"
    );
    assert!(
        total_ironwood_actions > 0,
        "the ironwood era (NU6.3+) produced no ironwood actions"
    );

    Ok(())
}
