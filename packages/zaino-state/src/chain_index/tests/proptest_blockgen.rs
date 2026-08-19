use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use zaino_chain_head::ChainHeadSnapshot as _;

use futures::stream::FuturesUnordered;
use proptest::{
    prelude::{Arbitrary as _, BoxedStrategy, Just},
    strategy::Strategy,
};
use rand::seq::IndexedRandom;
use tokio_stream::StreamExt as _;
use zaino_common::{network::ActivationHeights, DatabaseConfig, StorageConfig};
use zebra_chain::{
    block::arbitrary::{self, LedgerStateOverride},
    fmt::SummaryDebug,
    serialization::ZcashSerialize,
    transaction::SerializedTransaction,
    LedgerState,
};

use crate::{
    chain_index::{
        finalized_height_floor,
        source::GetTransactionLocation,
        tests::{init_tracing, poll::poll_until, proptest_blockgen::proptest_helpers::add_segment},
        types::BestChainLocation,
        OPERATIONAL_NFS_DEPTH,
    },
    ChainIndex, ChainIndexConfig, ChainIndexRpcExt, Height, NodeBackedChainIndex,
    NodeBackedChainIndexSubscriber, TransactionHash,
};

use zaino_proto::proto::utils::PoolTypeFilter;

/// Chain length per generated segment in the passthrough harness — long enough to
/// have some finalised blocks to play with. The best chain is twice this (genesis
/// segment plus one branch), so its expected tip height is
/// `2 * PASSTHROUGH_SEGMENT_LENGTH - 1`.
const PASSTHROUGH_SEGMENT_LENGTH: usize = OPERATIONAL_NFS_DEPTH as usize + 20;

/// Handle all the boilerplate for a passthrough
fn passthrough_test(
    // The actual assertions. Takes as args:
    test: impl AsyncFn(
        // The mockchain, to use a a source of truth
        &ValidatorSource<ProptestMockchain>,
        // The subscriber to test against
        NodeBackedChainIndexSubscriber<ValidatorSource<ProptestMockchain>>,
        // A snapshot, which will have only the genesis block
        &std::sync::Arc<crate::MapBackedSnapshot>,
    ),
) {
    passthrough_test_on(
        ActivationHeights::default().to_regtest_network(),
        // A small delay keeps source calls genuinely asynchronous, so the
        // concurrency in the paths under test is exercised rather than
        // collapsed into immediate returns.
        //
        // It used to be 100ms, chosen to hold the indexer in passthrough while
        // the assertions ran. There is no passthrough state to hold it in any
        // more — the chain head is populated from the moment the index exists —
        // and at that magnitude the delay simply multiplied by the number of
        // blocks the chain head walks, costing tens of seconds per case for no
        // assertion.
        Some(Duration::from_millis(2)),
        |_| {},
        test,
    )
}

/// [`passthrough_test`] on an explicit network, with a per-segment chain mutator.
///
/// The mutator exists because zebra's stock `Transaction` strategy generates V6
/// transactions only probabilistically (its NU6.3/NU7 arm picks one of v4/v5/v6 per
/// transaction), so deterministic ironwood-era content must be injected after
/// generation. Mutating a block's transactions is safe here: the
/// block hash covers only the header, so parent-hash continuity is untouched, and the
/// header's merkle root is already arbitrary — the passthrough path tolerates that by
/// construction.
fn passthrough_test_on(
    network: zebra_chain::parameters::Network,
    source_delay: Option<Duration>,
    mutate_segment: impl Fn(&mut Vec<Arc<zebra_chain::block::Block>>),
    test: impl AsyncFn(
        &ValidatorSource<ProptestMockchain>,
        NodeBackedChainIndexSubscriber<ValidatorSource<ProptestMockchain>>,
        &std::sync::Arc<crate::MapBackedSnapshot>,
    ),
) {
    init_tracing();
    let segment_length = PASSTHROUGH_SEGMENT_LENGTH;
    // No need to worry about non-best chains for this test
    let branch_count = 1;

    // from this line to `runtime.block_on(async {` are all
    // copy-pasted. Could a macro get rid of some of this boilerplate?
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network.clone()))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (mut genesis_segment, mut branching_segments) = segments;
            mutate_segment(&mut genesis_segment.0);
            for segment in &mut branching_segments {
                mutate_segment(&mut segment.0);
            }
            let mockchain = wrap_proptest_mockchain(ProptestMockchain {
                genesis_segment,
                branching_segments,
                delay: source_delay,
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
                roots_cache: Arc::new(Mutex::new(HashMap::new())),
            }, network.clone());
            let temp_dir: tempfile::TempDir = tempfile::tempdir().unwrap();
            let db_path: std::path::PathBuf = temp_dir.path().to_path_buf();

            let config = ChainIndexConfig {
                storage: StorageConfig {
                    database: DatabaseConfig {
                        path: db_path,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ephemeral: true,
                mempool: Default::default(),
                db_version: 1,
                network: network.clone(),

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            let index_reader = indexer.subscriber();
            // The best chain is `2 * segment_length` blocks (genesis segment +
            // one branch), so its tip height is `2 * segment_length - 1`.
            //
            // These cases used to wait for the *finalised floor*, because a
            // still-syncing snapshot reported that as the highest height it
            // could serve and everything above it went to the validator by
            // passthrough. The chain head serves to the chain tip from the
            // moment the index exists, so the tip is what to wait for — and
            // what these queries now exercise is the chain head rather than the
            // passthrough that used to answer them.
            let tip_height = (2 * segment_length - 1) as u32;
            // Poll rather than sleeping a fixed 5 s: with a 1 s per-block
            // source delay (above) the chain head reaches the tip well inside
            // that, but it can be longer under parallel-suite scheduler
            // pressure.
            poll_until(
                "chain head to reach the source's chain tip",
                Duration::from_secs(30),
                Duration::from_millis(50),
                || async {
                    let snapshot = index_reader.snapshot_nonfinalized_state();
                    (u32::from(snapshot.best_tip().height) == tip_height).then_some(())
                },
            )
            .await;
            let snapshot = index_reader.snapshot_nonfinalized_state();
            assert_eq!(u32::from(snapshot.best_tip().height), tip_height);

            test(&mockchain, index_reader, &snapshot).await;




        });
    })
}

#[test]
fn passthrough_find_fork_point() {
    // TODO: passthrough_test handles a good chunck of boilerplate, but there's
    // still a lot more inside of the closures being passed to passthrough_test.
    // Can we DRY out more of it?
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This allows the artificial delays to happen in parallel
        let mut parallel = FuturesUnordered::new();
        // As we only have one branch, arbitrary branch order is fine
        for (height, hash) in mockchain
            .source()
            .all_blocks_arb_branch_order()
            .map(|block| (block.coinbase_height().unwrap(), block.hash()))
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let fork_point = index_reader
                    .find_fork_point(&snapshot, &hash.into())
                    .await
                    .unwrap();

                if height <= crate::Height(u32::from(snapshot.best_tip().height)) {
                    // passthrough fork point can only ever be the requested block
                    // as we don't passthrough to nonfinalized state
                    assert_eq!(hash, fork_point.unwrap().0);
                    assert_eq!(height, fork_point.unwrap().1);
                } else {
                    assert!(fork_point.is_none());
                }
            })
        }
        while let Some(_success) = parallel.next().await {}
    });
}

