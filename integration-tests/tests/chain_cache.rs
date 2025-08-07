use zaino_commons::config::{BackendType, CookieAuth, ValidatorConfig, ZainoStateConfig};
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_state::bench::chain_index::non_finalised_state::{BlockchainSource, NonFinalizedState};
use zaino_testutils::{TestManager, Validator as _, ValidatorKind};

async fn create_test_manager_and_connector(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (TestManager, JsonRpSeeConnector) {
    let test_manager = TestManager::launch(
        validator,
        &BackendType::Fetch,
        None,
        chain_cache,
        enable_zaino,
        false,
        false,
        zaino_no_sync,
        zaino_no_db,
        enable_clients,
    )
    .await
    .unwrap();

    let json_service = JsonRpSeeConnector::new_with_basic_auth(
        test_node_and_return_url(&ValidatorConfig {
            config: ZainoStateConfig {
                ..Default::default()
            },
            rpc_address: test_manager.zebrad_rpc_listen_address,
            indexer_rpc_address: test_manager.zebrad_grpc_listen_address,
            cookie: CookieAuth::Disabled,
            rpc_user: "xxxxxx".to_string(),
            rpc_password: "xxxxxx".to_string(),
        })
        .await
        .unwrap(),
        "xxxxxx".to_string(),
        "xxxxxx".to_string(),
    )
    .unwrap();
    (test_manager, json_service)
}

async fn create_test_manager_and_nfs(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (
    TestManager,
    JsonRpSeeConnector,
    zaino_state::bench::chain_index::non_finalised_state::NonFinalizedState,
) {
    let (test_manager, json_service) = create_test_manager_and_connector(
        validator,
        chain_cache,
        enable_zaino,
        zaino_no_sync,
        zaino_no_db,
        enable_clients,
    )
    .await;

    // todo! unify duplicated Network enums
    let network = match test_manager.network {
        zaino_testutils::Network::Regtest => zebra_chain::parameters::Network::new_regtest(
            zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                before_overwinter: Some(1),
                overwinter: Some(1),
                sapling: Some(1),
                blossom: Some(1),
                heartwood: Some(1),
                canopy: Some(1),
                nu5: Some(1),
                nu6: Some(1),
                // TODO: What is network upgrade 6.1? What does a minor version NU mean?
                nu6_1: None,
                nu7: None,
            },
        ),
        zaino_testutils::Network::Testnet => {
            zebra_chain::parameters::Network::new_default_testnet()
        }
        zaino_testutils::Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
    };

    let non_finalized_state =
        NonFinalizedState::initialize(BlockchainSource::Fetch(json_service.clone()), network)
            .await
            .unwrap();

    (test_manager, json_service, non_finalized_state)
}

#[tokio::test]
async fn nfs_simple_sync() {
    let (test_manager, _json_service, non_finalized_state) =
        create_test_manager_and_nfs(&ValidatorKind::Zebrad, None, true, false, false, true).await;

    let snapshot = non_finalized_state.get_snapshot();
    assert_eq!(
        snapshot.best_tip.0,
        zaino_state::Height::try_from(1).unwrap()
    );

    test_manager.generate_blocks_with_delay(5).await;
    non_finalized_state.sync().await.unwrap();
    let snapshot = non_finalized_state.get_snapshot();
    assert_eq!(
        snapshot.best_tip.0,
        zaino_state::Height::try_from(6).unwrap()
    );
}

mod chain_query_interface {

    use futures::TryStreamExt as _;
    use zaino_commons::config::{CacheConfig, CookieAuth, DatabaseConfig};
    use zaino_state::{
        bench::chain_index::{
            self,
            interface::{ChainIndex, NodeBackedChainIndex},
        },
        StateService, StateServiceConfig, ZcashService as _,
    };
    use zebra_chain::serialization::{ZcashDeserialize, ZcashDeserializeInto};

    use super::*;

    async fn create_test_manager_and_chain_index(
        validator: &ValidatorKind,
        chain_cache: Option<std::path::PathBuf>,
        enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
        enable_clients: bool,
    ) -> (TestManager, StateService, NodeBackedChainIndex) {
        let (test_manager, _json_service) = create_test_manager_and_connector(
            validator,
            chain_cache.clone(),
            enable_zaino,
            zaino_no_sync,
            zaino_no_db,
            enable_clients,
        )
        .await;

        let state_chain_cache_dir = match chain_cache {
            Some(dir) => dir,
            None => test_manager.data_dir.clone(),
        };
        // todo! move into a method, scrap the string layer
        let network = match test_manager.network.to_string().as_str() {
            "Regtest" => zebra_chain::parameters::Network::new_regtest(
                zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                    before_overwinter: Some(1),
                    overwinter: Some(1),
                    sapling: Some(1),
                    blossom: Some(1),
                    heartwood: Some(1),
                    canopy: Some(1),
                    nu5: Some(1),
                    nu6: Some(1),
                    // TODO: What is network upgrade 6.1? What does a minor version NU mean?
                    nu6_1: None,
                    nu7: None,
                },
            ),
            "Testnet" => zebra_chain::parameters::Network::new_default_testnet(),
            "Mainnet" => zebra_chain::parameters::Network::Mainnet,
            _ => panic!("Incorrect newtork type found."),
        };

        let state_service_config: StateServiceConfig = StateServiceConfig {
            validator: zaino_commons::config::ValidatorConfig {
                config: zaino_commons::config::ZainoStateConfig {
                    cache_dir: state_chain_cache_dir,
                    ephemeral: false,
                    delete_old_database: true,
                    debug_stop_at_height: None,
                    debug_validity_check_interval: None,
                },
                rpc_address: test_manager.zebrad_rpc_listen_address,
                indexer_rpc_address: false,
                cookie: CookieAuth::Disabled,
                rpc_user: None,
                rpc_password: None,
            },
            service: zaino_commons::config::ServiceConfig {
                timeout: None,
                channel_size: None,
            },
            block_cache: zaino_state::config::BlockCacheConfig {
                cache: CacheConfig {
                    capacity: None,
                    shard_amount: None,
                },
                database: DatabaseConfig {
                    path: test_manager
                        .local_net
                        .data_dir()
                        .path()
                        .to_path_buf()
                        .join("zaino"),
                    size: None,
                },
                network: network.clone(),
                no_sync: false,
                no_db: false,
            },
        };

        let state_service = StateService::spawn(state_service_config).await.unwrap();

        let chain_index = NodeBackedChainIndex::new(
            BlockchainSource::State(state_service.read_state_service().clone()),
            network,
        )
        .await
        .unwrap();

        (test_manager, state_service, chain_index)
    }

    #[tokio::test]
    async fn get_block_range() {
        let (test_manager, _json_service, chain_index) = create_test_manager_and_chain_index(
            &ValidatorKind::Zebrad,
            None,
            true,
            false,
            false,
            true,
        )
        .await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = chain_index.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 6);
        let range = chain_index
            .get_block_range(&snapshot, None, None)
            .unwrap()
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
    #[tokio::test]
    async fn find_fork_point() {
        let (test_manager, _json_service, chain_index) = create_test_manager_and_chain_index(
            &ValidatorKind::Zebrad,
            None,
            true,
            false,
            false,
            true,
        )
        .await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = chain_index.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 6);
        for block_hash in snapshot.heights_to_hashes.values() {
            // As all blocks are currently on the main chain,
            // this should be the block provided
            assert_eq!(
                block_hash,
                &chain_index
                    .find_fork_point(&snapshot, block_hash)
                    .unwrap()
                    .unwrap()
                    .0
            )
        }
    }
    #[tokio::test]
    async fn get_raw_transaction() {
        let (test_manager, _json_service, chain_index) = create_test_manager_and_chain_index(
            &ValidatorKind::Zebrad,
            None,
            true,
            false,
            false,
            true,
        )
        .await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = chain_index.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 6);
        for txid in snapshot
            .blocks
            .values()
            .flat_map(|block| block.transactions().iter().map(|txdata| txdata.txid()))
        {
            let raw_transaction = chain_index
                .get_raw_transaction(&snapshot, *txid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                *txid,
                zebra_chain::transaction::Transaction::zcash_deserialize(&raw_transaction[..])
                    .unwrap()
                    .hash()
                    .0
            );
        }
    }
    #[tokio::test]
    async fn get_transaction_status() {
        let (test_manager, _json_service, chain_index) = create_test_manager_and_chain_index(
            &ValidatorKind::Zebrad,
            None,
            true,
            false,
            false,
            true,
        )
        .await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = chain_index.snapshot_nonfinalized_state();
        assert_eq!(snapshot.as_ref().blocks.len(), 6);
        for (txid, height, block_hash) in snapshot.blocks.values().flat_map(|block| {
            block
                .transactions()
                .iter()
                .map(|txdata| (txdata.txid(), block.height(), block.hash()))
        }) {
            let transaction_status = chain_index
                .get_transaction_status(&snapshot, *txid)
                .unwrap();
            assert_eq!(1, transaction_status.len());
            assert_eq!(transaction_status.keys().next().unwrap(), block_hash);
            assert_eq!(transaction_status.values().next().unwrap(), &height)
        }
    }
}
