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
    use zaino_proto::proto::compact_formats::CompactBlock;
    use zebra_chain::serialization::ZcashDeserializeInto;

    use crate::{
        bench::BlockCacheConfig,
        chain_index::{
            source::test::MockchainSource,
            tests::vectors::{
                build_active_mockchain_source, build_mockchain_source, load_test_vectors,
            },
            types::TransactionHash,
            ChainIndex, NodeBackedChainIndex,
        },
        ChainBlock,
    };

    async fn load_test_vectors_and_sync_chain_index(
        active_mockchain_source: bool,
    ) -> (
        Vec<(
            u32,
            ChainBlock,
            CompactBlock,
            zebra_chain::block::Block,
            (
                zebra_chain::sapling::tree::Root,
                u64,
                zebra_chain::orchard::tree::Root,
                u64,
            ),
        )>,
        NodeBackedChainIndex<MockchainSource>,
        MockchainSource,
    ) {
        let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

        let source = if active_mockchain_source {
            build_active_mockchain_source(1, blocks.clone())
        } else {
            build_mockchain_source(blocks.clone())
        };

        let temp_dir: TempDir = tempfile::tempdir().unwrap();
        let db_path: PathBuf = temp_dir.path().to_path_buf();

        let config = BlockCacheConfig {
            map_capacity: None,
            map_shard_amount: None,
            db_version: 1,
            db_path,
            db_size: None,
            network: zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
                    nu6_1: None,
                    nu7: None,
                },
            ),
            no_sync: false,
            no_db: false,
        };

        let indexer = NodeBackedChainIndex::new(source.clone(), config)
            .await
            .unwrap();

        loop {
            if indexer.finalized_state.db_height().await.unwrap() == Some(crate::Height(100)) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        (blocks, indexer, source)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_block_range() {
        let (blocks, indexer, _mockchain) = load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = indexer.snapshot_nonfinalized_state();

        let start = crate::Height(0);

        let indexer_blocks =
            ChainIndex::get_block_range(&indexer, &nonfinalized_snapshot, start, None)
                .unwrap()
                .collect::<Vec<_>>()
                .await;

        for (i, block) in indexer_blocks.into_iter().enumerate() {
            let parsed_block = block
                .unwrap()
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();

            let expected_block = &blocks[i].3;
            assert_eq!(&parsed_block, expected_block);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_raw_transaction() {
        let (blocks, indexer, _mockchain) = load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = indexer.snapshot_nonfinalized_state();
        for expected_transaction in blocks
            .into_iter()
            .flat_map(|block| block.3.transactions.into_iter())
        {
            let zaino_transaction = indexer
                .get_raw_transaction(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_transaction.hash()),
                )
                .await
                .unwrap()
                .unwrap()
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .unwrap();
            assert_eq!(expected_transaction.as_ref(), &zaino_transaction)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_transaction_status() {
        let (blocks, indexer, _mockchain) = load_test_vectors_and_sync_chain_index(false).await;
        let nonfinalized_snapshot = indexer.snapshot_nonfinalized_state();

        for (expected_transaction, block_hash, block_height) in
            blocks.into_iter().flat_map(|block| {
                block
                    .3
                    .transactions
                    .iter()
                    .cloned()
                    .map(|transaction| (transaction, block.3.hash(), block.3.coinbase_height()))
                    .collect::<Vec<_>>()
                    .into_iter()
            })
        {
            let expected_txid = expected_transaction.hash();

            let (tx_status_blocks, _tx_mempool_status) = indexer
                .get_transaction_status(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_txid),
                )
                .await
                .unwrap();
            assert_eq!(tx_status_blocks.len(), 1);
            let (hash, height) = tx_status_blocks.iter().next().unwrap();
            assert_eq!(hash.0, block_hash.0);
            assert_eq!(height.unwrap().0, block_height.unwrap().0);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_mempool_transaction() {
        let (blocks, indexer, mockchain) = load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|(_height, _chain_block, _compact_block, zebra_block, _roots)| zebra_block.clone())
            .collect();

        mockchain.mine_blocks(150);
        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mempool_transactions = block_data
            .get(mempool_height)
            .map(|b| b.transactions.clone())
            .unwrap_or_default();

        let nonfinalized_snapshot = indexer.snapshot_nonfinalized_state();
        for expected_transaction in mempool_transactions.into_iter() {
            let zaino_transaction = indexer
                .get_raw_transaction(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_transaction.hash()),
                )
                .await
                .unwrap()
                .unwrap()
                .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                .unwrap();
            assert_eq!(expected_transaction.as_ref(), &zaino_transaction)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_mempool_transaction_status() {
        let (blocks, indexer, mockchain) = load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|(_height, _chain_block, _compact_block, zebra_block, _roots)| zebra_block.clone())
            .collect();

        mockchain.mine_blocks(150);
        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mempool_transactions = block_data
            .get(mempool_height)
            .map(|b| b.transactions.clone())
            .unwrap_or_default();

        let nonfinalized_snapshot = indexer.snapshot_nonfinalized_state();
        for expected_transaction in mempool_transactions.into_iter() {
            let expected_txid = expected_transaction.hash();

            let (tx_status_blocks, tx_mempool_status) = indexer
                .get_transaction_status(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_txid),
                )
                .await
                .unwrap();
            assert!(tx_status_blocks.is_empty());
            assert!(tx_mempool_status);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_mempool_transactions() {
        let (blocks, indexer, mockchain) = load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|(_height, _chain_block, _compact_block, zebra_block, _roots)| zebra_block.clone())
            .collect();

        mockchain.mine_blocks(150);
        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mut mempool_transactions = block_data
            .get(mempool_height)
            .map(|b| b.transactions.clone())
            .unwrap_or_default();
        mempool_transactions.sort_by_key(|a| a.hash());

        let mut found_mempool_transactions: Vec<zebra_chain::transaction::Transaction> = indexer
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_filtered_mempool_transactions() {
        let (blocks, indexer, mockchain) = load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|(_height, _chain_block, _compact_block, zebra_block, _roots)| zebra_block.clone())
            .collect();

        mockchain.mine_blocks(150);
        sleep(Duration::from_millis(2000)).await;

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mut mempool_transactions = block_data
            .get(mempool_height)
            .map(|b| b.transactions.clone())
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

        let mut found_mempool_transactions: Vec<zebra_chain::transaction::Transaction> = indexer
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
        let (blocks, indexer, mockchain) = load_test_vectors_and_sync_chain_index(true).await;
        let block_data: Vec<zebra_chain::block::Block> = blocks
            .iter()
            .map(|(_height, _chain_block, _compact_block, zebra_block, _roots)| zebra_block.clone())
            .collect();

        dbg!(indexer.snapshot_nonfinalized_state().best_tip);

        for _ in 0..150 {
            mockchain.mine_blocks(1);
            sleep(Duration::from_millis(200)).await;
        }
        sleep(Duration::from_millis(2000)).await;

        dbg!(indexer.snapshot_nonfinalized_state().best_tip);

        let mempool_height = (dbg!(mockchain.active_height()) as usize) + 1;
        let mut mempool_transactions = block_data
            .get(mempool_height)
            .map(|b| b.transactions.clone())
            .unwrap_or_default();
        mempool_transactions.sort_by_key(|a| a.hash());

        let nonfinalized_snapshot = indexer.snapshot_nonfinalized_state();
        let stream = indexer.get_mempool_stream(&nonfinalized_snapshot).unwrap();

        let mut streamed: Vec<zebra_chain::transaction::Transaction> = stream
            .take(mempool_transactions.len() + 1)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|res| res.unwrap())
            .map(|bytes| {
                bytes
                    .zcash_deserialize_into::<zebra_chain::transaction::Transaction>()
                    .unwrap()
            })
            .collect();
        streamed.sort_by_key(|a| a.hash());

        assert_eq!(
            mempool_transactions
                .iter()
                .map(|tx| tx.as_ref().clone())
                .collect::<Vec<_>>(),
            streamed,
        );
    }
}