#[test]
fn passthrough_get_transaction_status() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This allows the artificial delays to happen in parallel
        let mut parallel = FuturesUnordered::new();
        // As we only have one branch, arbitrary branch order is fine
        for (height, txid) in mockchain
            .source()
            .all_blocks_arb_branch_order()
            .flat_map(|block| {
                block
                    .transactions
                    .iter()
                    .map(|transaction| (block.coinbase_height().unwrap(), transaction.hash()))
                    .collect::<Vec<_>>()
            })
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let transaction_status = index_reader
                    .get_transaction_status(&snapshot, &txid.into())
                    .await
                    .unwrap();

                if height <= crate::Height(u32::from(snapshot.best_tip().height)) {
                    // passthrough transaction status can only ever be on the best
                    // chain as we don't passthrough to nonfinalized state
                    let Some(BestChainLocation::Block(_block_hash, transaction_height)) =
                        transaction_status.0
                    else {
                        panic!("expected best chain location")
                    };
                    assert_eq!(height, transaction_height);
                } else {
                    assert!(transaction_status.0.is_none());
                }
                assert!(transaction_status.1.is_empty());
            })
        }
        while let Some(_success) = parallel.next().await {}
    });
}

#[test]
fn passthrough_get_raw_transaction() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This allows the artificial delays to happen in parallel
        let mut parallel = FuturesUnordered::new();
        // As we only have one branch, arbitrary branch order is fine
        for (expected_transaction, height) in mockchain
            .source()
            .all_blocks_arb_branch_order()
            .flat_map(|block| {
                block
                    .transactions
                    .iter()
                    .map(|transaction| (transaction, block.coinbase_height().unwrap()))
                    .collect::<Vec<_>>()
            })
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let actual_transaction = index_reader
                    .get_raw_transaction(
                        &snapshot,
                        &TransactionHash::from(expected_transaction.hash()),
                    )
                    .await
                    .unwrap();
                let Some((raw_transaction, _branch_id)) = actual_transaction else {
                    panic!("missing transaction at height {}", height.0)
                };
                assert_eq!(
                    raw_transaction,
                    SerializedTransaction::from(expected_transaction.clone()).as_ref()
                )
            })
        }
        while let Some(_success) = parallel.next().await {}
    });
}

/// The reported tip is the source's own tip.
///
/// This used to expect the *finalised floor*: with no non-finalised state yet,
/// the highest height the index would admit to was the seam, and everything
/// above it was passthrough. The chain head tracks the source's tip directly,
/// so that is what the index now reports.
#[test]
fn passthrough_best_chaintip() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        let tip = index_reader.best_chaintip(snapshot).await.unwrap();
        assert_eq!(
            tip.height.0,
            mockchain
                .source()
                .best_branch()
                .last()
                .unwrap()
                .coinbase_height()
                .map(|h| h.0)
                .unwrap()
        );
    })
}

#[test]
fn passthrough_get_block_height() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This allows the artificial delays to happen in parallel
        let mut parallel = FuturesUnordered::new();

        for (expected_height, hash) in mockchain
            .source()
            .all_blocks_arb_branch_order()
            .map(|block| (block.coinbase_height().unwrap(), block.hash()))
        {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let height = index_reader
                    .get_block_height(&snapshot, hash.into())
                    .await
                    .unwrap();
                if expected_height <= crate::Height(u32::from(snapshot.best_tip().height)) {
                    assert_eq!(height, Some(expected_height.into()));
                } else {
                    assert_eq!(height, None);
                }
            });
        }
        while let Some(_success) = parallel.next().await {}
    })
}

#[test]
fn passthrough_get_block_range() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        // We use a futures-unordered instead of only a for loop
        // as this lets us call all the get_raw_transaction requests
        // at the same time and wait for them in parallel
        //
        // This allows the artificial delays to happen in parallel
        let mut parallel = FuturesUnordered::new();

        for expected_start_height in mockchain
            .source()
            .all_blocks_arb_branch_order()
            .map(|block| block.coinbase_height().unwrap())
        {
            let expected_end_height = (expected_start_height + 9).unwrap();
            if expected_end_height.0 as usize
                <= mockchain.source().all_blocks_arb_branch_order().count()
            {
                let index_reader = index_reader.clone();
                let snapshot = snapshot.clone();
                parallel.push(async move {
                    let block_range_stream = index_reader.get_block_range(
                        &snapshot,
                        expected_start_height.into(),
                        Some(expected_end_height.into()),
                    );
                    if expected_start_height <= crate::Height(u32::from(snapshot.best_tip().height))
                    {
                        let mut block_range_stream = Box::pin(block_range_stream.unwrap());
                        let mut num_blocks_in_stream = 0;
                        while let Some(block) = block_range_stream.next().await {
                            let expected_block = mockchain
                                .source()
                                .all_blocks_arb_branch_order()
                                .nth(expected_start_height.0 as usize + num_blocks_in_stream)
                                .unwrap()
                                .zcash_serialize_to_vec()
                                .unwrap();
                            assert_eq!(block.unwrap(), expected_block);
                            num_blocks_in_stream += 1;
                        }
                        assert_eq!(
                            num_blocks_in_stream,
                            // expect 10 blocks
                            10.min(
                                // unless the provided range overlaps the finalized boundary.
                                // in that case, expect all blocks between start height
                                // and finalized height, (+1 for inclusive range)
                                u32::from(snapshot.best_tip().height)
                                    .saturating_sub(expected_start_height.0)
                                    + 1
                            ) as usize
                        );
                    } else {
                        assert!(block_range_stream.is_none())
                    }
                });
            }
        }
        while let Some(_success) = parallel.next().await {}
    })
}

/// Upstream capability guard: zebra-chain's stock [`Transaction`] strategy generates V6
/// transactions for an NU6.3 ledger state — its NU6.3/NU7 arm is
/// `prop_oneof![v4_strategy, v5_strategy, v6_strategy]` (zebra-chain
/// `transaction/arbitrary.rs`). Before zebra-chain 12.0 that arm carried no `v6_strategy`
/// and this test was a `should_panic` canary tracking the gap; the gap is now closed.
///
/// V6 generation is nonetheless *probabilistic* — one arm of three per transaction — so
/// the `passthrough_metadata_consistency_*` walks still inject `fake_v6_transaction`
/// ironwood content rather than relying on generation. Those walks assert their own
/// non-vacuity (`above > 0`), which probabilistic content would turn into a flake rather
/// than a silent pass.
///
/// If a future zebra release drops the V6 arm, this fails loudly and the injection's
/// justification reverts from "determinism" to "necessity".
///
/// [`Transaction`]: zebra_chain::transaction::Transaction
#[test]
fn zebra_arbitrary_generates_v6_transactions_for_nu6_3() {
    use proptest::strategy::ValueTree as _;
    use proptest::test_runner::TestRunner;
    use zebra_chain::parameters::NetworkUpgrade;

    let mut runner = TestRunner::default();

    let ledger = LedgerState::arbitrary_with(LedgerStateOverride {
        network_upgrade_override: Some(NetworkUpgrade::Nu6_3),
        ..LedgerStateOverride::default()
    })
    .new_tree(&mut runner)
    .expect("ledger strategy yields a value")
    .current();
    assert_eq!(ledger.network_upgrade(), NetworkUpgrade::Nu6_3);

    let transaction_strategy =
        zebra_chain::transaction::Transaction::arbitrary_with(ledger.clone());

    let mut generated_versions = std::collections::BTreeSet::new();
    for _ in 0..64 {
        let transaction = transaction_strategy
            .new_tree(&mut runner)
            .expect("transaction strategy yields a value")
            .current();
        generated_versions.insert(transaction.version());
    }

    assert!(
        generated_versions.contains(&6),
        "zebra's stock Transaction strategy generated no V6 transaction for an NU6.3 \
         ledger state in 64 samples (saw versions {generated_versions:?})"
    );
}

/// NU6.3 active from height 2, so post-activation generated blocks carry V6
/// transactions whose shielded data lands in the Ironwood pool.
const IRONWOOD_ONLY_HEIGHTS: ActivationHeights = ActivationHeights {
    before_overwinter: Some(1),
    overwinter: Some(1),
    sapling: Some(1),
    blossom: Some(1),
    heartwood: Some(1),
    canopy: Some(1),
    nu5: Some(2),
    nu6: Some(2),
    nu6_1: Some(2),
    nu6_2: Some(2),
    nu6_3: Some(2),
    nu7: None,
};

