use zaino_state::{
    config::{FetchServiceConfig, StateServiceConfig},
    fetch::{FetchService, FetchServiceSubscriber},
    indexer::{ZcashIndexer, ZcashService as _},
    state::StateService,
};
use zaino_testutils::{TestManager, ZEBRAD_CHAIN_CACHE_DIR, ZEBRAD_TESTNET_CACHE_DIR};
use zebra_chain::{parameters::Network, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetInfo};
use zingo_infra_testutils::services::{self, validator::Validator as _};

async fn create_test_manager_and_services(
    validator: &str,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
    network: Option<services::network::Network>,
) -> (
    TestManager,
    FetchService,
    FetchServiceSubscriber,
    StateService,
) {
    let test_manager = TestManager::launch(
        validator,
        network,
        chain_cache.clone(),
        enable_zaino,
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
        _ => (Network::new_regtest(Some(1), Some(1)), true),
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

    let subscriber = fetch_service.get_subscriber().inner();

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
        false,
        None,
        None,
        None,
        None,
        None,
        network_type,
    ))
    .await
    .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    (test_manager, fetch_service, subscriber, state_service)
}

#[tokio::test]
async fn state_service_check_info_regtest_no_cache_zebrad() {
    state_service_check_info("zebrad", None, services::network::Network::Regtest).await;
}

#[tokio::test]
async fn state_service_check_info_regtest_with_cache_zebrad() {
    state_service_check_info(
        "zebrad",
        ZEBRAD_CHAIN_CACHE_DIR.clone(),
        services::network::Network::Regtest,
    )
    .await;
}

#[tokio::test]
async fn state_service_check_info_testnet_zebrad() {
    state_service_check_info(
        "zebrad",
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        services::network::Network::Testnet,
    )
    .await;
}

async fn state_service_check_info(
    validator: &str,
    chain_cache: Option<std::path::PathBuf>,
    network: services::network::Network,
) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, chain_cache, false, false, Some(network)).await;

    if dbg!(network.to_string()) == "Regtest".to_string() {
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let fetch_service_info = dbg!(fetch_service_subscriber.get_info().await.unwrap());
    let fetch_service_blockchain_info = dbg!(fetch_service_subscriber
        .get_blockchain_info()
        .await
        .unwrap());

    let state_service_info = dbg!(state_service.get_info().await.unwrap());
    let state_service_blockchain_info = dbg!(state_service.get_blockchain_info().await.unwrap());

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
    let cleaned_fetch_info = GetInfo::from_parts(
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
    let cleaned_state_info = GetInfo::from_parts(
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

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_get_address_balance_regtest_zebrad() {
    state_service_get_address_balance("zebrad").await;
}

#[tokio::test]
async fn state_service_get_address_balance_testnet_zebrad() {
    state_service_get_address_balance_testnet().await;
}

async fn state_service_get_address_balance(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;

    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");
    let recipient_address = clients.get_recipient_address("transparent").await;

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(recipient_address.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.recipient.do_sync(true).await.unwrap();
    let recipient_balance = clients.recipient.do_balance().await;

    let fetch_service_balance = fetch_service_subscriber
        .z_get_address_balance(AddressStrings::new_valid(vec![recipient_address.clone()]).unwrap())
        .await
        .unwrap();

    let state_service_balance = state_service
        .z_get_address_balance(AddressStrings::new_valid(vec![recipient_address]).unwrap())
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&fetch_service_balance);
    dbg!(&state_service_balance);

    assert_eq!(recipient_balance.transparent_balance.unwrap(), 250_000,);
    assert_eq!(
        recipient_balance.transparent_balance.unwrap(),
        fetch_service_balance.balance,
    );
    assert_eq!(fetch_service_balance, state_service_balance);

    test_manager.close().await;
}

async fn state_service_get_address_balance_testnet() {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
            ZEBRAD_TESTNET_CACHE_DIR.clone(),
            false,
            false,
            Some(services::network::Network::Testnet),
        )
        .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request = AddressStrings::new_valid(vec![address.to_string()]).unwrap();

    let fetch_service_balance = dbg!(
        fetch_service_subscriber
            .z_get_address_balance(address_request.clone())
            .await
    )
    .unwrap();

    let state_service_balance =
        dbg!(state_service.z_get_address_balance(address_request).await).unwrap();

    assert_eq!(fetch_service_balance, state_service_balance);

    test_manager.close().await;
}

#[tokio::test]
async fn state_service_get_block_raw_regtest_zebrad() {
    state_service_get_block_raw("zebrad", None, services::network::Network::Regtest).await;
}

#[tokio::test]
async fn state_service_get_block_raw_testnet_zebrad() {
    state_service_get_block_raw(
        "zebrad",
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        services::network::Network::Testnet,
    )
    .await;
}

async fn state_service_get_block_raw(
    validator: &str,
    chain_cache: Option<std::path::PathBuf>,
    network: services::network::Network,
) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, chain_cache, false, false, Some(network)).await;

    let height = match network {
        services::network::Network::Regtest => "1".to_string(),
        _ => "1000000".to_string(),
    };

    let fetch_service_block = dbg!(fetch_service_subscriber
        .z_get_block(height.clone(), Some(0))
        .await
        .unwrap());

    let state_service_block = dbg!(state_service.z_get_block(height, Some(0)).await.unwrap());

    assert_eq!(fetch_service_block, state_service_block);

    test_manager.close().await;
}

