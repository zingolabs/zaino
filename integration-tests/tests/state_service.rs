use zaino_state::BackendType;
use zaino_state::{
    FetchService, FetchServiceConfig, FetchServiceSubscriber, LightWalletIndexer, StateService,
    StateServiceConfig, StateServiceSubscriber, ZcashIndexer, ZcashService as _,
};
use zaino_testutils::from_inputs;
use zaino_testutils::services;
use zaino_testutils::Validator as _;
use zaino_testutils::{TestManager, ValidatorKind, ZEBRAD_TESTNET_CACHE_DIR};
use zebra_chain::{parameters::Network, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetInfo};

async fn create_test_manager_and_services(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
    network: Option<services::network::Network>,
) -> (
    TestManager,
    FetchService,
    FetchServiceSubscriber,
    StateService,
    StateServiceSubscriber,
) {
    let test_manager = TestManager::launch(
        validator,
        &BackendType::Fetch,
        network,
        chain_cache.clone(),
        enable_zaino,
        false,
        false,
        true,
        true,
        enable_clients,
    )
    .await
    .unwrap();

    let (network_type, zaino_sync_bool) = match network {
        Some(services::network::Network::Mainnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            (Network::Mainnet, false)
        }
        Some(services::network::Network::Testnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            (Network::new_default_testnet(), false)
        }
        _ => (
            Network::new_regtest(
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
            true,
        ),
    };

    test_manager.local_net.print_stdout();

    let fetch_service = FetchService::spawn(FetchServiceConfig::new(
        test_manager.zebrad_rpc_listen_address,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        test_manager
            .local_net
            .data_dir()
            .path()
            .to_path_buf()
            .join("zaino"),
        None,
        network_type.clone(),
        zaino_sync_bool,
        true,
    ))
    .await
    .unwrap();

    let fetch_subscriber = fetch_service.get_subscriber().inner();

    let state_chain_cache_dir = match chain_cache {
        Some(dir) => dir,
        None => test_manager.data_dir.clone(),
    };

    let state_service = StateService::spawn(StateServiceConfig::new(
        zebra_state::Config {
            cache_dir: state_chain_cache_dir,
            ephemeral: false,
            delete_old_database: true,
            debug_stop_at_height: None,
            debug_validity_check_interval: None,
        },
        test_manager.zebrad_rpc_listen_address,
        test_manager.zebrad_grpc_listen_address,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        test_manager
            .local_net
            .data_dir()
            .path()
            .to_path_buf()
            .join("zaino"),
        None,
        network_type,
        true,
        true,
    ))
    .await
    .unwrap();

    let state_subscriber = state_service.get_subscriber().inner();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    (
        test_manager,
        fetch_service,
        fetch_subscriber,
        state_service,
        state_subscriber,
    )
}

async fn state_service_check_info(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: services::network::Network,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, chain_cache, false, false, Some(network)).await;

    if dbg!(network.to_string()) == *"Regtest" {
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let fetch_service_info = dbg!(fetch_service_subscriber.get_info().await.unwrap());
    let fetch_service_blockchain_info = dbg!(fetch_service_subscriber
        .get_blockchain_info()
        .await
        .unwrap());

    let state_service_info = dbg!(state_service_subscriber.get_info().await.unwrap());
    let state_service_blockchain_info = dbg!(state_service_subscriber
        .get_blockchain_info()
        .await
        .unwrap());

    // Clean timestamp from get_info
    let (
        version,
        build,
        subversion,
        protocol_version,
        blocks,
        connections,
        proxy,
        difficulty,
        testnet,
        pay_tx_fee,
        relay_fee,
        errors,
        _,
    ) = fetch_service_info.into_parts();
    let cleaned_fetch_info = GetInfo::new(
        version,
        build,
        subversion,
        protocol_version,
        blocks,
        connections,
        proxy,
        difficulty,
        testnet,
        pay_tx_fee,
        relay_fee,
        errors,
        String::new(),
    );

    let (
        version,
        build,
        subversion,
        protocol_version,
        blocks,
        connections,
        proxy,
        difficulty,
        testnet,
        pay_tx_fee,
        relay_fee,
        errors,
        _,
    ) = state_service_info.into_parts();
    let cleaned_state_info = GetInfo::new(
        version,
        build,
        subversion,
        protocol_version,
        blocks,
        connections,
        proxy,
        difficulty,
        testnet,
        pay_tx_fee,
        relay_fee,
        errors,
        String::new(),
    );

    assert_eq!(cleaned_fetch_info, cleaned_state_info);

    assert_eq!(
        fetch_service_blockchain_info.chain(),
        state_service_blockchain_info.chain()
    );
    assert_eq!(
        fetch_service_blockchain_info.blocks(),
        state_service_blockchain_info.blocks()
    );
    assert_eq!(
        fetch_service_blockchain_info.best_block_hash(),
        state_service_blockchain_info.best_block_hash()
    );
    assert_eq!(
        fetch_service_blockchain_info.estimated_height(),
        state_service_blockchain_info.estimated_height()
    );
    // TODO: Fix this! (ignored due to [https://github.com/zingolabs/zaino/issues/235]).
    // assert_eq!(
    //     fetch_service_blockchain_info.value_pools(),
    //     state_service_blockchain_info.value_pools()
    // );
    assert_eq!(
        fetch_service_blockchain_info.upgrades(),
        state_service_blockchain_info.upgrades()
    );
    assert_eq!(
        fetch_service_blockchain_info.consensus(),
        state_service_blockchain_info.consensus()
    );

    test_manager.close().await;
}

async fn state_service_get_address_balance(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.recipient.sync_and_await().await.unwrap();
    let recipient_balance = clients.recipient.do_balance().await;

    let fetch_service_balance = fetch_service_subscriber
        .z_get_address_balance(AddressStrings::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();

    let state_service_balance = state_service_subscriber
        .z_get_address_balance(AddressStrings::new(vec![recipient_taddr]))
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&fetch_service_balance);
    dbg!(&state_service_balance);

    assert_eq!(
        recipient_balance.confirmed_transparent_balance.unwrap(),
        250_000,
    );
    assert_eq!(
        recipient_balance.confirmed_transparent_balance.unwrap(),
        fetch_service_balance.balance(),
    );
    assert_eq!(fetch_service_balance, state_service_balance);

    test_manager.close().await;
}

async fn state_service_get_address_balance_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request = AddressStrings::new(vec![address.to_string()]);

    let fetch_service_balance = dbg!(
        fetch_service_subscriber
            .z_get_address_balance(address_request.clone())
            .await
    )
    .unwrap();

    let state_service_balance = dbg!(
        state_service_subscriber
            .z_get_address_balance(address_request)
            .await
    )
    .unwrap();

    assert_eq!(fetch_service_balance, state_service_balance);

    test_manager.close().await;
}