/// Per-block consistency between served compact-block content and its chain metadata.
///
/// A compact block's `chainMetadata` tree sizes are cumulative note-commitment counts;
/// a scanning wallet advances its trees by the actions/outputs each served block
/// carries, so a served block whose tree-size delta disagrees with its served
/// commitment count reads as a tree-size discontinuity — a phantom chain reorg. The
/// walk checks every serviceable height, for both the wire-decoded unfiltered request
/// (empty `poolTypes`, what real — including pre-Ironwood — clients send) and the
/// explicit all-pools filter, and cross-checks served counts against the mockchain
/// source of truth.
#[test]
fn passthrough_metadata_consistency_ironwood_only() {
    metadata_consistency_for_era(IRONWOOD_ONLY_HEIGHTS, Some(2), false)
}

/// Orchard-only heights: every upgrade through NU6.2 at height 2, NU6.3 never
/// activating.
const ORCHARD_ONLY_HEIGHTS: ActivationHeights = ActivationHeights {
    before_overwinter: Some(1),
    overwinter: Some(1),
    sapling: Some(1),
    blossom: Some(1),
    heartwood: Some(1),
    canopy: Some(1),
    nu5: Some(2),
    nu6: Some(2),
    nu6_1: Some(2),
    nu6_2: Some(2),
    nu6_3: None,
    nu7: None,
};

/// Orchard-only era (NU6.3 never activates): fake Orchard content from height 2, and —
/// since the stock strategy's V6 arm is reachable only from an NU6.3/NU7 ledger state,
/// which `nu6_3: None` never produces — ironwood provably never appears anywhere in the
/// chain or the served form.
#[test]
fn passthrough_metadata_consistency_orchard_only() {
    metadata_consistency_for_era(ORCHARD_ONLY_HEIGHTS, None, false)
}

/// The transition: fake Orchard content below the NU6.3 boundary, fake Ironwood
/// content from it. The boundary is placed inside the walked non-finalised window so
/// both eras are actually observed by the walk.
#[test]
fn passthrough_metadata_consistency_orchard_to_ironwood_transition() {
    let expected_tip = (2 * PASSTHROUGH_SEGMENT_LENGTH - 1) as u32;
    let boundary = expected_tip - (OPERATIONAL_NFS_DEPTH / 2);
    metadata_consistency_for_era(
        ActivationHeights {
            nu6_3: Some(boundary),
            ..IRONWOOD_ONLY_HEIGHTS
        },
        Some(boundary),
        true,
    )
}

/// A structurally-valid (cryptographically fake) V6 transaction carrying a two-action
/// Ironwood bundle. Injected because zebra's stock strategy generates V6 only
/// probabilistically, so era content must be deterministic here
/// (see [`zebra_arbitrary_generates_v6_transactions_for_nu6_3`]).
fn fake_ironwood_transaction() -> zebra_chain::transaction::Transaction {
    use zebra_chain::amount::Amount;
    use zebra_chain::orchard::{Flags, ShieldedDataV6};
    use zebra_chain::parameters::NetworkUpgrade;
    use zebra_chain::transaction::arbitrary::{fake_v6_orchard_shielded_data, fake_v6_transaction};

    let ironwood = zebra_chain::ironwood::ShieldedData::new(ShieldedDataV6::new(
        fake_v6_orchard_shielded_data(
            Flags::ENABLE_SPENDS,
            Amount::try_from(0).expect("zero is a valid amount"),
            2,
        ),
    ));
    fake_v6_transaction(NetworkUpgrade::Nu6_3, None, Some(ironwood))
}

/// A structurally-valid (cryptographically fake) V5 transaction carrying a two-action
/// Orchard bundle, for deterministic orchard-era content (the stock strategy's orchard
/// data is probabilistic).
fn fake_orchard_transaction() -> zebra_chain::transaction::Transaction {
    use zebra_chain::amount::Amount;
    use zebra_chain::orchard::Flags;
    use zebra_chain::parameters::NetworkUpgrade;
    use zebra_chain::transaction::arbitrary::fake_v6_orchard_shielded_data;
    use zebra_chain::transaction::{LockTime, Transaction};

    Transaction::V5 {
        network_upgrade: NetworkUpgrade::Nu5,
        lock_time: LockTime::unlocked(),
        expiry_height: zebra_chain::block::Height(0),
        inputs: Vec::new(),
        outputs: Vec::new(),
        sapling_shielded_data: None,
        orchard_shielded_data: Some(fake_v6_orchard_shielded_data(
            Flags::ENABLE_SPENDS,
            Amount::try_from(0).expect("zero is a valid amount"),
            2,
        )),
    }
}

