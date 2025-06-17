use zaino_state::{
    BackendType, FetchService, FetchServiceConfig, FetchServiceSubscriber, ZcashIndexer,
    ZcashService as _,
};
use zaino_testutils::{from_inputs, Validator as _};
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::{parameters::Network, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetInfo};

async fn create_test_manager_and_fetch_services(
    enable_cookie_auth: bool,
    clients: bool,
) -> (
    TestManager,
    FetchService,
    FetchServiceSubscriber,
    FetchService,
    FetchServiceSubscriber,
) {
    println!("Launching test manager..");
    let test_manager = TestManager::launch(
        &ValidatorKind::Zcashd,
        &BackendType::Fetch,
        None,
        None,
        true,
        true,
        enable_cookie_auth,
        true,
        true,
        clients,
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Launching zcashd fetch service..");
    let zcashd_fetch_service = FetchService::spawn(FetchServiceConfig::new(
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
        true,
    ))
    .await
    .unwrap();
    let zcashd_subscriber = zcashd_fetch_service.get_subscriber().inner();

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Launching zaino fetch service..");
    let zaino_json_server_address = dbg!(test_manager.zaino_json_rpc_listen_address.unwrap());
    let zaino_fetch_service = FetchService::spawn(FetchServiceConfig::new(
        zaino_json_server_address,
        enable_cookie_auth,
        test_manager
            .json_server_cookie_dir
            .clone()
            .map(|p| p.to_string_lossy().into_owned()),
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
        true,
    ))
    .await
    .unwrap();
    let zaino_subscriber = zaino_fetch_service.get_subscriber().inner();

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Testmanager launch complete!");
    (
        test_manager,
        zcashd_fetch_service,
        zcashd_subscriber,
        zaino_fetch_service,
        zaino_subscriber,
    )
}

async fn launch_json_server_check_info(enable_cookie_auth: bool) {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(enable_cookie_auth, false).await;

    let zcashd_info = dbg!(zcashd_subscriber.get_info().await.unwrap());
    let zcashd_blockchain_info = dbg!(zcashd_subscriber.get_blockchain_info().await.unwrap());
    let zaino_info = dbg!(zaino_subscriber.get_info().await.unwrap());
    let zaino_blockchain_info = dbg!(zaino_subscriber.get_blockchain_info().await.unwrap());

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
    ) = zcashd_info.into_parts();
    let cleaned_zcashd_info = GetInfo::from_parts(
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
    ) = zaino_info.into_parts();
    let cleaned_zaino_info = GetInfo::from_parts(
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

    assert_eq!(cleaned_zcashd_info, cleaned_zaino_info);

    assert_eq!(
        zcashd_blockchain_info.chain(),
        zaino_blockchain_info.chain()
    );
    assert_eq!(
        zcashd_blockchain_info.blocks(),
        zaino_blockchain_info.blocks()
    );
    assert_eq!(
        zcashd_blockchain_info.best_block_hash(),
        zaino_blockchain_info.best_block_hash()
    );
    assert_eq!(
        zcashd_blockchain_info.estimated_height(),
        zaino_blockchain_info.estimated_height()
    );
    assert_eq!(
        zcashd_blockchain_info.value_pools(),
        zaino_blockchain_info.value_pools()
    );
    assert_eq!(
        zcashd_blockchain_info.upgrades(),
        zaino_blockchain_info.upgrades()
    );
    assert_eq!(
        zcashd_blockchain_info.consensus(),
        zaino_blockchain_info.consensus()
    );

    test_manager.close().await;
}

async fn get_block_count_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, false).await;

    let zcashd_block_count = dbg!(zcashd_subscriber.get_block_count().await.unwrap());
    let zaino_block_count = dbg!(zaino_subscriber.get_block_count().await.unwrap());

    assert_eq!(zcashd_block_count, zaino_block_count);

    test_manager.close().await;
}

async fn z_get_address_balance_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

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

    let zcashd_service_balance = zcashd_subscriber
        .z_get_address_balance(AddressStrings::new_valid(vec![recipient_taddr.clone()]).unwrap())
        .await
        .unwrap();

    let zaino_service_balance = zaino_subscriber
        .z_get_address_balance(AddressStrings::new_valid(vec![recipient_taddr]).unwrap())
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&zcashd_service_balance);
    dbg!(&zaino_service_balance);

    assert_eq!(
        recipient_balance.confirmed_transparent_balance.unwrap(),
        250_000,
    );
    assert_eq!(
        recipient_balance.confirmed_transparent_balance.unwrap(),
        zcashd_service_balance.balance,
    );
    assert_eq!(zcashd_service_balance, zaino_service_balance);

    test_manager.close().await;
}