async fn state_service_get_block_raw(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: services::network::Network,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, chain_cache, false, false, Some(network)).await;

    let height = match network {
        services::network::Network::Regtest => "1".to_string(),
        _ => "1000000".to_string(),
    };

    let fetch_service_block = dbg!(fetch_service_subscriber
        .z_get_block(height.clone(), Some(0))
        .await
        .unwrap());

    let state_service_block = dbg!(state_service_subscriber
        .z_get_block(height, Some(0))
        .await
        .unwrap());

    assert_eq!(fetch_service_block, state_service_block);

    test_manager.close().await;
}

async fn state_service_get_block_object(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: services::network::Network,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, chain_cache, false, false, Some(network)).await;

    let height = match network {
        services::network::Network::Regtest => "1".to_string(),
        _ => "1000000".to_string(),
    };

    let fetch_service_block = dbg!(fetch_service_subscriber
        .z_get_block(height.clone(), Some(1))
        .await
        .unwrap());

    let state_service_block = dbg!(state_service_subscriber
        .z_get_block(height, Some(1))
        .await
        .unwrap());

    assert_eq!(fetch_service_block, state_service_block);

    let hash = match fetch_service_block {
        zebra_rpc::methods::GetBlock::Raw(_) => panic!("expected object"),
        zebra_rpc::methods::GetBlock::Object(obj) => obj.hash().to_string(),
    };
    let state_service_get_block_by_hash = state_service_subscriber
        .z_get_block(hash.clone(), Some(1))
        .await
        .unwrap();
    assert_eq!(state_service_get_block_by_hash, state_service_block);

    test_manager.close().await;
}