/// Runs the metadata-consistency walk on a chain whose injected shielded content
/// follows the era layout:
///
/// - `ironwood_boundary: None` — orchard era only: fake Orchard content from height 2,
///   and ironwood must never appear anywhere;
/// - `ironwood_boundary: Some(b)` — fake Ironwood content from height `b`; when
///   `orchard_below_boundary` is set, fake Orchard content fills heights 2..b (the
///   transition layout), otherwise heights below `b` carry only generated content.
fn metadata_consistency_for_era(
    heights: ActivationHeights,
    ironwood_boundary: Option<u32>,
    orchard_below_boundary: bool,
) {
    let inject = move |blocks: &mut Vec<Arc<zebra_chain::block::Block>>| {
        for block in blocks.iter_mut() {
            let height = block
                .coinbase_height()
                .expect("generated blocks always have a coinbase height")
                .0;
            if height < 2 {
                continue;
            }
            let fake_tx = match ironwood_boundary {
                None => fake_orchard_transaction(),
                Some(boundary) if height >= boundary => fake_ironwood_transaction(),
                Some(_) if orchard_below_boundary => fake_orchard_transaction(),
                Some(_) => continue,
            };
            let mut new_block = (**block).clone();
            new_block.transactions.push(Arc::new(fake_tx));
            *block = Arc::new(new_block);
        }
    };

    passthrough_test_on(
        heights.to_regtest_network(),
        // No artificial source delay: this test waits for the indexer to finish
        // syncing, because compact blocks are not served while the finalised state
        // is still syncing (get_compact_block's StillSyncingFinalizedState arm).
        None,
        inject,
        async |mockchain, index_reader, _snapshot| {
            // Source of truth: per-height shielded commitment counts from the mockchain
            // blocks themselves (single branch, so arb branch order is chain order).
            let source_counts: Vec<(u32, u32, u32)> = mockchain
                .source()
                .all_blocks_arb_branch_order()
                .map(|block| {
                    let sapling = block
                        .transactions
                        .iter()
                        .map(|tx| tx.sapling_note_commitments().count() as u32)
                        .sum();
                    let orchard = block
                        .transactions
                        .iter()
                        .map(|tx| tx.orchard_note_commitments().count() as u32)
                        .sum();
                    let ironwood = block
                        .transactions
                        .iter()
                        .map(|tx| tx.ironwood_note_commitments().count() as u32)
                        .sum();
                    (sapling, orchard, ironwood)
                })
                .collect();

            // Era-composition guards on the source chain, so no assertion below can go
            // vacuously green (and no era leaks content into the other).
            match ironwood_boundary {
                None => {
                    let ironwood_total: u32 = source_counts.iter().map(|(_, _, i)| i).sum();
                    assert_eq!(
                        ironwood_total, 0,
                        "orchard-only era must carry no ironwood commitments"
                    );
                    let orchard_total: u32 = source_counts.iter().map(|(_, o, _)| o).sum();
                    assert!(
                        orchard_total > 0,
                        "orchard-only era carries no orchard commitments; the orchard \
                         assertions below would be vacuous"
                    );
                }
                Some(boundary) => {
                    let below: u32 = source_counts[..boundary as usize]
                        .iter()
                        .map(|(_, _, i)| i)
                        .sum();
                    assert_eq!(
                        below, 0,
                        "no ironwood commitments may exist below the activation boundary"
                    );
                    let above: u32 = source_counts[boundary as usize..]
                        .iter()
                        .map(|(_, _, i)| i)
                        .sum();
                    assert!(
                        above > 0,
                        "no ironwood commitments above the boundary; the ironwood \
                         assertions below would be vacuous"
                    );
                    if orchard_below_boundary {
                        let orchard_below: u32 = source_counts[2..boundary as usize]
                            .iter()
                            .map(|(_, o, _)| o)
                            .sum();
                        assert!(
                            orchard_below > 0,
                            "transition layout carries no orchard commitments below the \
                             boundary; the orchard-era half would be vacuous"
                        );
                    }
                }
            }

            // Compact blocks are only served once the finalised state has caught up.
            let snapshot = poll_until(
                "indexer to finish syncing so compact blocks are served",
                Duration::from_secs(60),
                Duration::from_millis(50),
                || async {
                    let snapshot = index_reader.snapshot_nonfinalized_state();
                    // The chain head is always populated; what this waits for
                    // is the finalised state catching up beneath it.
                    (snapshot.retained_block_count() > 0).then_some(snapshot)
                },
            )
            .await;
            let snapshot = &snapshot;

            let tip = crate::Height(u32::from(snapshot.best_tip().height));
            // The walk covers the non-finalised window; its absolute baseline is the
            // cumulative source count below the window.
            let first_walked = finalized_height_floor(tip.0).0 + 1;
            let baseline = source_counts[..first_walked as usize].iter().fold(
                (0u32, 0u32, 0u32),
                |(sapling, orchard, ironwood), (s, o, i)| (sapling + s, orchard + o, ironwood + i),
            );

            for unfiltered_wire_request in [true, false] {
                let (mut prev_sapling, mut prev_orchard, mut prev_ironwood) = baseline;

                for height_int in first_walked..=tip.0 {
                    // The empty slice is the wire shape unfiltered clients send; both
                    // filters include every shielded pool, which the delta assertions
                    // below rely on.
                    let filter = if unfiltered_wire_request {
                        PoolTypeFilter::new_from_slice(&[]).unwrap()
                    } else {
                        PoolTypeFilter::includes_all()
                    };
                    let block = index_reader
                        .get_compact_block(snapshot, Height(height_int), filter)
                        .await
                        .unwrap()
                        .expect("serviceable heights must serve a compact block");
                    let metadata = block
                        .chain_metadata
                        .as_ref()
                        .expect("served compact blocks carry chain metadata");

                    let served_sapling: u32 =
                        block.vtx.iter().map(|tx| tx.outputs.len() as u32).sum();
                    let served_orchard: u32 =
                        block.vtx.iter().map(|tx| tx.actions.len() as u32).sum();
                    let served_ironwood: u32 = block
                        .vtx
                        .iter()
                        .map(|tx| tx.ironwood_actions.len() as u32)
                        .sum();

                    // Serving completeness: everything the source block carries is served.
                    let (source_sapling, source_orchard, source_ironwood) =
                        source_counts[height_int as usize];
                    assert_eq!(
                        (served_sapling, served_orchard, served_ironwood),
                        (source_sapling, source_orchard, source_ironwood),
                        "served shielded counts must match the source block at height \
                         {height_int} (unfiltered_wire_request: {unfiltered_wire_request})"
                    );

                    // Metadata consistency: tree-size deltas equal served counts.
                    assert_eq!(
                        metadata.sapling_commitment_tree_size,
                        prev_sapling + served_sapling,
                        "sapling tree-size delta must equal the served output count at \
                         height {height_int}"
                    );
                    assert_eq!(
                        metadata.orchard_commitment_tree_size,
                        prev_orchard + served_orchard,
                        "orchard tree-size delta must equal the served action count at \
                         height {height_int}"
                    );
                    assert_eq!(
                        metadata.ironwood_commitment_tree_size,
                        prev_ironwood + served_ironwood,
                        "ironwood tree-size delta must equal the served action count at \
                         height {height_int}"
                    );

                    prev_sapling = metadata.sapling_commitment_tree_size;
                    prev_orchard = metadata.orchard_commitment_tree_size;
                    prev_ironwood = metadata.ironwood_commitment_tree_size;
                }
            }
        },
    )
}

// Ignored: this drives the full indexer over `partial_chain_strategy` blocks, whose headers carry
// arbitrary (invalid) merkle roots. The finalised state now validates blocks on the write path
// (cheap merkle + parent-continuity checks), so it correctly rejects these blocks once the indexer's
// finalised-sync reaches them. These proptest chains are not a valid input for the finalised state;
// MockSource-backed tests (chain_index::tests::finalised_state::v1 + migrations) cover the
// finalised state with valid blocks. Re-enable once the optional-db PR lands, which lets these
// passthrough proptests run without engaging the finalised state.
#[ignore = "proptest blocks have invalid merkle roots; finalised state rejects them. \
            Re-enable when the optional db PR lands. Covered by MockSource finalised_state tests."]
#[test]
fn make_chain() {
    init_tracing();
    let network = ActivationHeights::default().to_regtest_network();
    let segment_length = 12;

    let branch_count = 2;

    // default is 256. As each case takes multiple seconds, this seems too many.
    // TODO: this should be higher than 1. Currently set to 1 for ease of iteration
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network.clone()))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (genesis_segment, branching_segments) = segments;
            let mockchain = wrap_proptest_mockchain(ProptestMockchain {
                genesis_segment,
                branching_segments,
                delay: None,
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
                roots_cache: Arc::new(Mutex::new(HashMap::new())),
            }, network.clone());
            let temp_dir: tempfile::TempDir = tempfile::tempdir().unwrap();
            let db_path: std::path::PathBuf = temp_dir.path().to_path_buf();

            let config = ChainIndexConfig {
                storage: StorageConfig {
                    database: DatabaseConfig {
                        path: db_path,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ephemeral: true,
                mempool: Default::default(),
                db_version: 1,
                network: network.clone(),

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            let index_reader = indexer.subscriber();
            let expected_block_count = segment_length * (branch_count + 1);
            let snapshot = poll_until(
                "indexer to ingest the full proptest chain",
                Duration::from_secs(10),
                Duration::from_millis(25),
                || async {
                    let snapshot = index_reader.snapshot_nonfinalized_state();
                    (snapshot.retained_block_count() == expected_block_count)
                        .then_some(snapshot)
                },
            )
            .await;
            let best_tip = snapshot.best_tip();
            let best_tip_block = snapshot
                .block_by_hash(&best_tip.hash)
                .expect("the tip is retained");

            // A canonical block is its own fork point; a competing one resolves
            // to an ancestor. Both are answerable, which is what says the
            // branch is connected to the canonical chain rather than dangling.
            for block in snapshot.best_chain() {
                assert!(block.work <= best_tip_block.work);
                let hash = crate::BlockHash(block.hash().into());
                assert_eq!(
                    index_reader
                        .find_fork_point(&snapshot, &hash)
                        .await
                        .unwrap()
                        .unwrap()
                        .0,
                    hash,
                );
            }

            assert_eq!(snapshot.best_chain().count(), segment_length * 2);
        });
    });
}

