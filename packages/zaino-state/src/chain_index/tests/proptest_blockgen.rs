use std::{sync::Arc, time::Duration};

use futures::stream::FuturesUnordered;
use proptest::{
    prelude::{Arbitrary as _, BoxedStrategy, Just},
    strategy::Strategy,
};
use rand::seq::IndexedRandom;
use tokio_stream::StreamExt as _;
use zaino_common::{network::ActivationHeights, DatabaseConfig, Network, StorageConfig};
use zaino_fetch::jsonrpsee::response::address_deltas::{
    GetAddressDeltasParams, GetAddressDeltasResponse,
};
use zebra_chain::{
    block::arbitrary::{self, LedgerStateOverride},
    fmt::SummaryDebug,
    serialization::ZcashSerialize,
    transaction::SerializedTransaction,
    LedgerState,
};
use zebra_rpc::{
    client::{GetAddressBalanceRequest, GetAddressTxIdsRequest},
    methods::{AddressBalance, GetAddressUtxos},
};
use zebra_state::{FromDisk, HashOrHeight, IntoDisk as _};

use crate::{
    chain_index::{
        finalized_height_floor,
        non_finalised_state::ChainIndexSnapshot,
        source::{BlockchainSourceResult, GetTransactionLocation},
        tests::{init_tracing, poll::poll_until, proptest_blockgen::proptest_helpers::add_segment},
        types::BestChainLocation,
        NonFinalizedSnapshot, OPERATIONAL_NFS_DEPTH,
    },
    BlockHash, BlockchainSource, ChainIndex, ChainIndexConfig, Height, NodeBackedChainIndex,
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
        &ProptestMockchain,
        // The subscriber to test against
        NodeBackedChainIndexSubscriber<ProptestMockchain>,
        // A snapshot, which will have only the genesis block
        &ChainIndexSnapshot,
    ),
) {
    passthrough_test_on(
        Network::Regtest(ActivationHeights::default()),
        // Slow the source enough to hold the indexer in passthrough while the
        // assertions run, without slowing passthrough more than necessary.
        Some(Duration::from_millis(100)),
        |_| {},
        test,
    )
}