async fn state_service_get_raw_mempool(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_taddr, 250_000, None)])
        .await
        .unwrap();
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let mut fetch_service_mempool = fetch_service_subscriber.get_raw_mempool().await.unwrap();
    let mut state_service_mempool = state_service_subscriber.get_raw_mempool().await.unwrap();

    dbg!(&fetch_service_mempool);
    fetch_service_mempool.sort();

    dbg!(&state_service_mempool);
    state_service_mempool.sort();

    assert_eq!(fetch_service_mempool, state_service_mempool);

    test_manager.close().await;
}

async fn state_service_get_raw_mempool_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let mut fetch_service_mempool = fetch_service_subscriber.get_raw_mempool().await.unwrap();
    let mut state_service_mempool = state_service_subscriber.get_raw_mempool().await.unwrap();

    dbg!(&fetch_service_mempool);
    fetch_service_mempool.sort();

    dbg!(&state_service_mempool);
    state_service_mempool.sort();

    assert_eq!(fetch_service_mempool, state_service_mempool);

    test_manager.close().await;
}

async fn state_service_z_get_treestate(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_service_treestate = dbg!(fetch_service_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    let state_service_treestate = dbg!(state_service_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    assert_eq!(fetch_service_treestate, state_service_treestate);

    test_manager.close().await;
}

async fn state_service_z_get_treestate_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let fetch_service_treestate = dbg!(
        fetch_service_subscriber
            .z_get_treestate("3000000".to_string())
            .await
    )
    .unwrap();

    let state_service_tx_treestate = dbg!(
        state_service_subscriber
            .z_get_treestate("3000000".to_string())
            .await
    )
    .unwrap();

    assert_eq!(fetch_service_treestate, state_service_tx_treestate);

    test_manager.close().await;
}

async fn state_service_z_get_subtrees_by_index(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_service_subtrees = dbg!(fetch_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    let state_service_subtrees = dbg!(state_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    assert_eq!(fetch_service_subtrees, state_service_subtrees);

    test_manager.close().await;
}

async fn state_service_z_get_subtrees_by_index_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let fetch_service_sapling_subtrees = dbg!(
        fetch_service_subscriber
            .z_get_subtrees_by_index("sapling".to_string(), 0.into(), None)
            .await
    )
    .unwrap();

    let state_service_sapling_subtrees = dbg!(
        state_service_subscriber
            .z_get_subtrees_by_index("sapling".to_string(), 0.into(), None)
            .await
    )
    .unwrap();

    assert_eq!(
        fetch_service_sapling_subtrees,
        state_service_sapling_subtrees
    );

    let fetch_service_orchard_subtrees = dbg!(
        fetch_service_subscriber
            .z_get_subtrees_by_index("orchard".to_string(), 0.into(), None)
            .await
    )
    .unwrap();

    let state_service_orchard_subtrees = dbg!(
        state_service_subscriber
            .z_get_subtrees_by_index("orchard".to_string(), 0.into(), None)
            .await
    )
    .unwrap();

    assert_eq!(
        fetch_service_orchard_subtrees,
        state_service_orchard_subtrees
    );

    test_manager.close().await;
}

async fn state_service_get_raw_transaction(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    let tx = from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    test_manager.local_net.print_stdout();

    let fetch_service_transaction = dbg!(fetch_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    let state_service_transaction = dbg!(state_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    assert_eq!(fetch_service_transaction, state_service_transaction);

    test_manager.close().await;
}

async fn state_service_get_raw_transaction_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let txid = "abb0399df392130baa45644c421fab553670a2d0d399c4dd776a8f7862ec289d".to_string();

    let fetch_service_transaction = dbg!(
        fetch_service_subscriber
            .get_raw_transaction(txid.clone(), None)
            .await
    )
    .unwrap();

    let state_service_tx_transaction = dbg!(
        state_service_subscriber
            .get_raw_transaction(txid, None)
            .await
    )
    .unwrap();

    assert_eq!(fetch_service_transaction, state_service_tx_transaction);

    test_manager.close().await;
}

async fn state_service_get_address_tx_ids(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let tx = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let chain_height = fetch_service_subscriber
        .block_cache
        .get_chain_height()
        .await
        .unwrap()
        .0;
    dbg!(&chain_height);

    let fetch_service_txids = fetch_service_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr.clone()],
            Some(chain_height - 2),
            Some(chain_height),
        ))
        .await
        .unwrap();

    let state_service_txids = state_service_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr],
            Some(chain_height - 2),
            Some(chain_height),
        ))
        .await
        .unwrap();

    dbg!(&tx);
    dbg!(&fetch_service_txids);
    assert_eq!(tx.first().to_string(), fetch_service_txids[0]);

    dbg!(&state_service_txids);
    assert_eq!(fetch_service_txids, state_service_txids);

    test_manager.close().await;
}