#[tokio::test]
async fn state_service_get_block_object_regtest_zebrad() {
    state_service_get_block_object("zebrad", None, services::network::Network::Regtest).await;
}

#[tokio::test]
async fn state_service_get_block_object_testnet_zebrad() {
    state_service_get_block_object(
        "zebrad",
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        services::network::Network::Testnet,
    )
    .await;
}

async fn state_service_get_block_object(
    validator: &str,
    chain_cache: Option<std::path::PathBuf>,
    network: services::network::Network,
) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, chain_cache, false, false, Some(network)).await;

    let height = match network {
        services::network::Network::Regtest => "1".to_string(),
        _ => "1000000".to_string(),
    };

    let fetch_service_block = dbg!(fetch_service_subscriber
        .z_get_block(height.clone(), Some(1))
        .await
        .unwrap());

    let state_service_block = dbg!(state_service.z_get_block(height, Some(1)).await.unwrap());

    assert_eq!(fetch_service_block, state_service_block);

    test_manager.close().await;
}

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_get_raw_mempool_regtest_zebrad() {
    state_service_get_raw_mempool("zebrad").await;
}

#[tokio::test]
async fn state_service_get_raw_mempool_testnet_zebrad() {
    state_service_get_raw_mempool_testnet().await;
}

async fn state_service_get_raw_mempool(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;
    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(
            &clients.get_recipient_address("transparent").await,
            250_000,
            None,
        )],
    )
    .await
    .unwrap();
    zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(
            &clients.get_recipient_address("unified").await,
            250_000,
            None,
        )],
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let mut fetch_service_mempool = fetch_service_subscriber.get_raw_mempool().await.unwrap();
    let mut state_service_mempool = state_service.get_raw_mempool().await.unwrap();

    dbg!(&fetch_service_mempool);
    fetch_service_mempool.sort();

    dbg!(&state_service_mempool);
    state_service_mempool.sort();

    assert_eq!(fetch_service_mempool, state_service_mempool);

    test_manager.close().await;
}

async fn state_service_get_raw_mempool_testnet() {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
            ZEBRAD_TESTNET_CACHE_DIR.clone(),
            false,
            false,
            Some(services::network::Network::Testnet),
        )
        .await;

    let mut fetch_service_mempool = fetch_service_subscriber.get_raw_mempool().await.unwrap();
    let mut state_service_mempool = state_service.get_raw_mempool().await.unwrap();

    dbg!(&fetch_service_mempool);
    fetch_service_mempool.sort();

    dbg!(&state_service_mempool);
    state_service_mempool.sort();

    assert_eq!(fetch_service_mempool, state_service_mempool);

    test_manager.close().await;
}

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_z_get_treestate_regtest_zebrad() {
    state_service_z_get_treestate("zebrad").await;
}

#[tokio::test]
async fn state_service_z_get_treestate_testnet_zebrad() {
    state_service_z_get_treestate_testnet().await;
}

