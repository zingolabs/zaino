use std::{sync::Arc, time::Duration};

use futures::stream::FuturesUnordered;
use proptest::{
    prelude::{Arbitrary as _, BoxedStrategy, Just},
    strategy::Strategy,
};
use rand::seq::IndexedRandom;
use tokio_stream::StreamExt as _;
use tonic::async_trait;
use zaino_common::{network::ActivationHeights, DatabaseConfig, Network, StorageConfig};
use zebra_chain::{
    block::arbitrary::{self, LedgerStateOverride},
    fmt::SummaryDebug,
    serialization::ZcashSerialize,
    transaction::SerializedTransaction,
    LedgerState,
};
use zebra_state::{FromDisk, HashOrHeight, IntoDisk as _};

use crate::{
    chain_index::{
        non_finalised_state::ChainIndexSnapshot,
        source::{BlockchainSourceResult, GetTransactionLocation},
        tests::{init_tracing, poll::poll_until, proptest_blockgen::proptest_helpers::add_segment},
        types::BestChainLocation,
        NonFinalizedSnapshot,
    },
    BlockCacheConfig, BlockHash, BlockchainSource, ChainIndex, NodeBackedChainIndex,
    NodeBackedChainIndexSubscriber, TransactionHash,
};

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
    init_tracing();
    let network = Network::Regtest(ActivationHeights::default());
    // Long enough to have some finalized blocks to play with
    let segment_length = 120;
    // No need to worry about non-best chains for this test
    let branch_count = 1;

    // from this line to `runtime.block_on(async {` are all
    // copy-pasted. Could a macro get rid of some of this boilerplate?
    proptest::proptest!(proptest::test_runner::Config::with_cases(1), |(segments in make_branching_chain(branch_count, segment_length, network))| {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
        runtime.block_on(async {
            let (genesis_segment, branching_segments) = segments;
            let mockchain = ProptestMockchain {
                genesis_segment,
                branching_segments,
                // This number can be played with. We want to slow down
                // sync enough to trigger passthrough without
                // slowing down passthrough more than we need to
                delay: Some(Duration::from_millis(100)),
                best_branch_cache: Arc::new(std::sync::OnceLock::new()),
                tx_index: Arc::new(std::sync::OnceLock::new()),
            };
            let temp_dir: tempfile::TempDir = tempfile::tempdir().unwrap();
            let db_path: std::path::PathBuf = temp_dir.path().to_path_buf();

            let config = BlockCacheConfig {
                storage: StorageConfig {
                    database: DatabaseConfig {
                        path: db_path,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                db_version: 1,
                network,

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            let index_reader = indexer.subscriber();
            // 101 instead of 100 as heights are 0-indexed
            let expected_max_serviceable_height = (2 * segment_length) - 101;
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
                .unwrap()
                .0
                .saturating_sub(100)
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

            let config = BlockCacheConfig {
                storage: StorageConfig {
                    database: DatabaseConfig {
                        path: db_path,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                db_version: 1,
                network,

            };

            let indexer = NodeBackedChainIndex::new(mockchain.clone(), config)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await;
            let index_reader = indexer.subscriber();
            let snapshot = index_reader.snapshot_nonfinalized_state().await.unwrap();
            let non_finalized_snapshot = snapshot.get_nfs_snapshot().expect("not synced");
            let best_tip_hash = non_finalized_snapshot.best_tip.hash;
            let best_tip_block = non_finalized_snapshot
                .get_chainblock_by_hash(&best_tip_hash)
                .unwrap();
            for (hash, block) in &non_finalized_snapshot.blocks {
                if hash != &best_tip_hash {
                    assert!(block.chainwork().to_u256() <= best_tip_block.chainwork().to_u256());
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

#[async_trait]
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
    )> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let Some(chain_up_to_block) =
            self.get_block_and_all_preceeding(|block| block.hash().0 == id.0)
        else {
            return Ok((None, None));
        };

        let (sapling, orchard) =
            chain_up_to_block
                .iter()
                .fold((None, None), |(mut sapling, mut orchard), block| {
                    for transaction in &block.transactions {
                        for sap_commitment in transaction.sapling_note_commitments() {
                            let sap_commitment =
                                sapling_crypto::Node::from_bytes(sap_commitment.to_bytes())
                                    .unwrap();

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
                    }
                    (sapling, orchard)
                });
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
        ))
    }

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        _id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
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