async fn state_service_get_address_tx_ids_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request =
        GetAddressTxIdsRequest::new(vec![address.to_string()], Some(2000000), Some(3000000));

    let fetch_service_tx_ids = dbg!(
        fetch_service_subscriber
            .get_address_tx_ids(address_request.clone())
            .await
    )
    .unwrap();

    let state_service_tx_ids = dbg!(
        state_service_subscriber
            .get_address_tx_ids(address_request)
            .await
    )
    .unwrap();

    assert_eq!(fetch_service_tx_ids, state_service_tx_ids);

    test_manager.close().await;
}

async fn state_service_get_address_utxos(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let txid_1 = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.sync_and_await().await.unwrap();

    let fetch_service_utxos = fetch_service_subscriber
        .z_get_address_utxos(AddressStrings::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    let state_service_utxos = state_service_subscriber
        .z_get_address_utxos(AddressStrings::new(vec![recipient_taddr]))
        .await
        .unwrap();
    let (_, state_service_txid, ..) = state_service_utxos[0].into_parts();

    dbg!(&txid_1);
    dbg!(&fetch_service_utxos);
    assert_eq!(txid_1.first().to_string(), fetch_service_txid.to_string());

    dbg!(&state_service_utxos);

    assert_eq!(
        fetch_service_txid.to_string(),
        state_service_txid.to_string()
    );

    test_manager.close().await;
}

async fn state_service_get_address_utxos_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(services::network::Network::Testnet),
    )
    .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request = AddressStrings::new(vec![address.to_string()]);

    let fetch_service_utxos = dbg!(
        fetch_service_subscriber
            .z_get_address_utxos(address_request.clone())
            .await
    )
    .unwrap();

    let state_service_tx_utxos = dbg!(
        state_service_subscriber
            .z_get_address_utxos(address_request)
            .await
    )
    .unwrap();

    assert_eq!(fetch_service_utxos, state_service_tx_utxos);

    test_manager.close().await;
}

mod zebrad {

    use super::*;

    pub(crate) mod check_info {

        use super::*;
        use zaino_testutils::ZEBRAD_CHAIN_CACHE_DIR;

        #[tokio::test]
        async fn regtest_no_cache() {
            state_service_check_info(
                &ValidatorKind::Zebrad,
                None,
                services::network::Network::Regtest,
            )
            .await;
        }

