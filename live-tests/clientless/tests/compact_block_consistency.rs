//! Per-block consistency between served compact-block content and its chain metadata.
//!
//! A compact block's `chainMetadata` commitment-tree sizes are cumulative counts of the
//! note commitments the chain has produced. A scanning wallet advances its trees by the
//! actions/outputs each served block carries, so whenever a served block's tree-size
//! delta disagrees with its served commitment count the wallet observes a tree-size
//! discontinuity and treats it as a chain reorg. This walk pins that invariant per
//! block, per pool, for the request shape real (including pre-Ironwood) light clients
//! send: an empty `poolTypes` filter.

use zaino_common::network::ActivationHeights;
use zaino_fetch::jsonrpsee::response::GetBlockResponse;
#[allow(deprecated)]
use zaino_state::ZcashIndexer as _;
use zaino_testutils::{
    MinerPool, Rpc, TestManager, ValidatorKind, IRONWOOD_ONLY_ACTIVATION_HEIGHTS,
    NU6_3_TRANSITION_BOUNDARY, ORCHARD_ONLY_ACTIVATION_HEIGHTS,
    ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
};
use zcash_local_net::validator::zebrad::Zebrad;
use zebra_chain::serialization::ZcashDeserialize as _;

/// multi_thread required: the test manager spawns the validator and indexer services.
#[allow(deprecated)]
#[tokio::test(flavor = "multi_thread")]
async fn unfiltered_compact_blocks_match_chain_metadata_zebrad() {
    let mut test_manager = TestManager::<Zebrad, Rpc>::launch_mining_to(
        // Shielded mining: from NU6.3 an orchard-receiver coinbase is built as Ironwood
        // actions (the coinbase's Orchard component must be empty from NU6.3), so every
        // generated block carries ironwood data for the walk to check. A transparent
        // miner would leave the ironwood assertions vacuous.
        MinerPool::Orchard,
        &ValidatorKind::Zebrad,
        None,
        Some(IRONWOOD_ONLY_ACTIVATION_HEIGHTS),
        None,
        true,
        false,
        false,
    )
    .await
    .unwrap();
    let subscriber = test_manager.subscriber().clone();

    test_manager
        .generate_blocks_and_wait_for_tip(8, &subscriber)
        .await;
    let tip = u64::from(subscriber.chain_height().await.unwrap().0);

    // The empty pool filter is what unfiltered (pre-Ironwood) clients send; the served
    // stream must include every shielded pool's actions.
    let blocks = zaino_testutils::collect_block_range(&subscriber, 0, tip, vec![]).await;
    assert!(!blocks.is_empty(), "no compact blocks served");

    let connector = test_manager.full_node_jsonrpc_connector().await;

    let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) = (0u32, 0u32, 0u32);
    let mut total_orchard_actions = 0usize;
    let mut total_ironwood_actions = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        assert_eq!(
            block.height, index as u64,
            "served blocks must be contiguous from genesis for the walk's running totals"
        );
        let metadata = block
            .chain_metadata
            .as_ref()
            .expect("every served compact block carries chain metadata");

        let sapling_outputs: u32 = block.vtx.iter().map(|tx| tx.outputs.len() as u32).sum();
        let orchard_actions: u32 = block.vtx.iter().map(|tx| tx.actions.len() as u32).sum();
        let ironwood_actions: u32 = block
            .vtx
            .iter()
            .map(|tx| tx.ironwood_actions.len() as u32)
            .sum();
        total_orchard_actions += orchard_actions as usize;
        total_ironwood_actions += ironwood_actions as usize;

        assert_eq!(
            metadata.sapling_commitment_tree_size,
            prev_sapling + sapling_outputs,
            "sapling tree-size delta must equal the served output count at height {}",
            block.height
        );
        assert_eq!(
            metadata.orchard_commitment_tree_size,
            prev_orchard + orchard_actions,
            "orchard tree-size delta must equal the served action count at height {}",
            block.height
        );
        // The regression this walk exists for: a served block whose metadata counts
        // commitments from actions the block omits (e.g. ironwood stripped from an
        // unfiltered request) reads to a scanning wallet as a phantom chain reorg.
        assert_eq!(
            metadata.ironwood_commitment_tree_size,
            prev_ironwood + ironwood_actions,
            "ironwood tree-size delta must equal the served action count at height {}",
            block.height
        );

        // Clientless-exclusive predicate — oracle parity: zebrad's verbose getblock
        // reports the validator's own per-block tree sizes, an independent
        // implementation's answer to compare zaino's served metadata against. Package
        // tests cannot express this: their "source of truth" is the object being
        // served, so any such comparison is circular.
        // Verbosity 1: txids-as-strings plus the `trees` field the oracle needs
        // (verbosity 2 returns full transaction objects, which BlockObject's
        // string-typed `tx` field rejects).
        let oracle_trees = match connector
            .get_block(block.height.to_string(), Some(1))
            .await
            .expect("validator serves verbose blocks")
        {
            GetBlockResponse::Object(block_object) => block_object.trees,
            other => panic!("verbosity-2 getblock must return a block object, got {other:?}"),
        };
        assert_eq!(
            u64::from(metadata.sapling_commitment_tree_size),
            oracle_trees.sapling(),
            "served sapling tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            u64::from(metadata.orchard_commitment_tree_size),
            oracle_trees.orchard(),
            "served orchard tree size must match the validator's own at height {}",
            block.height
        );
        assert_eq!(
            u64::from(metadata.ironwood_commitment_tree_size),
            oracle_trees.ironwood(),
            "served ironwood tree size must match the validator's own at height {}",
            block.height
        );

        prev_sapling = metadata.sapling_commitment_tree_size;
        prev_orchard = metadata.orchard_commitment_tree_size;
        prev_ironwood = metadata.ironwood_commitment_tree_size;
    }

    assert!(
        total_ironwood_actions > 0,
        "the fixture produced no ironwood actions; the walk asserted nothing about ironwood"
    );
    // The counterpart of the guard above: the miner *asked* for Orchard, and from NU6.3
    // consensus requires an empty Orchard coinbase component, routing the reward into
    // Ironwood actions instead. With coinbase-only blocks, served orchard actions must
    // therefore be exactly zero. Together the two totals distinguish failure modes:
    // pool-swap (orchard > 0, ironwood == 0: ironwood served under the orchard field)
    // vs pool-drop (both zero) vs a broken routing premise.
    assert_eq!(
        total_orchard_actions, 0,
        "an Orchard-receiver coinbase must carry no Orchard actions from NU6.3"
    );

    test_manager.close().await;
}