#[derive(Clone)]
struct ProptestMockchain {
    genesis_segment: ChainSegment,
    branching_segments: Vec<ChainSegment>,
    delay: Option<Duration>,
    /// Cached result of `best_branch()`. The best branch is pure function of
    /// the other fields (which are never mutated after construction), so it's
    /// safe to memoize. Shared via `Arc` so `mockchain.clone()` — which
    /// happens per-future in the test bodies via `index_reader.clone()` —
    /// reuses the same cache rather than recomputing per clone.
    best_branch_cache: Arc<std::sync::OnceLock<SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>>>,
    /// Cached commitment tree frontiers per block hash.
    ///
    /// Rebuilding from genesis for every block asked about is quadratic over a
    /// chain, and the chain head asks once per block as it extends — which is
    /// exactly the access pattern that makes it matter. Caching the frontier
    /// rather than the finished roots is what lets an ascending walk resume
    /// instead of restart.
    #[allow(clippy::type_complexity)]
    roots_cache: Arc<Mutex<HashMap<[u8; 32], CachedFrontiers>>>,
    /// Cached txid → (tx, location) index. Built lazily on first `get_transaction`
    /// call. Replaces the O(N_blocks × M_txs) linear scan that recomputed
    /// `transaction.hash()` on every iteration — the dominant cost in the
    /// tx-iterating passthrough tests.
    #[allow(clippy::type_complexity)]
    tx_index: Arc<
        std::sync::OnceLock<
            std::collections::HashMap<
                zebra_chain::transaction::Hash,
                (
                    Arc<zebra_chain::transaction::Transaction>,
                    GetTransactionLocation,
                ),
            >,
        >,
    >,
}

impl ProptestMockchain {
    fn best_branch(&self) -> &SummaryDebug<Vec<Arc<zebra_chain::block::Block>>> {
        self.best_branch_cache.get_or_init(|| {
            let mut best_branch_and_work = None;
            for branch in self.branching_segments.clone() {
                let branch_chainwork: u128 = branch
                    .iter()
                    .map(|block| {
                        block
                            .header
                            .difficulty_threshold
                            .to_work()
                            .unwrap()
                            .as_u128()
                    })
                    .sum();
                match best_branch_and_work {
                    Some((ref _b, w)) => {
                        if w < branch_chainwork {
                            best_branch_and_work = Some((branch, branch_chainwork))
                        }
                    }
                    None => best_branch_and_work = Some((branch, branch_chainwork)),
                }
            }
            let mut combined = self.genesis_segment.clone();
            combined.append(&mut best_branch_and_work.unwrap().0.clone());
            combined
        })
    }

    /// Builds (lazily) and returns the tx-by-hash index.
    fn tx_index(
        &self,
    ) -> &std::collections::HashMap<
        zebra_chain::transaction::Hash,
        (
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        ),
    > {
        self.tx_index.get_or_init(|| {
            let best = self.best_branch().clone();
            let mut map = std::collections::HashMap::new();
            for block in self.all_blocks_arb_branch_order() {
                let location = if best.contains(block) {
                    GetTransactionLocation::BestChain(block.coinbase_height().unwrap())
                } else {
                    GetTransactionLocation::NonbestChain
                };
                for tx in block.transactions.iter() {
                    map.insert(tx.hash(), (tx.clone(), location.clone()));
                }
            }
            map
        })
    }

    fn all_blocks_arb_branch_order(&self) -> impl Iterator<Item = &Arc<zebra_chain::block::Block>> {
        self.genesis_segment.iter().chain(
            self.branching_segments
                .iter()
                .flat_map(|branch| branch.iter()),
        )
    }

    fn get_block_and_all_preceeding(
        &self,
        // This probably doesn't need to allow FnMut closures (Fn should suffice)
        // but there's no cost to allowing it
        mut block_identifier: impl FnMut(&zebra_chain::block::Block) -> bool,
    ) -> std::option::Option<Vec<&Arc<zebra_chain::block::Block>>> {
        let mut blocks = Vec::new();
        for block in self.genesis_segment.iter() {
            blocks.push(block);
            if block_identifier(block) {
                return Some(blocks);
            }
        }
        for branch in self.branching_segments.iter() {
            let mut branch_blocks = Vec::new();
            for block in branch.iter() {
                branch_blocks.push(block);
                if block_identifier(block) {
                    blocks.extend_from_slice(&branch_blocks);
                    return Some(blocks);
                }
            }
        }

        None
    }
}

use crate::chain_index::source::mockchain_source::port_fault;
use crate::chain_index::validator_source::ValidatorSource;
use zaino_source::QueryError as PortError;

/// Present the generated chain through ChainIndex's driven port, as a validator
/// is presented — the same `ValidatorSource` conversion runs here as in
/// production.
fn wrap_proptest_mockchain(
    source: ProptestMockchain,
    network: zebra_chain::parameters::Network,
) -> ValidatorSource<ProptestMockchain> {
    // No zebra state service behind a generated chain, so no `ChainTipChange`
    // stream — the same as an RPC-only deployment.
    ValidatorSource::new(source, network, None)
}

// ***** zaino-source port implementations *****
//
// This mock exercises sync and reorg handling, so it answers only the
// questions that drives: which block sits at a height (deliberately from an
// arbitrary branch, to simulate a reorg), which block a hash names, where the
// best chain tips, and the commitment tree state implied by a prefix. The rest
// stay unimplemented, as they were on `BlockchainSource`.

impl ProptestMockchain {
    /// The configured per-call delay, applied wherever the scaffolding applied
    /// it — the reorg tests use it to widen the window a racing reader sees.
    async fn settle(&self) {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
    }

    /// Parses serialized bytes into the domain block shape.
    ///
    /// The harness stores zebra blocks, so the parsed ports go through the
    /// same bytes the raw ports serve — which keeps the two answers about one
    /// block consistent by construction.
    fn parse_domain_block(bytes: &[u8]) -> Result<zaino_primitives::types::Block, String> {
        use zebra_chain::serialization::ZcashDeserialize as _;

        let block = zebra_chain::block::Block::zcash_deserialize(bytes)
            .map_err(|error| format!("proptest block did not deserialize: {error}"))?;
        // The proptest chains carry no commitment trees, so every pool is empty.
        zaino_convert_zebra::block_from_zebra(
            &block,
            zaino_primitives::types::ChainMetadata {
                sapling_tree_size: 0,
                orchard_tree_size: 0,
                ironwood_tree_size: 0,
            },
        )
        .map_err(|error| format!("proptest block did not convert: {error}"))
    }

    fn serialize(block: &zebra_chain::block::Block) -> Result<Vec<u8>, String> {
        block
            .zcash_serialize_to_vec()
            .map_err(|error| format!("proptest block did not serialize: {error}"))
    }
}