        #[tokio::test]
        async fn state_service_chaintip_update_subscriber() {
            let (
                test_manager,
                _fetch_service,
                _fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            let mut chaintip_subscriber = state_service_subscriber.chaintip_update_subscriber();
            for _ in 0..5 {
                test_manager.generate_blocks_with_delay(1).await;
                assert_eq!(
                    chaintip_subscriber
                        .next_tip_hash()
                        .await
                        .unwrap()
                        .bytes_in_display_order(),
                    <[u8; 32]>::try_from(
                        state_service_subscriber
                            .get_latest_block()
                            .await
                            .unwrap()
                            .hash
                    )
                    .unwrap()
                )
            }
        }

        #[tokio::test]
        #[ignore = "We no longer use chain caches. See zcashd::check_info::regtest_no_cache."]
        async fn regtest_with_cache() {
            state_service_check_info(
                &ValidatorKind::Zebrad,
                ZEBRAD_CHAIN_CACHE_DIR.clone(),
                services::network::Network::Regtest,
            )
            .await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn testnet() {
            state_service_check_info(
                &ValidatorKind::Zebrad,
                ZEBRAD_TESTNET_CACHE_DIR.clone(),
                services::network::Network::Testnet,
            )
            .await;
        }
    }

    pub(crate) mod get {

        use super::*;

        #[tokio::test]
        async fn address_utxos() {
            state_service_get_address_utxos(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn address_utxos_testnet() {
            state_service_get_address_utxos_testnet().await;
        }

        #[tokio::test]
        async fn address_tx_ids_regtest() {
            state_service_get_address_tx_ids(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn address_tx_ids_testnet() {
            state_service_get_address_tx_ids_testnet().await;
        }

        #[tokio::test]
        async fn raw_transaction_regtest() {
            state_service_get_raw_transaction(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn raw_transaction_testnet() {
            state_service_get_raw_transaction_testnet().await;
        }

        #[tokio::test]
        async fn best_blockhash() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(999)).await;

            let fetch_service_bbh =
                dbg!(fetch_service_subscriber.get_best_blockhash().await.unwrap());
            let state_service_bbh =
                dbg!(state_service_subscriber.get_best_blockhash().await.unwrap());
            assert_eq!(fetch_service_bbh, state_service_bbh);
        }

        #[tokio::test]
        async fn block_count() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let fetch_service_block_count =
                dbg!(fetch_service_subscriber.get_block_count().await.unwrap());
            let state_service_block_count =
                dbg!(state_service_subscriber.get_block_count().await.unwrap());
            assert_eq!(fetch_service_block_count, state_service_block_count);

            test_manager.close().await;
        }

        #[tokio::test]
        async fn difficulty() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;

            let initial_fetch_service_difficulty =
                fetch_service_subscriber.get_difficulty().await.unwrap();
            let initial_state_service_difficulty =
                state_service_subscriber.get_difficulty().await.unwrap();
            assert_eq!(
                initial_fetch_service_difficulty,
                initial_state_service_difficulty
            );

            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let final_fetch_service_difficulty =
                fetch_service_subscriber.get_difficulty().await.unwrap();
            let final_state_service_difficulty =
                state_service_subscriber.get_difficulty().await.unwrap();
            assert_eq!(
                final_fetch_service_difficulty,
                final_state_service_difficulty
            );

            test_manager.close().await;
        }

        mod z {
            use super::*;

            #[tokio::test]
            pub(crate) async fn subtrees_by_index_regtest() {
                state_service_z_get_subtrees_by_index(&ValidatorKind::Zebrad).await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test]
            pub(crate) async fn subtrees_by_index_testnet() {
                state_service_z_get_subtrees_by_index_testnet().await;
            }

            #[tokio::test]
            pub(crate) async fn treestate_regtest() {
                state_service_z_get_treestate(&ValidatorKind::Zebrad).await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test]
            pub(crate) async fn treestate_testnet() {
                state_service_z_get_treestate_testnet().await;
            }
        }

        #[tokio::test]
        async fn raw_mempool_regtest() {
            state_service_get_raw_mempool(&ValidatorKind::Zebrad).await;
        }

        /// `getmempoolinfo` computed from local Broadcast state
        #[tokio::test]
        async fn get_mempool_info() {
            let (
                mut test_manager,
                _fetch_service,
                _fetch_service_subscriber, // no longer used
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(&ValidatorKind::Zebrad, None, true, true, None)
                .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            let recipient_taddr = clients.get_recipient_address("transparent").await;

            clients.faucet.sync_and_await().await.unwrap();

            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.sync_and_await().await.unwrap();
            clients.faucet.quick_shield().await.unwrap();
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            clients.faucet.sync_and_await().await.unwrap();

            from_inputs::quick_send(
                &mut clients.faucet,
                vec![(recipient_taddr.as_str(), 250_000, None)],
            )
            .await
            .unwrap();

            // Let the broadcaster/subscribers observe the new tx
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            // Call the internal mempool info method
            let info = state_service_subscriber.get_mempool_info().await.unwrap();

            // Derive expected values directly from the current mempool contents
            let entries = state_service_subscriber.mempool.get_mempool().await;

            assert_eq!(entries.len() as u64, info.size);
            assert!(info.size >= 1);

            let expected_bytes: u64 = entries
                .iter()
                .map(|(_, v)| v.0.as_ref().as_ref().len() as u64)
                .sum();

            let expected_key_heap_bytes: u64 =
                entries.iter().map(|(k, _)| k.0.capacity() as u64).sum();

            let expected_usage = expected_bytes.saturating_add(expected_key_heap_bytes);

            assert!(info.bytes > 0);
            assert_eq!(info.bytes, expected_bytes);

            assert!(info.usage >= info.bytes);
            assert_eq!(info.usage, expected_usage);

            // Optional: when exactly one tx, its serialized length must equal `bytes`
            if info.size == 1 {
                assert_eq!(
                    entries[0].1 .0.as_ref().as_ref().len() as u64,
                    expected_bytes
                );
            }

            test_manager.close().await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn raw_mempool_testnet() {
            state_service_get_raw_mempool_testnet().await;
        }

        #[tokio::test]
        async fn block_object_regtest() {
            state_service_get_block_object(
                &ValidatorKind::Zebrad,
                None,
                services::network::Network::Regtest,
            )
            .await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn block_object_testnet() {
            state_service_get_block_object(
                &ValidatorKind::Zebrad,
                ZEBRAD_TESTNET_CACHE_DIR.clone(),
                services::network::Network::Testnet,
            )
            .await;
        }

        #[tokio::test]
        async fn block_raw_regtest() {
            state_service_get_block_raw(
                &ValidatorKind::Zebrad,
                None,
                services::network::Network::Regtest,
            )
            .await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn block_raw_testnet() {
            state_service_get_block_raw(
                &ValidatorKind::Zebrad,
                ZEBRAD_TESTNET_CACHE_DIR.clone(),
                services::network::Network::Testnet,
            )
            .await;
        }

        #[tokio::test]
        async fn address_balance_regtest() {
            state_service_get_address_balance(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn address_balance_testnet() {
            state_service_get_address_balance_testnet().await;
        }
    }

    pub(crate) mod lightwallet_indexer {
        use futures::StreamExt as _;
        use zaino_proto::proto::service::{
            AddressList, BlockId, BlockRange, GetAddressUtxosArg, GetSubtreeRootsArg, TxFilter,
        };
        use zebra_rpc::methods::GetAddressTxIdsRequest;

        use super::*;
        #[tokio::test]
        async fn get_latest_block() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(1).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let fetch_service_block =
                dbg!(fetch_service_subscriber.get_latest_block().await.unwrap());
            let state_service_block =
                dbg!(state_service_subscriber.get_latest_block().await.unwrap());
            assert_eq!(fetch_service_block, state_service_block);
        }

        #[tokio::test]
        async fn get_block() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let second_block_by_height = BlockId {
                height: 2,
                hash: vec![],
            };
            let fetch_service_block_by_height = fetch_service_subscriber
                .get_block(second_block_by_height.clone())
                .await
                .unwrap();
            let state_service_block_by_height = dbg!(state_service_subscriber
                .get_block(second_block_by_height)
                .await
                .unwrap());
            assert_eq!(fetch_service_block_by_height, state_service_block_by_height);

            let hash = fetch_service_block_by_height.hash;
            let second_block_by_hash = BlockId { height: 0, hash };
            let fetch_service_block_by_hash = dbg!(fetch_service_subscriber
                .get_block(second_block_by_hash.clone())
                .await
                .unwrap());
            let state_service_block_by_hash = dbg!(state_service_subscriber
                .get_block(second_block_by_hash)
                .await
                .unwrap());
            assert_eq!(fetch_service_block_by_hash, state_service_block_by_hash);
            assert_eq!(state_service_block_by_hash, state_service_block_by_height)
        }
        #[tokio::test]
        async fn get_tree_state() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let second_treestate_by_height = BlockId {
                height: 2,
                hash: vec![],
            };
            let fetch_service_treestate_by_height = dbg!(fetch_service_subscriber
                .get_tree_state(second_treestate_by_height.clone())
                .await
                .unwrap());
            let state_service_treestate_by_height = dbg!(state_service_subscriber
                .get_tree_state(second_treestate_by_height)
                .await
                .unwrap());
            assert_eq!(
                fetch_service_treestate_by_height,
                state_service_treestate_by_height
            );
        }
        #[tokio::test]
        async fn get_subtree_roots() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(5).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let sapling_subtree_roots_request = GetSubtreeRootsArg {
                start_index: 2,
                shielded_protocol: 0,
                max_entries: 0,
            };
            let fetch_service_sapling_subtree_roots = fetch_service_subscriber
                .get_subtree_roots(sapling_subtree_roots_request.clone())
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .await;
            let state_service_sapling_subtree_roots = state_service_subscriber
                .get_subtree_roots(sapling_subtree_roots_request)
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                fetch_service_sapling_subtree_roots,
                state_service_sapling_subtree_roots
            );
        }
        #[tokio::test]
        async fn get_latest_tree_state() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let fetch_service_treestate = fetch_service_subscriber
                .get_latest_tree_state()
                .await
                .unwrap();
            let state_service_treestate = dbg!(state_service_subscriber
                .get_latest_tree_state()
                .await
                .unwrap());
            assert_eq!(fetch_service_treestate, state_service_treestate);
        }

        async fn get_block_range_helper(nullifiers_only: bool) {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(6).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let start = Some(BlockId {
                height: 2,
                hash: vec![],
            });
            let end = Some(BlockId {
                height: 5,
                hash: vec![],
            });
            let request = BlockRange { start, end };
            if nullifiers_only {
                let fetch_service_get_block_range = fetch_service_subscriber
                    .get_block_range_nullifiers(request.clone())
                    .await
                    .unwrap()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>()
                    .await;
                let state_service_get_block_range = state_service_subscriber
                    .get_block_range_nullifiers(request)
                    .await
                    .unwrap()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>()
                    .await;
                assert_eq!(fetch_service_get_block_range, state_service_get_block_range);
            } else {
                let fetch_service_get_block_range = fetch_service_subscriber
                    .get_block_range(request.clone())
                    .await
                    .unwrap()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>()
                    .await;
                let state_service_get_block_range = state_service_subscriber
                    .get_block_range(request)
                    .await
                    .unwrap()
                    .map(Result::unwrap)
                    .collect::<Vec<_>>()
                    .await;
                assert_eq!(fetch_service_get_block_range, state_service_get_block_range);
            }
        }

        #[tokio::test]
        async fn get_block_range_full() {
            get_block_range_helper(false).await;
        }
        #[tokio::test]
        async fn get_block_range_nullifiers() {
            get_block_range_helper(true).await;
        }
        #[tokio::test]
        async fn get_transaction() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(services::network::Network::Regtest),
            )
            .await;
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            clients.faucet.sync_and_await().await.unwrap();
            clients.faucet.quick_shield().await.unwrap();

            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let block = BlockId {
                height: 102,
                hash: vec![],
            };
            let state_service_block_by_height = state_service_subscriber
                .get_block(block.clone())
                .await
                .unwrap();
            let coinbase_tx = state_service_block_by_height.vtx.first().unwrap();
            let hash = coinbase_tx.hash.clone();
            let request = TxFilter {
                block: None,
                index: 0,
                hash,
            };
            let fetch_service_raw_transaction = fetch_service_subscriber
                .get_transaction(request.clone())
                .await
                .unwrap();
            let state_service_raw_transaction = state_service_subscriber
                .get_transaction(request)
                .await
                .unwrap();
            assert_eq!(fetch_service_raw_transaction, state_service_raw_transaction);
        }

        #[tokio::test]
        async fn get_taddress_txids() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(services::network::Network::Regtest),
            )
            .await;

            let clients = test_manager.clients.take().unwrap();
            let taddr = clients.get_faucet_address("transparent").await;
            test_manager.local_net.generate_blocks(100).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let state_service_taddress_txids = state_service_subscriber
                .get_address_tx_ids(GetAddressTxIdsRequest::new(
                    vec![taddr.clone()],
                    Some(2),
                    Some(5),
                ))
                .await
                .unwrap();
            dbg!(&state_service_taddress_txids);
            let fetch_service_taddress_txids = fetch_service_subscriber
                .get_address_tx_ids(GetAddressTxIdsRequest::new(vec![taddr], Some(2), Some(5)))
                .await
                .unwrap();
            dbg!(&fetch_service_taddress_txids);
            assert_eq!(fetch_service_taddress_txids, state_service_taddress_txids);
        }