async fn z_get_block_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, false).await;

    let zcashd_block_raw = dbg!(zcashd_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    let zaino_block_raw = dbg!(zaino_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    assert_eq!(zcashd_block_raw, zaino_block_raw);

    let zcashd_block = dbg!(zcashd_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    let zaino_block = dbg!(zaino_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    assert_eq!(zcashd_block, zaino_block);

    let hash = match zcashd_block {
        zebra_rpc::methods::GetBlock::Raw(_) => panic!("expected object"),
        zebra_rpc::methods::GetBlock::Object { hash, .. } => hash.0.to_string(),
    };
    let zaino_get_block_by_hash = zaino_subscriber
        .z_get_block(hash.clone(), Some(1))
        .await
        .unwrap();
    assert_eq!(zaino_get_block_by_hash, zaino_block);

    test_manager.close().await;
}

async fn get_raw_mempool_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    let recipient_taddr = &clients.get_recipient_address("transparent").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(recipient_taddr, 250_000, None)])
        .await
        .unwrap();
    from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let mut zcashd_mempool = zcashd_subscriber.get_raw_mempool().await.unwrap();
    let mut zaino_mempool = zaino_subscriber.get_raw_mempool().await.unwrap();

    dbg!(&zcashd_mempool);
    zcashd_mempool.sort();

    dbg!(&zaino_mempool);
    zaino_mempool.sort();

    assert_eq!(zcashd_mempool, zaino_mempool);

    test_manager.close().await;
}

async fn z_get_treestate_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let zcashd_treestate = dbg!(zcashd_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    let zaino_treestate = dbg!(zaino_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    assert_eq!(zcashd_treestate, zaino_treestate);

    test_manager.close().await;
}

async fn z_get_subtrees_by_index_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let zcashd_subtrees = dbg!(zcashd_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    let zaino_subtrees = dbg!(zaino_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    assert_eq!(zcashd_subtrees, zaino_subtrees);

    test_manager.close().await;
}

async fn get_raw_transaction_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    let tx = from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    test_manager.local_net.print_stdout();

    let zcashd_transaction = dbg!(zcashd_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    let zaino_transaction = dbg!(zaino_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    assert_eq!(zcashd_transaction, zaino_transaction);

    test_manager.close().await;
}

async fn get_address_tx_ids_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    let tx = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let chain_height = zcashd_subscriber
        .block_cache
        .get_chain_height()
        .await
        .unwrap()
        .0;
    dbg!(&chain_height);

    let zcashd_txids = zcashd_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
            vec![recipient_taddr.clone()],
            chain_height - 2,
            chain_height,
        ))
        .await
        .unwrap();

    let zaino_txids = zaino_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::from_parts(
            vec![recipient_taddr],
            chain_height - 2,
            chain_height,
        ))
        .await
        .unwrap();

    dbg!(&tx);
    dbg!(&zcashd_txids);
    assert_eq!(tx.first().to_string(), zcashd_txids[0]);

    dbg!(&zaino_txids);
    assert_eq!(zcashd_txids, zaino_txids);

    test_manager.close().await;
}

async fn z_get_address_utxos_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    let txid_1 = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.sync_and_await().await.unwrap();

    let zcashd_utxos = zcashd_subscriber
        .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_taddr.clone()]).unwrap())
        .await
        .unwrap();
    let (_, zcashd_txid, ..) = zcashd_utxos[0].into_parts();

    let zaino_utxos = zaino_subscriber
        .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_taddr]).unwrap())
        .await
        .unwrap();
    let (_, zaino_txid, ..) = zaino_utxos[0].into_parts();

    dbg!(&txid_1);
    dbg!(&zcashd_utxos);
    assert_eq!(txid_1.first().to_string(), zcashd_txid.to_string());

    dbg!(&zaino_utxos);

    assert_eq!(zcashd_txid.to_string(), zaino_txid.to_string());

    test_manager.close().await;
}

mod zcashd {
    use super::*;

    pub(crate) mod zcash_indexer {
        use super::*;

        #[tokio::test]
        async fn check_info_no_cookie() {
            launch_json_server_check_info(false).await;
        }

        #[tokio::test]
        async fn check_info_with_cookie() {
            launch_json_server_check_info(false).await;
        }

        #[tokio::test]
        async fn z_get_address_balance() {
            z_get_address_balance_inner().await;
        }

        #[tokio::test]
        async fn get_block_count() {
            get_block_count_inner().await;
        }

        #[tokio::test]
        async fn z_get_block() {
            z_get_block_inner().await;
        }

        #[tokio::test]
        async fn get_raw_mempool() {
            get_raw_mempool_inner().await;
        }

        #[tokio::test]
        async fn z_get_treestate() {
            z_get_treestate_inner().await;
        }

        #[tokio::test]
        async fn z_get_subtrees_by_index() {
            z_get_subtrees_by_index_inner().await;
        }

        #[tokio::test]
        async fn get_raw_transaction() {
            get_raw_transaction_inner().await;
        }

        #[tokio::test]
        async fn get_address_tx_ids() {
            get_address_tx_ids_inner().await;
        }

        #[tokio::test]
        async fn z_get_address_utxos() {
            z_get_address_utxos_inner().await;
        }
    }
}
