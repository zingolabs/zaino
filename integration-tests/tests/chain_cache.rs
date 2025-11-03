use zaino_common::network::ActivationHeights;
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_state::BackendType;
use zaino_testutils::{TestManager, Validator as _, ValidatorKind};

async fn create_test_manager_and_connector(
    validator: &ValidatorKind,
    activation_heights: Option<ActivationHeights>,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
) -> (TestManager, JsonRpSeeConnector) {
    let test_manager = TestManager::launch(
        validator,
        &BackendType::Fetch,
        None,
        activation_heights,
        chain_cache,
        enable_zaino,
        false,
        enable_clients,
    )
    .await
    .unwrap();

    let json_service = JsonRpSeeConnector::new_with_basic_auth(
        test_node_and_return_url(
            test_manager.full_node_rpc_listen_address,
            None,
            Some("xxxxxx".to_string()),
            Some("xxxxxx".to_string()),
        )
        .await
        .unwrap(),
        "xxxxxx".to_string(),
        "xxxxxx".to_string(),
    )
    .unwrap();
    (test_manager, json_service)
}

#[allow(deprecated)]
mod chain_query_interface {

    use std::{path::PathBuf, time::Duration};

    use futures::TryStreamExt as _;
    use tempfile::TempDir;
    use zaino_common::{
        network::ActivationHeights, CacheConfig, DatabaseConfig, ServiceConfig, StorageConfig,
    };
    use zaino_state::{
        chain_index::{
            source::ValidatorConnector,
            types::{BestChainLocation, TransactionHash},
            NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
        },
        test_dependencies::{
            chain_index::{self, ChainIndex},
            BlockCacheConfig,
        },
        Height, StateService, StateServiceConfig, ZcashService as _,
    };
    use zebra_chain::{
        parameters::NetworkKind,
        serialization::{ZcashDeserialize, ZcashDeserializeInto},
    };

    use super::*;