        #[tokio::test]
        async fn get_address_utxos_stream() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(services::network::Network::Regtest),
            )
            .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            let taddr = clients.get_faucet_address("transparent").await;
            test_manager.local_net.generate_blocks(5).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let request = GetAddressUtxosArg {
                addresses: vec![taddr],
                start_height: 2,
                max_entries: 3,
            };
            let state_service_address_utxos_streamed = state_service_subscriber
                .get_address_utxos_stream(request.clone())
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .await;
            let fetch_service_address_utxos_streamed = fetch_service_subscriber
                .get_address_utxos_stream(request)
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .await;
            assert_eq!(
                fetch_service_address_utxos_streamed,
                state_service_address_utxos_streamed
            );
            clients.faucet.sync_and_await().await.unwrap();
            assert_eq!(
                fetch_service_address_utxos_streamed.first().unwrap().txid,
                clients
                    .faucet
                    .transaction_summaries()
                    .await
                    .unwrap()
                    .txids()[1]
                    .as_ref()
            );
        }
        #[tokio::test]
        async fn get_address_utxos() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(services::network::Network::Regtest),
            )
            .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            let taddr = clients.get_faucet_address("transparent").await;
            test_manager.local_net.generate_blocks(5).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let request = GetAddressUtxosArg {
                addresses: vec![taddr],
                start_height: 2,
                max_entries: 3,
            };
            let state_service_address_utxos = state_service_subscriber
                .get_address_utxos(request.clone())
                .await
                .unwrap();
            let fetch_service_address_utxos = fetch_service_subscriber
                .get_address_utxos(request)
                .await
                .unwrap();
            assert_eq!(fetch_service_address_utxos, state_service_address_utxos);
            clients.faucet.sync_and_await().await.unwrap();
            assert_eq!(
                fetch_service_address_utxos
                    .address_utxos
                    .first()
                    .unwrap()
                    .txid,
                clients
                    .faucet
                    .transaction_summaries()
                    .await
                    .unwrap()
                    .txids()[1]
                    .as_ref()
            );
        }
        #[tokio::test]
        async fn get_taddress_balance() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(services::network::Network::Regtest),
            )
            .await;

            let clients = test_manager.clients.take().unwrap();
            let taddr = clients.get_faucet_address("transparent").await;
            test_manager.local_net.generate_blocks(5).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let state_service_taddress_balance = state_service_subscriber
                .get_taddress_balance(AddressList {
                    addresses: vec![taddr.clone()],
                })
                .await
                .unwrap();
            let fetch_service_taddress_balance = fetch_service_subscriber
                .get_taddress_balance(AddressList {
                    addresses: vec![taddr],
                })
                .await
                .unwrap();
            assert_eq!(
                fetch_service_taddress_balance,
                state_service_taddress_balance
            );
        }
    }
}