/// Class-1 (consensus) predicate: in the NU5-through-NU6.2 era, a shielded
/// (orchard-receiver) miner's coinbase carries the reward as Orchard actions.
///
/// The action count uses `>= 1` rather than the padded exact count so the predicate
/// does not couple to the Orchard bundle-padding rule.
fn is_valid_orchard_coinbase(block: &zebra_chain::block::Block) -> bool {
    let Some(coinbase) = block.transactions.first() else {
        return false;
    };
    coinbase.is_coinbase()
        && coinbase.version() == 5
        && coinbase.sapling_outputs().count() == 0
        && coinbase.orchard_actions().count() >= 1
        && coinbase.ironwood_actions().count() == 0
}

/// Class-1 (consensus) predicate: from NU6.3 the same miner's coinbase must have an
/// empty Orchard component, with the reward routed to Ironwood actions instead.
///
/// Known open question at the activation boundary: the first NU6.3 block's coinbase
/// has been observed served as Orchard (one action, zero ironwood) — see
/// <https://github.com/zingolabs/zaino/issues/1368> for the hypotheses (zebrad
/// builder off-by-one vs missing routing vs a zaino pool-swap). This predicate over
/// raw validator blocks is that issue's disambiguator.
fn is_valid_ironwood_coinbase(block: &zebra_chain::block::Block) -> bool {
    let Some(coinbase) = block.transactions.first() else {
        return false;
    };
    coinbase.is_coinbase()
        && coinbase.version() == 6
        && coinbase.sapling_outputs().count() == 0
        && coinbase.orchard_actions().count() == 0
        && coinbase.ironwood_actions().count() >= 1
}

/// One-line coinbase summary for assertion messages, so a predicate failure names the
/// violated clause instead of reporting a bare boolean.
fn describe_coinbase(block: &zebra_chain::block::Block) -> String {
    match block.transactions.first() {
        Some(coinbase) => format!(
            "coinbase: v{}, sapling outputs {}, orchard actions {}, ironwood actions {}, txs in block {}",
            coinbase.version(),
            coinbase.sapling_outputs().count(),
            coinbase.orchard_actions().count(),
            coinbase.ironwood_actions().count(),
            block.transactions.len(),
        ),
        None => "block has no transactions".to_string(),
    }
}

/// Which shielded pool the fixture's coinbase reward must land in at a given height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoinbaseEra {
    /// Below NU5: the shielded-receiver reward cannot land in either pool.
    Neither,
    /// NU5 through NU6.2: the orchard-receiver reward is paid as Orchard actions.
    Orchard,
    /// From NU6.3: the same receiver's reward is routed to Ironwood actions.
    Ironwood,
}