    #[allow(deprecated)]
    async fn create_test_manager_and_chain_index(
        validator: &ValidatorKind,
        chain_cache: Option<std::path::PathBuf>,
        enable_zaino: bool,
        enable_clients: bool,
    ) -> (
        TestManager,
        JsonRpSeeConnector,
        Option<StateService>,
        NodeBackedChainIndex,
        NodeBackedChainIndexSubscriber,
    ) {
        // until zaino is switched over to using chain index we will keep these activation heights separate.
        // TODO: unify acitvation heights after switchover to chain index
        let activation_heights = match validator {
            ValidatorKind::Zebrad => ActivationHeights {
                overwinter: Some(1),
                before_overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(2),
                nu6: Some(2),
                nu6_1: Some(1000),
                nu7: None,
            },
            ValidatorKind::Zcashd => ActivationHeights {
                overwinter: Some(1),
                before_overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(2),
                nu6: Some(2),
                nu6_1: Some(2),
                nu7: None,
            },
        };

        let (test_manager, json_service) = create_test_manager_and_connector(
            validator,
            Some(activation_heights),
            chain_cache.clone(),
            enable_zaino,
            enable_clients,
        )
        .await;

        match validator {
            ValidatorKind::Zebrad => {
                let state_chain_cache_dir = match chain_cache {
                    Some(dir) => dir,
                    None => test_manager.data_dir.clone(),
                };
                let network = match test_manager.network {
                    NetworkKind::Regtest => {
                        zebra_chain::parameters::Network::new_regtest(activation_heights.into())
                    }
                    NetworkKind::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
                    NetworkKind::Mainnet => zebra_chain::parameters::Network::Mainnet,
                };
                let state_service = StateService::spawn(StateServiceConfig::new(
                    zebra_state::Config {
                        cache_dir: state_chain_cache_dir,
                        ephemeral: false,
                        delete_old_database: true,
                        debug_stop_at_height: None,
                        debug_validity_check_interval: None,
                    },
                    test_manager.full_node_rpc_listen_address,
                    test_manager.full_node_grpc_listen_address,
                    false,
                    None,
                    None,
                    None,
                    ServiceConfig::default(),
                    StorageConfig {
                        cache: CacheConfig::default(),
                        database: DatabaseConfig {
                            path: test_manager
                                .local_net
                                .data_dir()
                                .path()
                                .to_path_buf()
                                .join("zaino"),
                            ..Default::default()
                        },
                    },
                    network.into(),
                ))
                .await
                .unwrap();
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
                    network: zaino_common::Network::Regtest(activation_heights),
                };
                let chain_index = NodeBackedChainIndex::new(
                    ValidatorConnector::State(chain_index::source::State {
                        read_state_service: state_service.read_state_service().clone(),
                        mempool_fetcher: json_service.clone(),
                        network: config.network,
                    }),
                    config,
                )
                .await
                .unwrap();
                let index_reader = chain_index.subscriber().await;
                tokio::time::sleep(Duration::from_secs(3)).await;

                (
                    test_manager,
                    json_service,
                    Some(state_service),
                    chain_index,
                    index_reader,
                )
            }
            ValidatorKind::Zcashd => {
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
                    network: zaino_common::Network::Regtest(activation_heights),
                };
                let chain_index = NodeBackedChainIndex::new(
                    ValidatorConnector::Fetch(json_service.clone()),
                    config,
                )
                .await
                .unwrap();
                let index_reader = chain_index.subscriber().await;
                tokio::time::sleep(Duration::from_secs(3)).await;

                (test_manager, json_service, None, chain_index, index_reader)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_block_range_zebrad() {
        get_block_range(&ValidatorKind::Zebrad).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_block_range_zcashd() {
        get_block_range(&ValidatorKind::Zcashd).await
    }

    async fn get_block_range(validator: &ValidatorKind) {
        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index(validator, None, false, false).await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = indexer.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 8);
        let range = indexer
            .get_block_range(&snapshot, Height::try_from(0).unwrap(), None)
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in range {
            let block = block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
            assert_eq!(
                block.hash().0,
                snapshot
                    .heights_to_hashes
                    .get(
                        &chain_index::types::Height::try_from(block.coinbase_height().unwrap())
                            .unwrap()
                    )
                    .unwrap()
                    .0
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_fork_point_zebrad() {
        find_fork_point(&ValidatorKind::Zebrad).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_fork_point_zcashd() {
        find_fork_point(&ValidatorKind::Zcashd).await
    }

    async fn find_fork_point(validator: &ValidatorKind) {
        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index(validator, None, false, false).await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = indexer.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 8);
        for block_hash in snapshot.heights_to_hashes.values() {
            // As all blocks are currently on the main chain,
            // this should be the block provided
            assert_eq!(
                block_hash,
                &indexer
                    .find_fork_point(&snapshot, block_hash)
                    .unwrap()
                    .unwrap()
                    .0
            )
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_raw_transaction_zebrad() {
        get_raw_transaction(&ValidatorKind::Zebrad).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_raw_transaction_zcashd() {
        get_raw_transaction(&ValidatorKind::Zcashd).await
    }

    async fn get_raw_transaction(validator: &ValidatorKind) {
        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index(validator, None, false, false).await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = indexer.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 8);
        for (txid, height) in snapshot.blocks.values().flat_map(|block| {
            block
                .transactions()
                .iter()
                .map(|txdata| (txdata.txid().0, block.height()))
        }) {
            let (raw_transaction, branch_id) = indexer
                .get_raw_transaction(&snapshot, &TransactionHash(txid))
                .await
                .unwrap()
                .unwrap();
            let zebra_txn =
                zebra_chain::transaction::Transaction::zcash_deserialize(&raw_transaction[..])
                    .unwrap();

            assert_eq!(
                branch_id,
                if height == Some(chain_index::types::GENESIS_HEIGHT) {
                    None
                } else if height == Some(Height::try_from(1).unwrap()) {
                    zebra_chain::parameters::NetworkUpgrade::Canopy
                        .branch_id()
                        .map(u32::from)
                } else {
                    zebra_chain::parameters::NetworkUpgrade::Nu6
                        .branch_id()
                        .map(u32::from)
                }
            );

            let correct_txid = zebra_txn.hash().0;

            assert_eq!(txid, correct_txid);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_transaction_status_zebrad() {
        get_transaction_status(&ValidatorKind::Zebrad).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_transaction_status_zcashd() {
        get_transaction_status(&ValidatorKind::Zcashd).await
    }

    async fn get_transaction_status(validator: &ValidatorKind) {
        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index(validator, None, false, false).await;
        let snapshot = indexer.snapshot_nonfinalized_state();
        // I don't know where this second block is generated. Somewhere in the
        // guts of create_test_manager_and_chain_index
        assert_eq!(snapshot.as_ref().blocks.len(), 3);

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = indexer.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 8);
        for (txid, height, block_hash) in snapshot.blocks.values().flat_map(|block| {
            block
                .transactions()
                .iter()
                .map(|txdata| (txdata.txid().0, block.height(), block.hash()))
        }) {
            let (transaction_status_best_chain, transaction_status_nonbest_chain) = indexer
                .get_transaction_status(&snapshot, &TransactionHash(txid))
                .await
                .unwrap();
            assert_eq!(
                transaction_status_best_chain.unwrap(),
                BestChainLocation::Block(*block_hash, height.unwrap())
            );
            assert!(transaction_status_nonbest_chain.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_large_chain_zebrad() {
        sync_large_chain(&ValidatorKind::Zebrad).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sync_large_chain_zcashd() {
        sync_large_chain(&ValidatorKind::Zcashd).await
    }

    async fn sync_large_chain(validator: &ValidatorKind) {
        let (test_manager, json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index(validator, None, false, false).await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        {
            let chain_height =
                Height::try_from(json_service.get_blockchain_info().await.unwrap().blocks.0)
                    .unwrap();
            let indexer_height = indexer.snapshot_nonfinalized_state().best_tip.height;
            assert_eq!(chain_height, indexer_height);
        }

        test_manager.generate_blocks_with_delay(150).await;

        tokio::time::sleep(std::time::Duration::from_millis(5000)).await;

        let snapshot = indexer.snapshot_nonfinalized_state();
        let chain_height = json_service.get_blockchain_info().await.unwrap().blocks.0;
        let indexer_height = snapshot.best_tip.height;
        assert_eq!(Height::try_from(chain_height).unwrap(), indexer_height);

        let finalised_start = Height::try_from(chain_height - 150).unwrap();
        let finalised_tip = Height::try_from(chain_height - 100).unwrap();
        let end = Height::try_from(chain_height - 50).unwrap();

        let finalized_blocks = indexer
            .get_block_range(&snapshot, finalised_start, Some(finalised_tip))
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in finalized_blocks {
            block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
        }

        let non_finalised_blocks = indexer
            .get_block_range(&snapshot, finalised_tip, Some(end))
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in non_finalised_blocks {
            block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
        }
    }
}