impl zaino_source::GetRawBlock for ProptestMockchain {
    async fn get_raw_block(
        &self,
        height: zaino_primitives::types::Height,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetBlockError>> {
        self.settle().await;
        let wanted = zebra_chain::block::Height(u32::from(height));

        // Deliberately an arbitrary branch rather than the best one: a reader
        // walking by height must cope with the answer changing under it, which
        // is the reorg these tests are about.
        let block = self
            .genesis_segment
            .iter()
            .find(|block| block.coinbase_height() == Some(wanted))
            .cloned()
            .or_else(|| {
                self.branching_segments
                    .choose(&mut rand::rng())?
                    .iter()
                    .find(|block| block.coinbase_height() == Some(wanted))
                    .cloned()
            })
            .ok_or(PortError::Domain(
                zaino_source::GetBlockError::HeightNotFound(height),
            ))?;

        Self::serialize(&block).map_err(port_fault)
    }
}

impl zaino_source::GetRawBlockByHash for ProptestMockchain {
    async fn get_raw_block_by_hash(
        &self,
        hash: zaino_primitives::types::BlockHash,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetBlockByHashError>> {
        self.settle().await;
        let wanted = zebra_chain::block::Hash(<[u8; 32]>::from(hash));

        // By hash every branch is in scope, side chains included — that is the
        // difference between the two questions.
        let block = self
            .all_blocks_arb_branch_order()
            .find(|block| block.hash() == wanted)
            .cloned()
            .ok_or(PortError::Domain(
                zaino_source::GetBlockByHashError::NotFound(hash),
            ))?;

        Self::serialize(&block).map_err(port_fault)
    }
}

/// The parsed block a height names, with the tree sizes this harness reports.
///
/// Shares `get_raw_block`'s deliberate arbitrary-branch choice, so a reader
/// walking by height sees the same instability whichever form it asks for.
impl zaino_source::OneShotGetBlock for ProptestMockchain {
    async fn get_block(
        &self,
        height: zaino_primitives::types::Height,
    ) -> Result<zaino_primitives::types::Block, PortError<zaino_source::GetBlockError>> {
        let bytes = zaino_source::GetRawBlock::get_raw_block(self, height).await?;
        Self::parse_domain_block(&bytes).map_err(port_fault)
    }
}

impl zaino_source::OneShotGetBlockByHash for ProptestMockchain {
    async fn get_block_by_hash(
        &self,
        hash: zaino_primitives::types::BlockHash,
    ) -> Result<zaino_primitives::types::Block, PortError<zaino_source::GetBlockByHashError>> {
        let bytes = zaino_source::GetRawBlockByHash::get_raw_block_by_hash(self, hash).await?;
        Self::parse_domain_block(&bytes).map_err(port_fault)
    }
}

impl zaino_source::OneShotGetChainTip for ProptestMockchain {
    async fn get_chain_tip(
        &self,
    ) -> Result<
        (
            zaino_primitives::types::BlockHash,
            zaino_primitives::types::Height,
        ),
        PortError<zaino_source::GetChainTipError>,
    > {
        self.settle().await;
        let tip = self
            .best_branch()
            .last()
            .ok_or(PortError::Domain(zaino_source::GetChainTipError::NotReady))?;
        let height = tip.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetChainTipError>("proptest tip has no coinbase height")
        })?;
        Ok((
            zaino_primitives::types::BlockHash::from(tip.hash().0),
            zaino_primitives::types::Height::try_from(height.0)
                .map_err(|e| port_fault::<zaino_source::GetChainTipError>(e.to_string()))?,
        ))
    }
}

impl zaino_source::OneShotGetBestBlockHeight for ProptestMockchain {
    async fn get_best_block_height(
        &self,
    ) -> Result<zaino_primitives::types::Height, PortError<zaino_source::GetBestBlockHeightError>>
    {
        self.settle().await;
        let tip = self.best_branch().last().ok_or(PortError::Domain(
            zaino_source::GetBestBlockHeightError::NotReady,
        ))?;
        let height = tip.coinbase_height().ok_or_else(|| {
            port_fault::<zaino_source::GetBestBlockHeightError>(
                "proptest tip has no coinbase height",
            )
        })?;
        zaino_primitives::types::Height::try_from(height.0).map_err(|e| port_fault(e.to_string()))
    }
}

impl zaino_source::GetTransaction for ProptestMockchain {
    async fn get_transaction(
        &self,
        txid: zaino_primitives::types::TransactionId,
    ) -> Result<zaino_source::TransactionResponse, PortError<zaino_source::GetTransactionError>>
    {
        self.settle().await;
        let zebra_txid = zebra_chain::transaction::Hash(<[u8; 32]>::from(txid));
        let Some((transaction, location)) = self.tx_index().get(&zebra_txid) else {
            return Err(PortError::Domain(
                zaino_source::GetTransactionError::NotFound(txid),
            ));
        };

        let location = match location {
            GetTransactionLocation::BestChain(height) => {
                zaino_primitives::types::TransactionLocation::BestChain(
                    zaino_primitives::types::Height::try_from(height.0)
                        .map_err(|e| port_fault(e.to_string()))?,
                )
            }
            GetTransactionLocation::NonbestChain => {
                zaino_primitives::types::TransactionLocation::NonBestChain
            }
            GetTransactionLocation::Mempool => {
                zaino_primitives::types::TransactionLocation::Mempool
            }
        };

        Ok(zaino_source::TransactionResponse {
            bytes: transaction
                .zcash_serialize_to_vec()
                .map_err(|error| port_fault(format!("proptest tx did not serialize: {error}")))?,
            location,
        })
    }
}