/// [`passthrough_test`] on an explicit network, with a per-segment chain mutator.
///
/// The mutator exists because zebra's stock `Transaction` strategy never generates V6
/// transactions (its NU6.3/NU7 arm produces only v4/v5), so ironwood-era content must
/// be injected after generation. Mutating a block's transactions is safe here: the
/// block hash covers only the header, so parent-hash continuity is untouched, and the
/// header's merkle root is already arbitrary — the passthrough path tolerates that by
/// construction.
fn passthrough_test_on(
    network: Network,
    source_delay: Option<Duration>,
    mutate_segment: impl Fn(&mut Vec<Arc<zebra_chain::block::Block>>),
    test: impl AsyncFn(
        &ProptestMockchain,
        NodeBackedChainIndexSubscriber<ProptestMockchain>,
        &ChainIndexSnapshot,
    ),
) {
    init_tracing();
    let segment_length = PASSTHROUGH_SEGMENT_LENGTH;
    // No need to worry about non-best chains for this test
    let branch_count = 1;

    // from this line to `runtime.block_on(async {` are all
    // copy-pasted. Could a macro get rid of some of this boilerplate?
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (mut genesis_segment, mut branching_segments) = segments;
            mutate_segment(&mut genesis_segment.0);
            for segment in &mut branching_segments {
                mutate_segment(&mut segment.0);
            }
            let mockchain = ProptestMockchain {
                genesis_segment,
                branching_segments,
                delay: source_delay,
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
            };
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
                db_version: 1,
                network,

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            let index_reader = indexer.subscriber();
            // The best chain is `2 * segment_length` blocks (genesis segment +
            // one branch), so its tip height is `2 * segment_length - 1`. The
            // serviceable cutoff is the finalized floor at that tip — mirror
            // production's `finalized_height_floor` exactly.
            let tip_height = (2 * segment_length - 1) as u32;
            let expected_max_serviceable_height = finalized_height_floor(tip_height).0 as usize;
            // Poll rather than sleeping a fixed 5 s: the indexer discovers the
            // chain topology as soon as the sync task has walked enough of the
            // source to identify the finalized-state cutoff. With a 1 s
            // per-block source delay (above) that's well under 5 s in practice,
            // but can be longer under parallel-suite scheduler pressure.
            poll_until(
                "indexer to reach expected max_serviceable_height",
                Duration::from_secs(30),
                Duration::from_millis(50),
                || async {
                    let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
                    (snapshot.max_serviceable_height().0 as usize
                        == expected_max_serviceable_height)
                        .then_some(())
                },
            )
            .await;
            let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
            assert_eq!(snapshot.max_serviceable_height().0 as usize, expected_max_serviceable_height);
            assert!(matches!(snapshot, ChainIndexSnapshot::StillSyncingFinalizedState { .. }));

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

                if height <= *snapshot.max_serviceable_height() {
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
        for (height, txid) in mockchain.all_blocks_arb_branch_order().flat_map(|block| {
            block
                .transactions
                .iter()
                .map(|transaction| (block.coinbase_height().unwrap(), transaction.hash()))
                .collect::<Vec<_>>()
        }) {
            let index_reader = index_reader.clone();
            let snapshot = snapshot.clone();
            parallel.push(async move {
                let transaction_status = index_reader
                    .get_transaction_status(&snapshot, &txid.into())
                    .await
                    .unwrap();

                if height <= *snapshot.max_serviceable_height() {
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
        for (expected_transaction, height) in
            mockchain.all_blocks_arb_branch_order().flat_map(|block| {
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

#[test]
fn passthrough_best_chaintip() {
    passthrough_test(async |mockchain, index_reader, snapshot| {
        let tip = index_reader.best_chaintip(snapshot).await.unwrap();
        assert_eq!(
            tip.height.0,
            mockchain
                .best_branch()
                .last()
                .unwrap()
                .coinbase_height()
                .map(|h| finalized_height_floor(h.0).0)
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
                if expected_height <= *snapshot.max_serviceable_height() {
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
            .all_blocks_arb_branch_order()
            .map(|block| block.coinbase_height().unwrap())
        {
            let expected_end_height = (expected_start_height + 9).unwrap();
            if expected_end_height.0 as usize <= mockchain.all_blocks_arb_branch_order().count() {
                let index_reader = index_reader.clone();
                let snapshot = snapshot.clone();
                parallel.push(async move {
                    let block_range_stream = index_reader.get_block_range(
                        &snapshot,
                        expected_start_height.into(),
                        Some(expected_end_height.into()),
                    );
                    if expected_start_height <= *snapshot.max_serviceable_height() {
                        let mut block_range_stream = Box::pin(block_range_stream.unwrap());
                        let mut num_blocks_in_stream = 0;
                        while let Some(block) = block_range_stream.next().await {
                            let expected_block = mockchain
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
                                snapshot
                                    .max_serviceable_height()
                                    .0
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

/// Upstream gap demonstration: zebra-chain's stock [`Transaction`] strategy never
/// generates V6 transactions, even for an NU6.3 ledger state — its NU6.3/NU7 arm is
/// `prop_oneof![v4_strategy, v5_strategy]` (zebra-chain `transaction/arbitrary.rs`).
/// V6 is therefore structurally impossible from the stock strategy, not merely rare,
/// which is why the `passthrough_metadata_consistency_*` walks must inject
/// `fake_v6_transaction` ironwood content instead of relying on generation.
///
/// `should_panic` tracks the upstream gap: when a zebra upgrade starts generating V6,
/// this test flips, and the `#[should_panic]` should be removed together with the
/// fake-transaction injection in `inject_ironwood_transactions` (generation then covers
/// it natively).
///
/// [`Transaction`]: zebra_chain::transaction::Transaction
#[test]
#[should_panic(expected = "zebra's stock Transaction strategy generated no V6")]
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
const NU6_3_ACTIVE_HEIGHTS: ActivationHeights = ActivationHeights {
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
    metadata_consistency_for_era(NU6_3_ACTIVE_HEIGHTS, Some(2), false)
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

/// Orchard-only era (NU6.3 never activates): fake Orchard content from height 2,
/// and — since zebra's stock strategy cannot generate V6 — ironwood provably never
/// appears anywhere in the chain or the served form.
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
            ..NU6_3_ACTIVE_HEIGHTS
        },
        Some(boundary),
        true,
    )
}

/// A structurally-valid (cryptographically fake) V6 transaction carrying a two-action
/// Ironwood bundle. Injected because zebra's stock strategy never generates V6
/// (demonstrated by [`zebra_arbitrary_generates_v6_transactions_for_nu6_3`]).
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
        Network::Regtest(heights),
        // No artificial source delay: this test waits for the indexer to finish
        // syncing, because compact blocks are not served while the finalised state
        // is still syncing (get_compact_block's StillSyncingFinalizedState arm).
        None,
        inject,
        async |mockchain, index_reader, _snapshot| {
            // Source of truth: per-height shielded commitment counts from the mockchain
            // blocks themselves (single branch, so arb branch order is chain order).
            let source_counts: Vec<(u32, u32, u32)> = mockchain
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
                    let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
                    matches!(snapshot, ChainIndexSnapshot::NonFinalizedStateExists { .. })
                        .then_some(snapshot)
                },
            )
            .await;
            let snapshot = &snapshot;

            let tip = snapshot
                .get_nfs_snapshot()
                .expect("fully synced snapshot has a non-finalised state")
                .best_tip
                .height;
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
// MockchainSource-backed tests (chain_index::tests::finalised_state::v1 + migrations) cover the
// finalised state with valid blocks. Re-enable once the optional-db PR lands, which lets these
// passthrough proptests run without engaging the finalised state.
#[ignore = "proptest blocks have invalid merkle roots; finalised state rejects them. \
            Re-enable when the optional db PR lands. Covered by MockchainSource finalised_state tests."]
#[test]
fn make_chain() {
    init_tracing();
    let network = Network::Regtest(ActivationHeights::default());
    let segment_length = 12;

    let branch_count = 2;

    // default is 256. As each case takes multiple seconds, this seems too many.
    // TODO: this should be higher than 1. Currently set to 1 for ease of iteration
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (genesis_segment, branching_segments) = segments;
            let mockchain = ProptestMockchain {
                genesis_segment,
                branching_segments,
                delay: None,
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
            };
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
                db_version: 1,
                network,

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
                    let snapshot = index_reader.snapshot_nonfinalized_state().await.ok()?;
                    (snapshot.get_nfs_snapshot()?.blocks.len() == expected_block_count)
                        .then_some(snapshot)
                },
            )
            .await;
            let non_finalized_snapshot = snapshot.get_nfs_snapshot().expect("not synced");
            let best_tip_hash = non_finalized_snapshot.best_tip.hash;
            let best_tip_block = non_finalized_snapshot
                .get_chainblock_by_hash(&best_tip_hash)
                .unwrap();
            for (hash, block) in &non_finalized_snapshot.blocks {
                if hash != &best_tip_hash {
                    assert!(block.chainwork() <= best_tip_block.chainwork());
                    if non_finalized_snapshot.heights_to_hashes.get(&block.height()) == Some(block.hash()) {
                        assert_eq!(index_reader.find_fork_point(&snapshot, hash).await.unwrap().unwrap().0, *hash);
                    } else {
                        assert_ne!(index_reader.find_fork_point(&snapshot, hash).await.unwrap().unwrap().0, *hash);
                    }
                }
            }
            assert_eq!(non_finalized_snapshot.heights_to_hashes.len(), (segment_length * 2) );
            assert_eq!(
                non_finalized_snapshot.blocks.len(),
                segment_length * (branch_count + 1)
            );
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

impl BlockchainSource for ProptestMockchain {
    /// Returns the block by hash or height
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        match id {
            HashOrHeight::Hash(hash) => {
                let matches_hash = |block: &&Arc<zebra_chain::block::Block>| block.hash() == hash;
                Ok(self
                    .genesis_segment
                    .iter()
                    .find(matches_hash)
                    .or_else(|| {
                        self.branching_segments
                            .iter()
                            .flat_map(|vec| vec.iter())
                            .find(matches_hash)
                    })
                    .cloned())
            }
            // This implementation selects a block from a random branch instead
            // of the best branch. This is intended to simulate reorgs
            HashOrHeight::Height(height) => Ok(self
                .genesis_segment
                .iter()
                .find(|block| block.coinbase_height().unwrap() == height)
                .cloned()
                .or_else(|| {
                    self.branching_segments
                        .choose(&mut rand::rng())
                        .unwrap()
                        .iter()
                        .find(|block| block.coinbase_height().unwrap() == height)
                        .cloned()
                })),
        }
    }

    /// Returns the block commitment tree data by hash
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let Some(chain_up_to_block) =
            self.get_block_and_all_preceeding(|block| block.hash().0 == id.0)
        else {
            return Ok((None, None, None));
        };

        let (sapling, orchard, ironwood) = chain_up_to_block.iter().fold(
            (None, None, None),
            |(mut sapling, mut orchard, mut ironwood), block| {
                for transaction in &block.transactions {
                    for sap_commitment in transaction.sapling_note_commitments() {
                        let sap_commitment =
                            sapling_crypto::Node::from_bytes(sap_commitment.to_bytes()).unwrap();

                        sapling = Some(sapling.unwrap_or_else(|| {
                            incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                        }));

                        sapling = sapling.map(|mut tree| {
                            tree.append(sap_commitment);
                            tree
                        });
                    }
                    for orc_commitment in transaction.orchard_note_commitments() {
                        let orc_commitment =
                            zebra_chain::orchard::tree::Node::from(*orc_commitment);

                        orchard = Some(orchard.unwrap_or_else(|| {
                            incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                        }));

                        orchard = orchard.map(|mut tree| {
                            tree.append(orc_commitment);
                            tree
                        });
                    }
                    // Ironwood reuses the Orchard tree/node types.
                    for irw_commitment in transaction.ironwood_note_commitments() {
                        let irw_commitment =
                            zebra_chain::orchard::tree::Node::from(*irw_commitment);

                        ironwood = Some(ironwood.unwrap_or_else(|| {
                            incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                        }));

                        ironwood = ironwood.map(|mut tree| {
                            tree.append(irw_commitment);
                            tree
                        });
                    }
                }
                (sapling, orchard, ironwood)
            },
        );
        Ok((
            sapling.map(|sap_front| {
                (
                    zebra_chain::sapling::tree::Root::from_bytes(sap_front.root().to_bytes()),
                    sap_front.tree_size(),
                )
            }),
            orchard.map(|orc_front| {
                (
                    zebra_chain::orchard::tree::Root::from_bytes(orc_front.root().as_bytes()),
                    orc_front.tree_size(),
                )
            }),
            ironwood.map(|irw_front| {
                (
                    zebra_chain::orchard::tree::Root::from_bytes(irw_front.root().as_bytes()),
                    irw_front.tree_size(),
                )
            }),
        ))
    }

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        _id: BlockHash,
    ) -> BlockchainSourceResult<crate::chain_index::source::TreestateBytes> {
        // I don't think this is used for sync?
        unimplemented!()
    }

    /// Returns the complete list of txids currently in the mempool.
    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        Ok(Some(Vec::new()))
    }

    /// Returns the transaction by txid
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<
        Option<(
            Arc<zebra_chain::transaction::Transaction>,
            GetTransactionLocation,
        )>,
    > {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        Ok(self.tx_index().get(&txid.into()).cloned())
    }

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        Ok(Some(self.best_branch().last().unwrap().hash()))
    }

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_height(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Height>> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        Ok(Some(
            self.best_branch()
                .last()
                .unwrap()
                .coinbase_height()
                .unwrap(),
        ))
    }

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let (sender, receiver) = tokio::sync::mpsc::channel(1_000);
        let self_clone = self.clone();
        tokio::task::spawn(async move {
            for block in self_clone.all_blocks_arb_branch_order() {
                sender.send((block.hash(), block.clone())).await.unwrap()
            }
            // don't drop the sender
            std::mem::forget(sender);
        })
        .await
        .unwrap();
        Ok(Some(receiver))
    }

    async fn get_subtree_roots(
        &self,
        _pool: crate::chain_index::ShieldedPool,
        _start_index: u16,
        _max_entries: Option<u16>,
    ) -> BlockchainSourceResult<Vec<([u8; 32], u32)>> {
        todo!()
    }

    // ********** Transparent address methods **********

    async fn get_address_deltas(
        &self,
        _params: GetAddressDeltasParams,
    ) -> BlockchainSourceResult<GetAddressDeltasResponse> {
        //
        todo!()
    }

    async fn get_address_balance(
        &self,
        _address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<AddressBalance> {
        //
        todo!()
    }

    async fn get_address_txids(
        &self,
        _request: GetAddressTxIdsRequest,
    ) -> BlockchainSourceResult<Vec<TransactionHash>> {
        //
        todo!()
    }

    async fn get_address_utxos(
        &self,
        _address_strings: GetAddressBalanceRequest,
    ) -> BlockchainSourceResult<Vec<GetAddressUtxos>> {
        //
        todo!()
    }
}

type ChainSegment = SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>;

fn make_branching_chain(
    // The number of separate branches, after the branching point at the tip
    // of the initial segment.
    num_branches: usize,
    // The length of the initial segment, and of the branches
    // TODO: it would be useful to allow branches of different lengths.
    chain_size: usize,
    network_override: Network,
) -> BoxedStrategy<(ChainSegment, Vec<ChainSegment>)> {
    let network_override = Some(network_override.to_zebra_network());
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
