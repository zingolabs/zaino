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
    use std::{path::PathBuf, time::Duration};

    use tempfile::TempDir;
    use tokio_stream::StreamExt as _;
    use zaino_proto::proto::compact_formats::CompactBlock;
    use zebra_chain::serialization::ZcashDeserializeInto;

    use crate::{
        bench::BlockCacheConfig,
        chain_index::{
            source::test::MockchainSource,
            tests::vectors::{build_mockchain_source, load_test_vectors},
            types::TransactionHash,
            ChainIndex, NodeBackedChainIndex,
        },
        ChainBlock,
    };

    async fn load_test_vectors_and_sync_chain_index() -> (
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
    ) {
        let (blocks, _faucet, _recipient) = load_test_vectors().unwrap();

        let source = build_mockchain_source(blocks.clone());
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

        let indexer = NodeBackedChainIndex::new(source, config).await.unwrap();

        loop {
            if indexer.finalized_state.db_height().await.unwrap() == Some(crate::Height(100)) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        (blocks, indexer)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_block_range() {
        let (blocks, indexer) = load_test_vectors_and_sync_chain_index().await;
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
        let (blocks, indexer) = load_test_vectors_and_sync_chain_index().await;
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
        let (blocks, indexer) = load_test_vectors_and_sync_chain_index().await;
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

            let tx_status = indexer
                .get_transaction_status(
                    &nonfinalized_snapshot,
                    &TransactionHash::from(expected_txid),
                )
                .await
                .unwrap();
            assert_eq!(tx_status.len(), 1);
            let (hash, height) = tx_status.iter().next().unwrap();
            assert_eq!(hash.0, block_hash.0);
            assert_eq!(height.unwrap().0, block_height.unwrap().0);
        }
    }
}