impl zaino_source::OneShotGetMempoolTxids for ProptestMockchain {
    async fn get_mempool_txids(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::TransactionId>,
        PortError<zaino_source::GetMempoolTxidsError>,
    > {
        self.settle().await;
        // Generated chains carry no mempool.
        Ok(Vec::new())
    }
}

/// Generated chains carry no mempool, so all three answer empty or absent. The
/// impls exist because `ChainIndexSourcePorts` requires them, not because the
/// proptest suite exercises mempool behaviour — `mockchain_tests` does that.
impl zaino_source::OneShotGetMempoolMetadata for ProptestMockchain {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<zaino_source::MempoolTxMeta>, PortError<zaino_source::GetMempoolMetadataError>>
    {
        self.settle().await;
        Ok(Vec::new())
    }
}

impl zaino_source::GetRawMempoolTransaction for ProptestMockchain {
    async fn get_raw_mempool_transaction(
        &self,
        txid: zaino_primitives::types::TransactionId,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetRawMempoolTransactionError>> {
        self.settle().await;
        Err(PortError::Domain(
            zaino_source::GetRawMempoolTransactionError::NotFound(txid),
        ))
    }
}

impl zaino_source::OneShotGetMempoolSourceTip for ProptestMockchain {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<
        (
            zaino_primitives::types::BlockHash,
            zaino_primitives::types::Height,
        ),
        PortError<std::convert::Infallible>,
    > {
        use zaino_source::OneShotGetChainTip as _;

        // No domain answer on this port by design — see `GetMempoolSourceTip`.
        self.get_chain_tip().await.map_err(|e| match e {
            PortError::Domain(zaino_source::GetChainTipError::NotReady) => {
                super::super::source::mockchain_source::port_fault(
                    "proptest mockchain has no chain tip to serve the mempool",
                )
            }
            PortError::Fetch(fetch) => PortError::Fetch(fetch),
        })
    }
}

impl zaino_source::OneShotGetCommitmentTreeRoots for ProptestMockchain {
    async fn get_commitment_tree_roots(
        &self,
        block: zaino_primitives::types::BlockHash,
    ) -> Result<
        zaino_primitives::types::TreeRoots,
        PortError<zaino_source::GetCommitmentTreeRootsError>,
    > {
        self.settle().await;
        let wanted = <[u8; 32]>::from(block);

        let Some(chain_up_to_block) =
            self.get_block_and_all_preceeding(|block| block.hash().0 == wanted)
        else {
            return Ok(zaino_primitives::types::TreeRoots {
                sapling: None,
                orchard: None,
                ironwood: None,
            });
        };

        // The trees are accumulated over the prefix rather than stored. Rebuilding
        // from genesis for each block asked about is quadratic over a chain, and
        // the chain head asks once per block as it extends — so the frontier at
        // each block is cached and the walk resumes from the deepest ancestor
        // already known, leaving each call to append only what is new.
        let mut resume_from = 0usize;
        let mut carried = (None, None, None);
        {
            let cache = self.roots_cache.lock().expect("roots cache mutex poisoned");
            for (index, block) in chain_up_to_block.iter().enumerate().rev() {
                if let Some(frontiers) = cache.get(&block.hash().0) {
                    carried = frontiers.clone();
                    resume_from = index + 1;
                    break;
                }
            }
        }

        let (sapling, orchard, ironwood) = chain_up_to_block[resume_from..].iter().fold(
            carried,
            |(mut sapling, mut orchard, mut ironwood), block| {
                for transaction in &block.transactions {
                    for sap_commitment in transaction.sapling_note_commitments() {
                        let Some(sap_commitment) = Option::<sapling_crypto::Node>::from(
                            sapling_crypto::Node::from_bytes(sap_commitment.to_bytes()),
                        ) else {
                            continue;
                        };
                        let mut tree = sapling.unwrap_or_else(|| {
                            incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                        });
                        tree.append(sap_commitment);
                        sapling = Some(tree);
                    }
                    for orc_commitment in transaction.orchard_note_commitments() {
                        let orc_commitment =
                            zebra_chain::orchard::tree::Node::from(*orc_commitment);
                        let mut tree = orchard.unwrap_or_else(|| {
                            incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                        });
                        tree.append(orc_commitment);
                        orchard = Some(tree);
                    }
                    // Ironwood reuses the Orchard tree/node types.
                    for irw_commitment in transaction.ironwood_note_commitments() {
                        let irw_commitment =
                            zebra_chain::orchard::tree::Node::from(*irw_commitment);
                        let mut tree = ironwood.unwrap_or_else(|| {
                            incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                        });
                        tree.append(irw_commitment);
                        ironwood = Some(tree);
                    }
                }
                self.roots_cache
                    .lock()
                    .expect("roots cache mutex poisoned")
                    .insert(
                        block.hash().0,
                        (sapling.clone(), orchard.clone(), ironwood.clone()),
                    );
                (sapling, orchard, ironwood)
            },
        );

        let info = |root: [u8; 32], size: u64| zaino_primitives::types::TreeRootInfo {
            root: zaino_primitives::types::TreeRoot::from(root),
            size,
        };

        // An empty pool reports the empty-tree root, not an absent one. A
        // validator answers that way for any activated pool, and the finalised
        // state's passthrough requires it — previously unnoticed here because
        // these queries were short-circuited before the finalised state saw
        // them.
        let sapling_front =
            sapling.unwrap_or_else(incrementalmerkletree::frontier::Frontier::<_, 32>::empty);
        let orchard_front =
            orchard.unwrap_or_else(incrementalmerkletree::frontier::Frontier::<_, 32>::empty);

        let roots = zaino_primitives::types::TreeRoots {
            sapling: Some(info(
                sapling_front.root().to_bytes(),
                sapling_front.tree_size(),
            )),
            orchard: Some(info(
                orchard_front.root().to_repr(),
                orchard_front.tree_size(),
            )),
            // Ironwood stays absent when the chain has none: it activates at
            // NU6.3, and reporting a root before then would be inventing one.
            ironwood: ironwood.map(|front| info(front.root().to_repr(), front.tree_size())),
        };

        Ok(roots)
    }
}

type ChainSegment = SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>;

/// Sapling, Orchard and Ironwood frontiers as of one block.
type CachedFrontiers = (
    Option<incrementalmerkletree::frontier::Frontier<sapling_crypto::Node, 32>>,
    Option<incrementalmerkletree::frontier::Frontier<zebra_chain::orchard::tree::Node, 32>>,
    Option<incrementalmerkletree::frontier::Frontier<zebra_chain::orchard::tree::Node, 32>>,
);

fn make_branching_chain(
    // The number of separate branches, after the branching point at the tip
    // of the initial segment.
    num_branches: usize,
    // The length of the initial segment, and of the branches
    // TODO: it would be useful to allow branches of different lengths.
    chain_size: usize,
    network_override: zebra_chain::parameters::Network,
) -> BoxedStrategy<(ChainSegment, Vec<ChainSegment>)> {
    let network_override = Some(network_override);
    add_segment(
        SummaryDebug(Vec::new()),
        network_override.clone(),
        chain_size,
    )
    .prop_flat_map(move |segment| {
        (
            Just(segment.clone()),
            LedgerState::arbitrary_with(LedgerStateOverride {
                height_override: segment.last().unwrap().coinbase_height().unwrap() + 1,
                previous_block_hash_override: Some(segment.last().unwrap().hash()),
                network_upgrade_override: None,
                transaction_version_override: None,
                transaction_has_valid_network_upgrade: true,
                always_has_coinbase: true,
                network_override: network_override.clone(),
            }),
        )
    })
    .prop_flat_map(move |(segment, ledger)| {
        (
            Just(segment),
            std::iter::repeat_with(|| {
                zebra_chain::block::Block::partial_chain_strategy(
                    ledger.clone(),
                    chain_size,
                    arbitrary::allow_all_transparent_coinbase_spends,
                    true,
                )
            })
            .take(num_branches)
            .collect::<Vec<_>>(),
        )
    })
    .boxed()
}

mod proptest_helpers {

    use proptest::prelude::{Arbitrary, BoxedStrategy, Strategy};
    use zebra_chain::{
        block::{
            arbitrary::{allow_all_transparent_coinbase_spends, LedgerStateOverride},
            Block, Height,
        },
        parameters::{Network, GENESIS_PREVIOUS_BLOCK_HASH},
        LedgerState,
    };

    use super::ChainSegment;

