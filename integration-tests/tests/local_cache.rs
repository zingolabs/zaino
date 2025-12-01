use zaino_common::{DatabaseConfig, StorageConfig};
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
#[allow(deprecated)]
use zaino_state::FetchService;
use zaino_state::{
    test_dependencies::{BlockCache, BlockCacheConfig, BlockCacheSubscriber},
    BackendType,
};
use zaino_testutils::{TestManager, ValidatorKind};
use zcash_local_net::validator::Validator as _;
use zebra_chain::{block::Height, parameters::NetworkKind};
use zebra_state::HashOrHeight;

#[allow(deprecated)]
async fn create_test_manager_and_block_cache(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
) -> (
    TestManager<FetchService>,
    JsonRpSeeConnector,
    BlockCache,
    BlockCacheSubscriber,
) {
    let test_manager = TestManager::<FetchService>::launch(
        validator,
        &BackendType::Fetch,
        None,
        None,
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

    let network = match test_manager.network {
        NetworkKind::Regtest => zebra_chain::parameters::Network::new_regtest(
            test_manager.local_net.get_activation_heights(),
        ),
        NetworkKind::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
        NetworkKind::Mainnet => zebra_chain::parameters::Network::Mainnet,
    };

    let block_cache_config = BlockCacheConfig {
        storage: StorageConfig {
            database: DatabaseConfig {
                path: test_manager.data_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        },
        db_version: 1,
        network: network.into(),
    };

    let block_cache = BlockCache::spawn(&json_service, None, block_cache_config)
        .await
        .unwrap();

    let block_cache_subscriber = block_cache.subscriber();

    (
        test_manager,
        json_service,
        block_cache,
        block_cache_subscriber,
    )
}

async fn launch_local_cache(validator: &ValidatorKind) {
    create_test_manager_and_block_cache(validator, None, false, false).await;
}

/// Launches a testmanager and block cache and generates `n*100` blocks, checking blocks are stored and fetched correctly.
async fn launch_local_cache_process_n_block_batches(validator: &ValidatorKind, batches: u32) {
    let (test_manager, json_service, mut block_cache, mut block_cache_subscriber) =
        create_test_manager_and_block_cache(validator, None, true, false).await;

    let finalised_state = block_cache.finalised_state.take().unwrap();
    let finalised_state_subscriber = block_cache_subscriber.finalised_state.take().unwrap();

    for _ in 1..=batches {
        test_manager.generate_blocks_and_poll(100).await;

        // Check chain height in validator, non-finalised state and finalised state.
        let validator_height = dbg!(json_service.get_blockchain_info().await.unwrap().blocks.0);
        let non_finalised_state_height =
            dbg!(block_cache_subscriber.get_chain_height().await.unwrap().0);
        let finalised_state_height =
            dbg!(dbg!(finalised_state.get_db_height()).unwrap_or(Height(0)).0);

        assert_eq!(&validator_height, &non_finalised_state_height);
        assert_eq!(
            (&non_finalised_state_height.saturating_sub(101)),
            &finalised_state_height
        );

        // Fetch blocks in non-finalised state.
        let mut non_finalised_state_blocks = Vec::new();
        for height in (finalised_state_height + 1)..=non_finalised_state_height {
            let block = block_cache_subscriber
                .non_finalised_state
                .get_compact_block(HashOrHeight::Height(Height(height)))
                .await
                .unwrap();
            non_finalised_state_blocks.push(block);
        }

        // Fetch blocks in finalised state.
        let mut finalised_state_blocks = Vec::new();
        for height in 1..=finalised_state_height {
            let block = finalised_state_subscriber
                .get_compact_block(HashOrHeight::Height(Height(height)))
                .await
                .unwrap();
            finalised_state_blocks.push(block);
        }

        dbg!(non_finalised_state_blocks.first());
        dbg!(non_finalised_state_blocks.last());
        dbg!(finalised_state_blocks.first());
        dbg!(finalised_state_blocks.last());
    }
}

mod zcashd {
    use zaino_testutils::ValidatorKind;

    use crate::{launch_local_cache, launch_local_cache_process_n_block_batches};

    #[tokio::test]
    async fn launch_local_cache_zcashd() {
        launch_local_cache(&ValidatorKind::Zcashd).await;
    }

    #[tokio::test]
    async fn process_100_blocks() {
        launch_local_cache_process_n_block_batches(&ValidatorKind::Zcashd, 1).await;
    }

    #[tokio::test]
    async fn process_200_blocks() {
        launch_local_cache_process_n_block_batches(&ValidatorKind::Zcashd, 2).await;
    }
}

mod zebrad {
    use zaino_testutils::ValidatorKind;

    use crate::{launch_local_cache, launch_local_cache_process_n_block_batches};

    #[tokio::test]
    async fn launch_local_cache_zebrad() {
        launch_local_cache(&ValidatorKind::Zebrad).await;
    }

    #[tokio::test]
    async fn process_100_blocks() {
        launch_local_cache_process_n_block_batches(&ValidatorKind::Zebrad, 1).await;
    }

    #[tokio::test]
    async fn process_200_blocks() {
        launch_local_cache_process_n_block_batches(&ValidatorKind::Zebrad, 2).await;
    }
}