/// Launches an orchard-receiver mining fixture on `activation_heights`, generates
/// `blocks` blocks, and asserts each height's raw validator block satisfies exactly
/// the era predicate `expected_era` assigns it. Raw blocks come straight from the
/// validator — zaino runs (the harness needs its subscriber as the block-generation
/// pollable) but is never consulted, so a failure here is a class-1
/// (consensus/routing) or fixture fact, never a zaino one.
///
/// Mismatches are collected across the whole chain and reported together rather than
/// aborting at the first failing height, so one run distinguishes the hypotheses of
/// <https://github.com/zingolabs/zaino/issues/1368>: a boundary-only mismatch is the
/// zebrad builder off-by-one (A1), every-post-activation-height mismatches mean the
/// routing is absent (A2), and a clean pass here while the e2e wire tests fail moves
/// the blame to a zaino pool-swap (B).
async fn assert_coinbase_routing(
    activation_heights: ActivationHeights,
    blocks: u32,
    expected_era: impl Fn(u64) -> CoinbaseEra,
) {
    #[allow(deprecated)]
    let mut test_manager = TestManager::<Zebrad, Rpc>::launch_mining_to(
        MinerPool::Orchard,
        &ValidatorKind::Zebrad,
        None,
        Some(activation_heights),
        None,
        true,
        false,
        false,
    )
    .await
    .unwrap();
    let subscriber = test_manager.subscriber().clone();

    test_manager
        .generate_blocks_and_wait_for_tip(blocks, &subscriber)
        .await;
    let tip = u64::from(subscriber.chain_height().await.unwrap().0);

    let connector = test_manager.full_node_jsonrpc_connector().await;
    let mut violations: Vec<String> = Vec::new();
    for height in 0..=tip {
        let block = match connector
            .get_block(height.to_string(), Some(0))
            .await
            .unwrap()
        {
            GetBlockResponse::Raw(raw) => {
                zebra_chain::block::Block::zcash_deserialize(raw.as_ref())
                    .expect("validator serves deserializable blocks")
            }
            other => panic!("verbosity-0 getblock must return a raw block, got {other:?}"),
        };

        let expected = expected_era(height);
        let (want_orchard, want_ironwood) = match expected {
            CoinbaseEra::Neither => (false, false),
            CoinbaseEra::Orchard => (true, false),
            CoinbaseEra::Ironwood => (false, true),
        };
        let got_orchard = is_valid_orchard_coinbase(&block);
        let got_ironwood = is_valid_ironwood_coinbase(&block);
        if got_orchard != want_orchard || got_ironwood != want_ironwood {
            violations.push(format!(
                "height {height}: expected {expected:?}, predicates say \
                 (orchard: {got_orchard}, ironwood: {got_ironwood}) — {}",
                describe_coinbase(&block)
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "coinbase routing mismatches ({} of {} heights; see \
         https://github.com/zingolabs/zaino/issues/1368 for the hypothesis map):\n{}",
        violations.len(),
        tip + 1,
        violations.join("\n")
    );

    test_manager.close().await;
}

/// Orchard-only era: NU6.3 never activates, so every post-NU5 coinbase stays an
/// Orchard coinbase and no ironwood ever appears. (The zebrad *default* heights are
/// now the canonical NU6.3-at-2 set, so this fixture is explicit.)
///
/// multi_thread required: the test manager spawns the validator and indexer services.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_only_coinbase_routing_zebrad() {
    assert_coinbase_routing(ORCHARD_ONLY_ACTIVATION_HEIGHTS, 6, |height| {
        if height >= 2 {
            CoinbaseEra::Orchard
        } else {
            CoinbaseEra::Neither
        }
    })
    .await;
}

/// Ironwood-only era: NU6.3 active from height 2 (with every prior upgrade), so every
/// post-activation coinbase is an Ironwood coinbase and no Orchard coinbase ever
/// appears.
///
/// multi_thread required: the test manager spawns the validator and indexer services.
#[tokio::test(flavor = "multi_thread")]
async fn ironwood_only_coinbase_routing_zebrad() {
    assert_coinbase_routing(IRONWOOD_ONLY_ACTIVATION_HEIGHTS, 6, |height| {
        if height >= 2 {
            CoinbaseEra::Ironwood
        } else {
            CoinbaseEra::Neither
        }
    })
    .await;
}

/// The transition: the same unchanged orchard-receiver miner produces Orchard
/// coinbases through NU6.2 and Ironwood coinbases from the NU6.3 activation height —
/// each predicate exactly delimiting its era, so a mis-timed flip fails on both sides
/// of the boundary.
///
/// multi_thread required: the test manager spawns the validator and indexer services.
#[tokio::test(flavor = "multi_thread")]
async fn orchard_coinbase_routing_flips_to_ironwood_at_activation_zebrad() {
    // Two blocks past the boundary, so both eras carry more than one block.
    assert_coinbase_routing(
        ORCHARD_THEN_IRONWOOD_ACTIVATION_HEIGHTS,
        NU6_3_TRANSITION_BOUNDARY + 2,
        |height| {
            if height >= u64::from(NU6_3_TRANSITION_BOUNDARY) {
                CoinbaseEra::Ironwood
            } else if height >= 2 {
                CoinbaseEra::Orchard
            } else {
                CoinbaseEra::Neither
            }
        },
    )
    .await;
}