    pub(super) fn add_segment(
        previous_chain: ChainSegment,
        network_override: Option<Network>,
        segment_length: usize,
    ) -> BoxedStrategy<ChainSegment> {
        LedgerState::arbitrary_with(LedgerStateOverride {
            height_override: Some(
                previous_chain
                    .last()
                    .map(|block| (block.coinbase_height().unwrap() + 1).unwrap())
                    .unwrap_or(Height(0)),
            ),
            previous_block_hash_override: Some(
                previous_chain
                    .last()
                    .map(|block| block.hash())
                    .unwrap_or(GENESIS_PREVIOUS_BLOCK_HASH),
            ),
            network_upgrade_override: None,
            transaction_version_override: None,
            transaction_has_valid_network_upgrade: true,
            always_has_coinbase: true,
            network_override,
        })
        .prop_flat_map(move |ledger| {
            Block::partial_chain_strategy(
                ledger,
                segment_length,
                allow_all_transparent_coinbase_spends,
                true,
            )
        })
        .prop_map(move |new_segment| {
            let mut full_chain = previous_chain.clone();
            full_chain.extend_from_slice(&new_segment);
            full_chain
        })
        .boxed()
    }
}

// ***** Questions a generated chain does not answer *****
//
// This fixture exercises sync and reorg handling. Everything below carried
// `unimplemented!()` or `todo!()` on `BlockchainSource` and keeps doing so —
// reaching one means a test started depending on something the generator does
// not model, which is worth a panic rather than a plausible-looking zero.

impl zaino_source::GetBlockVerboseByHash for ProptestMockchain {
    async fn get_block_verbose_by_hash(
        &self,
        _hash: zaino_primitives::types::BlockHash,
    ) -> Result<zaino_primitives::types::BlockVerbose, PortError<zaino_source::GetBlockVerboseError>>
    {
        unimplemented!("ProptestMockchain exercises sync/reorg, not the verbose getblock RPC")
    }
}

impl zaino_source::OneShotGetBlockHeader for ProptestMockchain {
    async fn get_block_header(
        &self,
        _hash: zaino_primitives::types::BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockHeaderVerbose,
        PortError<zaino_source::GetBlockHeaderError>,
    > {
        unimplemented!("ProptestMockchain exercises sync/reorg, not the getblockheader RPC")
    }
}

impl zaino_source::GetRawBlockHeader for ProptestMockchain {
    async fn get_raw_block_header(
        &self,
        _hash: zaino_primitives::types::BlockHash,
    ) -> Result<Vec<u8>, PortError<zaino_source::GetBlockHeaderError>> {
        unimplemented!("ProptestMockchain exercises sync/reorg, not the getblockheader RPC")
    }
}

impl zaino_source::OneShotGetBlockDeltas for ProptestMockchain {
    async fn get_block_deltas(
        &self,
        _hash: zaino_primitives::types::BlockHash,
    ) -> Result<
        zaino_primitives::types::rpc::BlockDeltas,
        PortError<zaino_source::GetBlockDeltasError>,
    > {
        unimplemented!("ProptestMockchain exercises sync/reorg, not the getblockdeltas RPC")
    }
}

impl zaino_source::OneShotGetDifficulty for ProptestMockchain {
    async fn get_difficulty(
        &self,
    ) -> Result<zaino_primitives::types::Difficulty, PortError<zaino_source::GetDifficultyError>>
    {
        unimplemented!("ProptestMockchain exercises sync/reorg, not the getdifficulty RPC")
    }
}

impl zaino_source::OneShotGetBlockchainInfo for ProptestMockchain {
    async fn get_blockchain_info(
        &self,
    ) -> Result<
        zaino_primitives::types::BlockchainInfo,
        PortError<zaino_source::GetBlockchainInfoError>,
    > {
        unimplemented!("ProptestMockchain exercises sync/reorg, not the getblockchaininfo RPC")
    }
}

impl zaino_source::OneShotGetNodeInfo for ProptestMockchain {
    async fn get_node_info(
        &self,
    ) -> Result<zaino_primitives::types::rpc::NodeInfo, PortError<zaino_source::GetNodeInfoError>>
    {
        unimplemented!()
    }
}

impl zaino_source::OneShotGetPeerInfo for ProptestMockchain {
    async fn get_peer_info(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::rpc::PeerInfo>,
        PortError<zaino_source::GetPeerInfoError>,
    > {
        unimplemented!()
    }
}

/// The tip of every branch this harness generated.
///
/// Previously `unimplemented!()`: nothing asked, because the old non-finalised
/// state never learned about competing branches at all. The chain head does
/// ask, and answering with the real branches is what puts its competing-branch
/// retention under these property tests rather than leaving it untested here.
impl zaino_source::OneShotGetChainTips for ProptestMockchain {
    async fn get_chain_tips(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::rpc::ChainTip>,
        PortError<zaino_source::GetChainTipsError>,
    > {
        self.settle().await;

        let best_tip_hash = self.best_branch().last().map(|block| block.hash());

        let tip = |block: &Arc<zebra_chain::block::Block>| {
            let height = block.coinbase_height()?;
            let is_active = Some(block.hash()) == best_tip_hash;
            Some(zaino_primitives::types::rpc::ChainTip {
                height: zaino_primitives::types::Height::try_from(height.0).ok()?,
                hash: zaino_primitives::types::BlockHash::from(block.hash().0),
                // Zero for the active tip; every generated branch forks off the
                // genesis segment's end, so the rest are one segment away.
                branch_len: if is_active {
                    0
                } else {
                    u32::try_from(self.genesis_segment.len()).unwrap_or(u32::MAX)
                },
                status: if is_active {
                    zaino_primitives::types::rpc::ChainTipStatus::Active
                } else {
                    zaino_primitives::types::rpc::ChainTipStatus::ValidFork
                },
            })
        };

        let mut tips: Vec<_> = self
            .branching_segments
            .iter()
            .filter_map(|branch| branch.last())
            .filter_map(tip)
            .collect();
        if tips.is_empty() {
            tips.extend(self.genesis_segment.last().and_then(tip));
        }
        Ok(tips)
    }
}

impl zaino_source::OneShotGetBlockSubsidy for ProptestMockchain {
    async fn get_block_subsidy(
        &self,
        _height: zaino_primitives::types::Height,
    ) -> Result<
        zaino_primitives::types::rpc::BlockSubsidy,
        PortError<zaino_source::GetBlockSubsidyError>,
    > {
        unimplemented!()
    }
}

impl zaino_source::OneShotGetMiningInfo for ProptestMockchain {
    async fn get_mining_info(
        &self,
    ) -> Result<zaino_primitives::types::rpc::MiningInfo, PortError<zaino_source::GetMiningInfoError>>
    {
        unimplemented!()
    }
}

impl zaino_source::GetTxOut for ProptestMockchain {
    async fn get_tx_out(
        &self,
        _txid: zaino_primitives::types::TransactionId,
        _index: zaino_primitives::types::OutputIndex,
        _include_mempool: bool,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, PortError<zaino_source::GetTxOutError>>
    {
        unimplemented!()
    }
}

impl zaino_source::GetSpentInfo for ProptestMockchain {
    async fn get_spent_info(
        &self,
        _outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<zaino_primitives::types::rpc::SpentInfo, PortError<zaino_source::GetSpentInfoError>>
    {
        unimplemented!()
    }
}

impl zaino_source::OneShotGetNetworkSolPs for ProptestMockchain {
    async fn get_network_sol_ps(
        &self,
        _blocks: Option<u32>,
        _height: Option<zaino_primitives::types::Height>,
    ) -> Result<u64, PortError<zaino_source::GetNetworkSolPsError>> {
        unimplemented!()
    }
}

impl zaino_source::SendRawTransaction for ProptestMockchain {
    async fn send_raw_transaction(
        &self,
        _transaction: Vec<u8>,
    ) -> Result<
        zaino_primitives::types::TransactionId,
        PortError<zaino_source::SendRawTransactionError>,
    > {
        unimplemented!()
    }
}

impl zaino_source::GetTreestate for ProptestMockchain {
    async fn get_treestate(
        &self,
        _height: zaino_primitives::types::Height,
    ) -> Result<zaino_primitives::types::Treestate, PortError<zaino_source::GetTreestateError>>
    {
        unimplemented!()
    }
}

impl zaino_source::GetTreestateByHash for ProptestMockchain {
    async fn get_treestate_by_hash(
        &self,
        _hash: zaino_primitives::types::BlockHash,
    ) -> Result<zaino_primitives::types::Treestate, PortError<zaino_source::GetTreestateByHashError>>
    {
        unimplemented!()
    }
}

impl zaino_source::GetSubtreeRoots for ProptestMockchain {
    async fn get_subtree_roots(
        &self,
        _pool: zaino_primitives::types::ShieldedPool,
        _start_index: u16,
        _limit: Option<u16>,
    ) -> Result<
        Vec<zaino_primitives::types::SubtreeRoot>,
        PortError<zaino_source::GetSubtreeRootsError>,
    > {
        todo!()
    }
}

impl zaino_source::OneShotGetAddressDeltas for ProptestMockchain {
    async fn get_address_deltas(
        &self,
        _addresses: Vec<String>,
        _start: zaino_primitives::types::Height,
        _end: zaino_primitives::types::Height,
    ) -> Result<
        Vec<zaino_primitives::types::AddressDelta>,
        PortError<zaino_source::GetAddressDeltasError>,
    > {
        todo!()
    }
}

impl zaino_source::OneShotGetAddressBalance for ProptestMockchain {
    async fn get_address_balance(
        &self,
        _addresses: Vec<String>,
    ) -> Result<
        zaino_primitives::types::AddressBalance,
        PortError<zaino_source::GetAddressBalanceError>,
    > {
        todo!()
    }
}

impl zaino_source::OneShotGetAddressTxids for ProptestMockchain {
    async fn get_address_txids(
        &self,
        _addresses: Vec<String>,
        _start: zaino_primitives::types::Height,
        _end: zaino_primitives::types::Height,
    ) -> Result<
        Vec<zaino_primitives::types::TransactionId>,
        PortError<zaino_source::GetAddressTxidsError>,
    > {
        todo!()
    }
}

impl zaino_source::OneShotGetAddressUtxos for ProptestMockchain {
    async fn get_address_utxos(
        &self,
        _addresses: Vec<String>,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, PortError<zaino_source::GetAddressUtxosError>>
    {
        todo!()
    }
}

impl zaino_source::SourceLifecycle for ProptestMockchain {}

impl zaino_source::SubscribeBlocks for ProptestMockchain {}
