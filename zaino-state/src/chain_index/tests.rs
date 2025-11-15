//! Zaino-State ChainIndex unit tests.

pub(crate) mod finalised_state;
pub(crate) mod mempool;
pub(crate) mod vectors;

pub(crate) fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .try_init()
        .unwrap();
}

mod mockchain_tests {
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration};
    use tokio_stream::StreamExt as _;
    use zaino_common::{network::ActivationHeights, DatabaseConfig, Network, StorageConfig};
    use zebra_chain::serialization::ZcashDeserializeInto;

    use crate::{
        chain_index::{
            source::test::MockchainSource,
            tests::vectors::{
                build_active_mockchain_source, build_mockchain_source, load_test_vectors,
                TestVectorBlockData,
            },
            types::{BestChainLocation, TransactionHash},
            ChainIndex, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
        },
        BlockCacheConfig,
    };

    async fn load_test_vectors_and_sync_chain_index(
        active_mockchain_source: bool,
    ) -> (
        Vec<TestVectorBlockData>,
        NodeBackedChainIndex<MockchainSource>,
        NodeBackedChainIndexSubscriber<MockchainSource>,
        MockchainSource,
    ) {
        super::init_tracing();

        let blocks = load_test_vectors().unwrap().blocks;

        let source = if active_mockchain_source {
            build_active_mockchain_source(150, blocks.clone())
        } else {
            build_mockchain_source(blocks.clone())
        };

        // TODO: the temp_dir is deleted when it goes out of scope
        // at the end of this function.
        // Somehow, this isn't breaking the database, but I'm confused
        // as to how the database works when the directory containing
        // it is deleted
        let temp_dir: TempDir = tempfile::tempdir().unwrap();
        let db_path: PathBuf = temp_dir.path().to_path_buf();

        let config = BlockCacheConfig {
            storage: StorageConfig {
                database: DatabaseConfig {
                    path: db_path,
                    ..Default::default()
                },
                ..Default::default()
            },
            db_version: 1,
            network: Network::Regtest(ActivationHeights::default()),
        };

        let indexer = NodeBackedChainIndex::new(source.clone(), config)
            .await
            .unwrap();
        let index_reader = indexer.subscriber().await;

        loop {
            let check_height: u32 = match active_mockchain_source {
                true => source.active_height() - 100,
                false => 100,
            };
            if index_reader.finalized_state.db_height().await.unwrap()
                == Some(crate::Height(check_height))
            {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        (blocks, indexer, index_reader, source)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range() {
        let (blocks, _indexer, index_reader, _mockchain) =
            load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();

        let start = crate::Height(0);

        let indexer_blocks =
            ChainIndex::get_block_range(&index_reader, &nonfinalized_snapshot, start, None)
                .unwrap()
                .collect::<Vec<_>>()
                .await;

        for (i, block) in indexer_blocks.into_iter().enumerate() {
            let parsed_block = block
                .unwrap()
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();

            let expected_block = &blocks[i].zebra_block;
            assert_eq!(&parsed_block, expected_block);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction() {
        let (blocks, _indexer, index_reader, _mockchain) =
            load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();
        for (expected_transaction, height) in blocks.into_iter().flat_map(|block| {
            block
                .zebra_block
                .transactions
                .into_iter()
                .map(move |transaction| (transaction, block.height))
        }) {
            let (transaction, branch_id) = index_reader
                .get_raw_transaction(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_transaction.hash()),
                )
                .await
                .unwrap()
                .unwrap();
            let zaino_transaction = transaction
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .unwrap();
            assert_eq!(expected_transaction.as_ref(), &zaino_transaction);
            assert_eq!(
                branch_id,
                if height == 0 {
                    None
                } else if height == 1 {
                    zebra_chain::parameters::NetworkUpgrade::Canopy
                        .branch_id()
                        .map(u32::from)
                } else {
                    zebra_chain::parameters::NetworkUpgrade::Nu6
                        .branch_id()
                        .map(u32::from)
                }
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_status() {
        let (blocks, _indexer, index_reader, _mockchain) =
            load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();

        for (expected_transaction, block_hash, block_height) in
            blocks.into_iter().flat_map(|block| {
                block
                    .zebra_block
                    .transactions
                    .iter()
                    .cloned()
                    .map(|transaction| {
                        (
                            transaction,
                            block.zebra_block.hash(),
                            block.zebra_block.coinbase_height(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
            })
        {
            let expected_txid = expected_transaction.hash();

            let (transaction_status_best_chain, transaction_status_nonbest_chain) = index_reader
                .get_transaction_status(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_txid),
                )
                .await
                .unwrap();
            assert_eq!(
                transaction_status_best_chain.unwrap(),
                BestChainLocation::Block(
                    crate::BlockHash(block_hash.0),
                    crate::Height(block_height.unwrap().0)
                )
            );
            assert!(transaction_status_nonbest_chain.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sync_blocks_after_startup() {
        let (_blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;

        let indexer_tip = dbg!(&index_reader.snapshot_nonfinalized_state().best_tip)
            .height
            .0;
        let active_mockchain_tip = dbg!(mockchain.active_height());
        assert_eq!(active_mockchain_tip, indexer_tip);

        for _ in 0..20 {
            mockchain.mine_blocks(1);
            sleep(Duration::from_millis(600)).await;
        }
        sleep(Duration::from_millis(2000)).await;

        let indexer_tip = dbg!(&index_reader.snapshot_nonfinalized_state().best_tip)
            .height
            .0;
        let active_mockchain_tip = dbg!(mockchain.active_height());
        assert_eq!(active_mockchain_tip, indexer_tip);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_transaction() {
        let (blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
            .collect();

        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;

        let mempool_transactions: Vec<_> = block_data
            .get(mempool_height)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();
        for expected_transaction in mempool_transactions.into_iter() {
            let (transaction, branch_id) = index_reader
                .get_raw_transaction(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_transaction.hash()),
                )
                .await
                .unwrap()
                .unwrap();
            let zaino_transaction = transaction
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .unwrap();
            assert_eq!(expected_transaction.as_ref(), &zaino_transaction);
            assert_eq!(
                branch_id,
                zebra_chain::parameters::NetworkUpgrade::Nu6
                    .branch_id()
                    .map(u32::from)
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_transaction_status() {
        let (blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
            .collect();

        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;

        let mempool_transactions: Vec<_> = block_data
            .get(mempool_height)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();
        for expected_transaction in mempool_transactions.into_iter() {
            let expected_txid = expected_transaction.hash();

            let (transaction_status_best_chain, transaction_status_nonbest_chain) = index_reader
                .get_transaction_status(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_txid),
                )
                .await
                .unwrap();
            assert_eq!(
                transaction_status_best_chain,
                Some(BestChainLocation::Mempool(
                    crate::chain_index::types::Height(mempool_height as u32)
                ))
            );
            assert!(transaction_status_nonbest_chain.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_transactions() {
        let (blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
            .collect();

        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mut mempool_transactions: Vec<_> = block_data
            .get(mempool_height)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        mempool_transactions.sort_by_key(|a| a.hash());

        let mut found_mempool_transactions: Vec<zebra_chain::transaction::Transaction> =
            index_reader
                .get_mempool_transactions(Vec::new())
                .await
                .unwrap()
                .iter()
                .map(|txn_bytes| {
                    txn_bytes
                        .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                        .unwrap()
                })
                .collect();
        found_mempool_transactions.sort_by_key(|a| a.hash());
        assert_eq!(
            mempool_transactions
                .iter()
                .map(|tx| tx.as_ref().clone())
                .collect::<Vec<_>>(),
            found_mempool_transactions,
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_filtered_mempool_transactions() {
        let (blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
            .collect();

        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mut mempool_transactions: Vec<_> = block_data
            .get(mempool_height)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let exclude_tx = mempool_transactions.pop().unwrap();
        dbg!(&exclude_tx.hash());

        // Reverse format to client type.
        //
        // TODO: Explore whether this is the correct byte order or whether we
        // replicated a bug in old code.
        let exclude_txid: String = exclude_tx
            .hash()
            .to_string()
            .chars()
            .collect::<Vec<_>>()
            .chunks(2)
            .rev()
            .map(|chunk| chunk.iter().collect::<String>())
            .collect();
        dbg!(&exclude_txid);
        mempool_transactions.sort_by_key(|a| a.hash());

        let mut found_mempool_transactions: Vec<zebra_chain::transaction::Transaction> =
            index_reader
                .get_mempool_transactions(vec![exclude_txid])
                .await
                .unwrap()
                .iter()
                .map(|txn_bytes| {
                    txn_bytes
                        .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                        .unwrap()
                })
                .collect();
        found_mempool_transactions.sort_by_key(|a| a.hash());
        assert_eq!(mempool_transactions.len(), found_mempool_transactions.len());
        assert_eq!(
            mempool_transactions
                .iter()
                .map(|tx| tx.as_ref().clone())
                .collect::<Vec<_>>(),
            found_mempool_transactions,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn get_mempool_stream() {
        let (blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;

        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|TestVectorBlockData { zebra_block, .. }| zebra_block.clone())
            .collect();

        sleep(Duration::from_millis(2000)).await;

        let next_mempool_height_index = (dbg!(mockchain.active_height()) as usize) + 1;
        let mut mempool_transactions: Vec<_> = block_data
            .get(next_mempool_height_index)
            .map(|b| {
                b.transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        mempool_transactions.sort_by_key(|transaction| transaction.hash());

        let mempool_stream_task = tokio::spawn(async move {
            let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();
            let mut mempool_stream = index_reader
                .get_mempool_stream(&nonfinalized_snapshot)
                .expect("failed to create mempool stream");

            let mut indexer_mempool_transactions: Vec<zebra_chain::transaction::Transaction> =
                Vec::new();

            while let Some(tx_bytes_res) = mempool_stream.next().await {
                let tx_bytes = tx_bytes_res.expect("stream error");
                let tx: zebra_chain::transaction::Transaction =
                    tx_bytes.zcash_deserialize_into().expect("deserialize tx");
                indexer_mempool_transactions.push(tx);
            }

            indexer_mempool_transactions.sort_by_key(|tx| tx.hash());
            indexer_mempool_transactions
        });

        sleep(Duration::from_millis(500)).await;

        mockchain.mine_blocks(1);

        let indexer_mempool_stream_transactions =
            mempool_stream_task.await.expect("collector task failed");

        assert_eq!(
            mempool_transactions
                .iter()
                .map(|tx| tx.as_ref().clone())
                .collect::<Vec<_>>(),
            indexer_mempool_stream_transactions,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn get_mempool_stream_for_stale_snapshot() {
        let (_blocks, _indexer, index_reader, mockchain) =
            load_test_vectors_and_sync_chain_index(true).await;
        sleep(Duration::from_millis(2000)).await;

        let stale_nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();

        mockchain.mine_blocks(1);
        sleep(Duration::from_millis(2000)).await;

        let mempool_stream = index_reader.get_mempool_stream(&stale_nonfinalized_snapshot);

        assert!(mempool_stream.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_height() {
        let (blocks, _indexer, index_reader, _mockchain) =
            load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = index_reader.snapshot_nonfinalized_state();

        // Positive cases: every known best-chain block returns its height
        for TestVectorBlockData {
            height,
            zebra_block,
            ..
        } in blocks.iter()
        {
            let got = index_reader
                .get_block_height(
                    &nonfinalized_snapshot,
                    crate::BlockHash(zebra_block.hash().0),
                )
                .await
                .unwrap();
            assert_eq!(got, Some(crate::Height(*height)));
        }

        // Negative case: an unknown hash returns None
        let unknown = crate::BlockHash([0u8; 32]);
        let got = index_reader
            .get_block_height(&nonfinalized_snapshot, unknown)
            .await
            .unwrap();
        assert_eq!(got, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_treestate() {
        let (blocks, _indexer, index_reader, _mockchain) =
            load_test_vectors_and_sync_chain_index(false).await;

        for TestVectorBlockData {
            zebra_block,
            sapling_tree_state,
            orchard_tree_state,
            ..
        } in blocks.into_iter()
        {
            let (sapling_bytes_opt, orchard_bytes_opt) = index_reader
                .get_treestate(&crate::BlockHash(zebra_block.hash().0))
                .await
                .unwrap();

            assert_eq!(
                sapling_bytes_opt.as_deref(),
                Some(sapling_tree_state.as_slice())
            );
            assert_eq!(
                orchard_bytes_opt.as_deref(),
                Some(orchard_tree_state.as_slice())
            );
        }
    }
}