async fn state_service_z_get_treestate(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;

    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(
            &clients.get_recipient_address("unified").await,
            250_000,
            None,
        )],
    )
    .await
    .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_service_treestate = dbg!(fetch_service_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    let state_service_treestate = dbg!(state_service
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    assert_eq!(fetch_service_treestate, state_service_treestate);

    test_manager.close().await;
}

async fn state_service_z_get_treestate_testnet() {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
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

    let state_service_tx_treestate =
        dbg!(state_service.z_get_treestate("3000000".to_string()).await).unwrap();

    assert_eq!(fetch_service_treestate, state_service_tx_treestate);

    test_manager.close().await;
}

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_z_get_subtrees_by_index_regtest_zebrad() {
    state_service_z_get_subtrees_by_index("zebrad").await;
}

#[tokio::test]
async fn state_service_z_get_subtrees_by_index_testnet_zebrad() {
    state_service_z_get_subtrees_by_index_testnet().await;
}

async fn state_service_z_get_subtrees_by_index(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;

    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(
            &clients.get_recipient_address("unified").await,
            250_000,
            None,
        )],
    )
    .await
    .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_service_subtrees = dbg!(fetch_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    let state_service_subtrees = dbg!(state_service
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    assert_eq!(fetch_service_subtrees, state_service_subtrees);

    test_manager.close().await;
}

async fn state_service_z_get_subtrees_by_index_testnet() {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
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
        state_service
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
        state_service
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

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_get_raw_transaction_regtest_zebrad() {
    state_service_get_raw_transaction("zebrad").await;
}

#[tokio::test]
async fn state_service_get_raw_transaction_testnet_zebrad() {
    state_service_get_raw_transaction_testnet().await;
}

async fn state_service_get_raw_transaction(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;

    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(
            &clients.get_recipient_address("unified").await,
            250_000,
            None,
        )],
    )
    .await
    .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    test_manager.local_net.print_stdout();

    let fetch_service_transaction = dbg!(fetch_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    let state_service_transaction = dbg!(state_service
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    assert_eq!(fetch_service_transaction, state_service_transaction);

    test_manager.close().await;
}

async fn state_service_get_raw_transaction_testnet() {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
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

    let state_service_tx_transaction =
        dbg!(state_service.get_raw_transaction(txid, None).await).unwrap();

    assert_eq!(fetch_service_transaction, state_service_tx_transaction);

    test_manager.close().await;
}

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_get_address_tx_ids_regtest_zebrad() {
    state_service_get_address_tx_ids("zebrad").await;
}

#[tokio::test]
async fn state_service_get_address_tx_ids_testnet_zebrad() {
    state_service_get_address_tx_ids_testnet().await;
}

async fn state_service_get_address_tx_ids(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;

    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");
    let recipient_address = clients.get_recipient_address("transparent").await;

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    let tx = zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(recipient_address.as_str(), 250_000, None)],
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
        .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
            vec![recipient_address.clone()],
            chain_height - 2,
            chain_height,
        ))
        .await
        .unwrap();

    let state_service_txids = state_service
        .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
            vec![recipient_address],
            chain_height - 2,
            chain_height,
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
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
            ZEBRAD_TESTNET_CACHE_DIR.clone(),
            false,
            false,
            Some(services::network::Network::Testnet),
        )
        .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request =
        GetAddressTxIdsRequest::from_parts(vec![address.to_string()], 2000000, 3000000);

    let fetch_service_tx_ids = dbg!(
        fetch_service_subscriber
            .get_address_tx_ids(address_request.clone())
            .await
    )
    .unwrap();

    let state_service_tx_ids =
        dbg!(state_service.get_address_tx_ids(address_request).await).unwrap();

    assert_eq!(fetch_service_tx_ids, state_service_tx_ids);

    test_manager.close().await;
}

#[ignore = "currently fails due to error in TrustedChainSync [https://github.com/zingolabs/zaino/issues/231]."]
#[tokio::test]
async fn state_service_get_address_utxos_zebrad() {
    state_service_get_address_utxos("zebrad").await;
}

#[tokio::test]
async fn state_service_get_address_utxos_testnet_zebrad() {
    state_service_get_address_utxos_testnet().await;
}

async fn state_service_get_address_utxos(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(validator, None, true, true, None).await;

    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");
    let recipient_address = clients.get_recipient_address("transparent").await;

    clients.faucet.do_sync(true).await.unwrap();

    if validator == "zebrad" {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.do_sync(true).await.unwrap();
    };

    let txid_1 = zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(recipient_address.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.do_sync(true).await.unwrap();

    let fetch_service_utxos = fetch_service_subscriber
        .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_address.clone()]).unwrap())
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    let state_service_utxos = state_service
        .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_address]).unwrap())
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
    let (mut test_manager, _fetch_service, fetch_service_subscriber, state_service) =
        create_test_manager_and_services(
            "zebrad",
            ZEBRAD_TESTNET_CACHE_DIR.clone(),
            false,
            false,
            Some(services::network::Network::Testnet),
        )
        .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request = AddressStrings::new_valid(vec![address.to_string()]).unwrap();

    let fetch_service_utxos = dbg!(
        fetch_service_subscriber
            .z_get_address_utxos(address_request.clone())
            .await
    )
    .unwrap();

    let state_service_tx_utxos =
        dbg!(state_service.z_get_address_utxos(address_request).await).unwrap();

    assert_eq!(fetch_service_utxos, state_service_tx_utxos);

    test_manager.close().await;
}
