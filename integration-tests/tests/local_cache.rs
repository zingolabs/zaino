use core::panic;
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_state::{
    bench::{BlockCache, BlockCacheConfig, BlockCacheSubscriber},
    BackendType,
};
use zaino_testutils::Validator as _;
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::block::Height;
use zebra_state::HashOrHeight;

async fn create_test_manager_and_block_cache(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (
    TestManager,
    JsonRpSeeConnector,
    BlockCache,
    BlockCacheSubscriber,
) {
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
        test_node_and_return_url(
            test_manager.zebrad_rpc_listen_address,
            false,
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

    let block_cache_config = BlockCacheConfig {
        map_capacity: None,
        map_shard_amount: None,
        db_path: test_manager.data_dir.clone(),
        db_size: None,
        network,
        no_sync: zaino_no_sync,
        no_db: zaino_no_db,
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

async fn launch_local_cache(validator: &ValidatorKind, no_db: bool) {
    let (_test_manager, _json_service, _block_cache, block_cache_subscriber) =
        create_test_manager_and_block_cache(validator, None, false, true, no_db, false).await;

    dbg!(block_cache_subscriber.status());
}

/// Launches a testmanager and block cache and generates `n*100` blocks, checking blocks are stored and fetched correctly.
async fn launch_local_cache_process_n_block_batches(validator: &ValidatorKind, batches: u32) {
    let (test_manager, json_service, mut block_cache, mut block_cache_subscriber) =
        create_test_manager_and_block_cache(validator, None, false, true, false, false).await;

    let finalised_state = block_cache.finalised_state.take().unwrap();
    let finalised_state_subscriber = block_cache_subscriber.finalised_state.take().unwrap();

    for _ in 1..=batches {
        // Generate blocks
        //
        // NOTE: Generating blocks with zcashd blocks the tokio main thread???, stopping background processes from running,
        //       for this reason we generate blocks 1 at a time and sleep to let other tasks run.
        for height in 1..=100 {
            println!("Generating block at height: {}", height);
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

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
    async fn launch_no_db() {
        launch_local_cache(&ValidatorKind::Zcashd, true).await;
    }

    #[tokio::test]
    async fn launch_with_db() {
        launch_local_cache(&ValidatorKind::Zcashd, false).await;
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
    async fn launch_no_db() {
        launch_local_cache(&ValidatorKind::Zebrad, true).await;
    }

    #[tokio::test]
    async fn launch_with_db() {
        launch_local_cache(&ValidatorKind::Zebrad, false).await;
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
