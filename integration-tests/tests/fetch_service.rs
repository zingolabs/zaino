use futures::StreamExt as _;
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_proto::proto::service::{
    AddressList, BlockId, BlockRange, Exclude, GetAddressUtxosArg, GetSubtreeRootsArg,
    TransparentAddressBlockFilter, TxFilter,
};
use zaino_state::{
    BackendType, FetchService, FetchServiceConfig, FetchServiceError, FetchServiceSubscriber,
    LightWalletIndexer, StatusType, ZcashIndexer, ZcashService as _,
};
use zaino_testutils::Validator as _;
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::{parameters::Network, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetBlock, GetBlockHash};

async fn create_test_manager_and_fetch_service(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (TestManager, FetchService, FetchServiceSubscriber) {
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
    let subscriber = fetch_service.get_subscriber().inner();
    (test_manager, fetch_service, subscriber)
}

async fn launch_fetch_service(validator: &ValidatorKind, chain_cache: Option<std::path::PathBuf>) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, chain_cache, false, true, true, false)
            .await;
    assert_eq!(fetch_service_subscriber.status(), StatusType::Ready);
    dbg!(fetch_service_subscriber.data.clone());
    dbg!(fetch_service_subscriber.get_info().await.unwrap());
    dbg!(fetch_service_subscriber
        .get_blockchain_info()
        .await
        .unwrap()
        .blocks());

    test_manager.close().await;
}

async fn fetch_service_get_address_balance(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_address = clients.get_recipient_address("transparent").await;

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

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_address.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.recipient.sync_and_await().await.unwrap();
    let recipient_balance = clients.recipient.do_balance().await;

    let fetch_service_balance = fetch_service_subscriber
        .z_get_address_balance(AddressStrings::new(vec![recipient_address]))
        .await
        .unwrap();

    dbg!(recipient_balance.clone());
    dbg!(fetch_service_balance);

    assert_eq!(
        recipient_balance.confirmed_transparent_balance.unwrap(),
        250_000,
    );
    assert_eq!(
        recipient_balance.confirmed_transparent_balance.unwrap(),
        fetch_service_balance.balance(),
    );

    test_manager.close().await;
}

async fn fetch_service_get_block_raw(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, false, true, true, false).await;

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_get_block_object(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, false, true, true, false).await;

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_get_raw_mempool(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

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
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let mut fetch_service_mempool = fetch_service_subscriber.get_raw_mempool().await.unwrap();
    let mut json_service_mempool = json_service.get_raw_mempool().await.unwrap().transactions;

    dbg!(&fetch_service_mempool);
    dbg!(&json_service_mempool);
    json_service_mempool.sort();
    fetch_service_mempool.sort();
    assert_eq!(json_service_mempool, fetch_service_mempool);

    test_manager.close().await;
}

// Zebra hasn't yet implemented the `getmempoolinfo` RPC.
async fn test_get_mempool_info(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

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

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.sync_and_await().await.unwrap();

    // We do this because Zebra does not support mine-to-Orchard
    // so we need to shield it manually.
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
    }

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    match validator {
        ValidatorKind::Zebrad => {
            // Zebra doesn’t implement this RPC, so we expect a failure.
            let err = fetch_service_subscriber
                .get_mempool_info()
                .await
                .expect_err("Zebrad should return an error for get_mempool_info.");

            assert!(
                matches!(err, FetchServiceError::Critical(_)),
                "Unexpected error variant: {err:?}"
            );
        }
        ValidatorKind::Zcashd => {
            // Zcashd implements the RPC, so the call should succeed and match JSON-RPC output.
            let fetch_info = fetch_service_subscriber.get_mempool_info().await.unwrap();
            let json_info = json_service.get_mempool_info().await.unwrap();

            assert_eq!(json_info.size, fetch_info.size);
            assert_eq!(json_info.bytes, fetch_info.bytes);
            assert_eq!(json_info.usage, fetch_info.usage);
            assert!(fetch_info.usage > 0);
        }
    };

    test_manager.close().await;
}

async fn fetch_service_z_get_treestate(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    dbg!(fetch_service_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_z_get_subtrees_by_index(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    dbg!(fetch_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_get_raw_transaction(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    dbg!(fetch_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_get_address_tx_ids(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    let tx = zaino_testutils::from_inputs::quick_send(
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
            vec![recipient_taddr],
            Some(chain_height - 2),
            None,
        ))
        .await
        .unwrap();

    dbg!(&tx);
    dbg!(&fetch_service_txids);
    assert_eq!(tx.first().to_string(), fetch_service_txids[0]);

    test_manager.close().await;
}

async fn fetch_service_get_address_utxos(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    let txid_1 = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.faucet.sync_and_await().await.unwrap();

    let fetch_service_utxos = fetch_service_subscriber
        .z_get_address_utxos(AddressStrings::new(vec![recipient_taddr]))
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    dbg!(&txid_1);
    dbg!(&fetch_service_utxos);
    assert_eq!(txid_1.first().to_string(), fetch_service_txid.to_string());

    test_manager.close().await;
}

async fn fetch_service_get_latest_block(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

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

    let fetch_service_get_latest_block =
        dbg!(fetch_service_subscriber.get_latest_block().await.unwrap());

    let json_service_blockchain_info = json_service.get_blockchain_info().await.unwrap();

    let json_service_get_latest_block = dbg!(BlockId {
        height: json_service_blockchain_info.blocks.0 as u64,
        hash: json_service_blockchain_info.best_block_hash.0.to_vec(),
    });

    assert_eq!(fetch_service_get_latest_block.height, 2);
    assert_eq!(
        fetch_service_get_latest_block,
        json_service_get_latest_block
    );

    test_manager.close().await;
}

async fn assert_fetch_service_difficulty_matches_rpc(validator: &ValidatorKind) {
    let (test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    let fetch_service_get_difficulty = fetch_service_subscriber.get_difficulty().await.unwrap();

    let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

    let rpc_difficulty_response = jsonrpc_client.get_difficulty().await.unwrap();

    assert_eq!(fetch_service_get_difficulty, rpc_difficulty_response.0);
}

async fn fetch_service_get_block(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    let block_id = BlockId {
        height: 1,
        hash: Vec::new(),
    };

    let fetch_service_get_block = dbg!(fetch_service_subscriber
        .get_block(block_id.clone())
        .await
        .unwrap());

    assert_eq!(fetch_service_get_block.height, block_id.height);
    let block_id_by_hash = BlockId {
        height: 0,
        hash: fetch_service_get_block.hash.clone(),
    };
    let fetch_service_get_block_by_hash = fetch_service_subscriber
        .get_block(block_id_by_hash.clone())
        .await
        .unwrap();
    assert_eq!(fetch_service_get_block_by_hash.hash, block_id_by_hash.hash);

    test_manager.close().await;
}

async fn fetch_service_get_best_blockhash(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    test_manager.local_net.generate_blocks(5).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let inspected_block: GetBlock = fetch_service_subscriber
        // Some(verbosity) : 1 for JSON Object, 2 for tx data as JSON instead of hex
        .z_get_block("6".to_string(), Some(1))
        .await
        .unwrap();

    let ret = match inspected_block {
        GetBlock::Object(obj) => Some(obj.hash()),
        _ => None,
    };

    let fetch_service_get_best_blockhash: GetBlockHash =
        dbg!(fetch_service_subscriber.get_best_blockhash().await.unwrap());

    assert_eq!(
        fetch_service_get_best_blockhash.hash(),
        ret.expect("ret to be Some(GetBlockHash) not None")
    );

    test_manager.close().await;
}

async fn fetch_service_get_block_count(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    test_manager.local_net.generate_blocks(5).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let block_id = BlockId {
        height: 6,
        hash: Vec::new(),
    };

    let fetch_service_get_block_count =
        dbg!(fetch_service_subscriber.get_block_count().await.unwrap());

    assert_eq!(fetch_service_get_block_count.0 as u64, block_id.height);

    test_manager.close().await;
}

async fn fetch_service_get_block_nullifiers(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    let block_id = BlockId {
        height: 1,
        hash: Vec::new(),
    };

    let fetch_service_get_block_nullifiers = dbg!(fetch_service_subscriber
        .get_block_nullifiers(block_id.clone())
        .await
        .unwrap());

    assert_eq!(fetch_service_get_block_nullifiers.height, block_id.height);

    test_manager.close().await;
}

async fn fetch_service_get_block_range(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    test_manager.local_net.generate_blocks(10).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let block_range = BlockRange {
        start: Some(BlockId {
            height: 1,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: 10,
            hash: Vec::new(),
        }),
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_block_range(block_range.clone())
        .await
        .unwrap();
    let fetch_service_compact_blocks: Vec<_> = fetch_service_stream.collect().await;

    let fetch_blocks: Vec<_> = fetch_service_compact_blocks
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(fetch_blocks);

    test_manager.close().await;
}

async fn fetch_service_get_block_range_nullifiers(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    test_manager.local_net.generate_blocks(10).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let block_range = BlockRange {
        start: Some(BlockId {
            height: 1,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: 10,
            hash: Vec::new(),
        }),
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_block_range_nullifiers(block_range.clone())
        .await
        .unwrap();
    let fetch_service_compact_blocks: Vec<_> = fetch_service_stream.collect().await;

    let fetch_nullifiers: Vec<_> = fetch_service_compact_blocks
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(fetch_nullifiers);

    test_manager.close().await;
}

async fn fetch_service_get_transaction_mined(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let tx_filter = TxFilter {
        block: None,
        index: 0,
        hash: tx.first().as_ref().to_vec(),
    };

    let fetch_service_get_transaction = dbg!(fetch_service_subscriber
        .get_transaction(tx_filter.clone())
        .await
        .unwrap());

    dbg!(fetch_service_get_transaction);

    test_manager.close().await;
}

async fn fetch_service_get_transaction_mempool(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    let tx_filter = TxFilter {
        block: None,
        index: 0,
        hash: tx.first().as_ref().to_vec(),
    };

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_service_get_transaction = dbg!(fetch_service_subscriber
        .get_transaction(tx_filter.clone())
        .await
        .unwrap());

    dbg!(fetch_service_get_transaction);

    test_manager.close().await;
}

async fn fetch_service_get_taddress_txids(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
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

    let block_filter = TransparentAddressBlockFilter {
        address: recipient_taddr,
        range: Some(BlockRange {
            start: Some(BlockId {
                height: (chain_height - 2) as u64,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: chain_height as u64,
                hash: Vec::new(),
            }),
        }),
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_taddress_txids(block_filter.clone())
        .await
        .unwrap();
    let fetch_service_tx: Vec<_> = fetch_service_stream.collect().await;

    let fetch_tx: Vec<_> = fetch_service_tx
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(tx);
    dbg!(&fetch_tx);

    test_manager.close().await;
}

async fn fetch_service_get_taddress_balance(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.recipient.sync_and_await().await.unwrap();
    let balance = clients.recipient.do_balance().await;

    let address_list = AddressList {
        addresses: vec![recipient_taddr],
    };

    let fetch_service_balance = fetch_service_subscriber
        .get_taddress_balance(address_list.clone())
        .await
        .unwrap();

    dbg!(&fetch_service_balance);
    assert_eq!(
        fetch_service_balance.value_zat as u64,
        balance.confirmed_transparent_balance.unwrap()
    );

    test_manager.close().await;
}

async fn fetch_service_get_mempool_tx(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
    let tx_1 = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    let tx_2 = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let exclude_list_empty = Exclude { txid: Vec::new() };

    let fetch_service_stream = fetch_service_subscriber
        .get_mempool_tx(exclude_list_empty.clone())
        .await
        .unwrap();
    let fetch_service_mempool_tx: Vec<_> = fetch_service_stream.collect().await;

    let fetch_mempool_tx: Vec<_> = fetch_service_mempool_tx
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
    sorted_fetch_mempool_tx.sort_by_key(|tx| tx.hash.clone());

    let mut tx1_bytes = *tx_1.first().as_ref();
    tx1_bytes.reverse();
    let mut tx2_bytes = *tx_2.first().as_ref();
    tx2_bytes.reverse();

    let mut sorted_txids = [tx1_bytes, tx2_bytes];
    sorted_txids.sort_by_key(|hash| *hash);

    assert_eq!(sorted_fetch_mempool_tx[0].hash, sorted_txids[0]);
    assert_eq!(sorted_fetch_mempool_tx[1].hash, sorted_txids[1]);

    let exclude_list = Exclude {
        txid: vec![sorted_txids[0][..8].to_vec()],
    };

    let exclude_fetch_service_stream = fetch_service_subscriber
        .get_mempool_tx(exclude_list.clone())
        .await
        .unwrap();
    let exclude_fetch_service_mempool_tx: Vec<_> = exclude_fetch_service_stream.collect().await;

    let exclude_fetch_mempool_tx: Vec<_> = exclude_fetch_service_mempool_tx
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    let mut sorted_exclude_fetch_mempool_tx = exclude_fetch_mempool_tx.clone();
    sorted_exclude_fetch_mempool_tx.sort_by_key(|tx| tx.hash.clone());

    assert_eq!(sorted_exclude_fetch_mempool_tx[0].hash, sorted_txids[1]);

    test_manager.close().await;
}

async fn fetch_service_get_mempool_stream(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    let fetch_service_handle = tokio::spawn(async move {
        let fetch_service_stream = fetch_service_subscriber.get_mempool_stream().await.unwrap();
        let fetch_service_mempool_tx: Vec<_> = fetch_service_stream.collect().await;
        fetch_service_mempool_tx
            .into_iter()
            .filter_map(|result| result.ok())
            .collect::<Vec<_>>()
    });

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_mempool_tx = fetch_service_handle.await.unwrap();

    let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
    sorted_fetch_mempool_tx.sort_by_key(|tx| tx.data.clone());

    dbg!(sorted_fetch_mempool_tx);

    test_manager.close().await;
}

async fn fetch_service_get_tree_state(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    let block_id = BlockId {
        height: 1,
        hash: Vec::new(),
    };

    let fetch_service_get_tree_state = dbg!(fetch_service_subscriber
        .get_tree_state(block_id.clone())
        .await
        .unwrap());

    dbg!(fetch_service_get_tree_state);

    test_manager.close().await;
}

async fn fetch_service_get_latest_tree_state(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    dbg!(fetch_service_subscriber
        .get_latest_tree_state()
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_get_subtree_roots(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    let subtree_roots_arg = GetSubtreeRootsArg {
        start_index: 0,
        shielded_protocol: 1,
        max_entries: 0,
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_subtree_roots(subtree_roots_arg.clone())
        .await
        .unwrap();
    let fetch_service_roots: Vec<_> = fetch_service_stream.collect().await;

    let fetch_roots: Vec<_> = fetch_service_roots
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(fetch_roots);

    test_manager.close().await;
}

async fn fetch_service_get_taddress_utxos(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let utxos_arg = GetAddressUtxosArg {
        addresses: vec![recipient_taddr],
        start_height: 0,
        max_entries: 0,
    };

    let fetch_service_get_taddress_utxos = fetch_service_subscriber
        .get_address_utxos(utxos_arg.clone())
        .await
        .unwrap();

    dbg!(tx);
    dbg!(&fetch_service_get_taddress_utxos);

    test_manager.close().await;
}

async fn fetch_service_get_taddress_utxos_stream(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let utxos_arg = GetAddressUtxosArg {
        addresses: vec![recipient_taddr],
        start_height: 0,
        max_entries: 0,
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_address_utxos_stream(utxos_arg.clone())
        .await
        .unwrap();
    let fetch_service_utxos: Vec<_> = fetch_service_stream.collect().await;

    let fetch_utxos: Vec<_> = fetch_service_utxos
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(fetch_utxos);

    test_manager.close().await;
}

async fn fetch_service_get_lightd_info(validator: &ValidatorKind) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    dbg!(fetch_service_subscriber.get_lightd_info().await.unwrap());

    test_manager.close().await;
}

mod zcashd {

    use super::*;

    mod launch {

        use super::*;

        #[tokio::test]
        pub(crate) async fn regtest_no_cache() {
            launch_fetch_service(&ValidatorKind::Zcashd, None).await;
        }

        #[tokio::test]
        #[ignore = "We no longer use chain caches. See zcashd::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() {
            launch_fetch_service(
                &ValidatorKind::Zcashd,
                zaino_testutils::ZCASHD_CHAIN_CACHE_DIR.clone(),
            )
            .await;
        }
    }

    mod get {

        use super::*;

        #[tokio::test]
        pub(crate) async fn address_balance() {
            fetch_service_get_address_balance(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block_raw() {
            fetch_service_get_block_raw(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block_object() {
            fetch_service_get_block_object(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn raw_mempool() {
            fetch_service_get_raw_mempool(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn mempool_info() {
            test_get_mempool_info(&ValidatorKind::Zcashd).await;
        }

        mod z {

            use super::*;

            #[tokio::test]
            pub(crate) async fn get_treestate() {
                fetch_service_z_get_treestate(&ValidatorKind::Zcashd).await;
            }

            #[tokio::test]
            pub(crate) async fn subtrees_by_index() {
                fetch_service_z_get_subtrees_by_index(&ValidatorKind::Zcashd).await;
            }
        }

        #[tokio::test]
        pub(crate) async fn raw_transaction() {
            fetch_service_get_raw_transaction(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn address_tx_ids() {
            fetch_service_get_address_tx_ids(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn address_utxos() {
            fetch_service_get_address_utxos(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn latest_block() {
            fetch_service_get_latest_block(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block() {
            fetch_service_get_block(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn difficulty() {
            assert_fetch_service_difficulty_matches_rpc(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn best_blockhash() {
            fetch_service_get_best_blockhash(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block_count() {
            fetch_service_get_block_count(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block_nullifiers() {
            fetch_service_get_block_nullifiers(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block_range() {
            fetch_service_get_block_range(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn block_range_nullifiers() {
            fetch_service_get_block_range_nullifiers(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn transaction_mined() {
            fetch_service_get_transaction_mined(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn transaction_mempool() {
            fetch_service_get_transaction_mempool(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_txids() {
            fetch_service_get_taddress_txids(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_balance() {
            fetch_service_get_taddress_balance(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn mempool_tx() {
            fetch_service_get_mempool_tx(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn mempool_stream() {
            fetch_service_get_mempool_stream(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn tree_state() {
            fetch_service_get_tree_state(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn latest_tree_state() {
            fetch_service_get_latest_tree_state(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn subtree_roots() {
            fetch_service_get_subtree_roots(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_utxos() {
            fetch_service_get_taddress_utxos(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_utxos_stream() {
            fetch_service_get_taddress_utxos_stream(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn lightd_info() {
            fetch_service_get_lightd_info(&ValidatorKind::Zcashd).await;
        }
    }
}

mod zebrad {

    use super::*;

    mod launch {

        use super::*;

        #[tokio::test]
        pub(crate) async fn regtest_no_cache() {
            launch_fetch_service(&ValidatorKind::Zebrad, None).await;
        }

        #[tokio::test]
        #[ignore = "We no longer use chain caches. See zebrad::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() {
            launch_fetch_service(
                &ValidatorKind::Zebrad,
                zaino_testutils::ZEBRAD_CHAIN_CACHE_DIR.clone(),
            )
            .await;
        }
    }

    mod get {

        use super::*;

        #[tokio::test]
        pub(crate) async fn address_balance() {
            fetch_service_get_address_balance(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block_raw() {
            fetch_service_get_block_raw(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block_object() {
            fetch_service_get_block_object(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn raw_mempool() {
            fetch_service_get_raw_mempool(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn mempool_info() {
            test_get_mempool_info(&ValidatorKind::Zebrad).await;
        }

        mod z {

            use super::*;

            #[tokio::test]
            pub(crate) async fn treestate() {
                fetch_service_z_get_treestate(&ValidatorKind::Zebrad).await;
            }

            #[tokio::test]
            pub(crate) async fn subtrees_by_index() {
                fetch_service_z_get_subtrees_by_index(&ValidatorKind::Zebrad).await;
            }
        }

        #[tokio::test]
        pub(crate) async fn raw_transaction() {
            fetch_service_get_raw_transaction(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn address_tx_ids() {
            fetch_service_get_address_tx_ids(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn address_utxos() {
            fetch_service_get_address_utxos(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn latest_block() {
            fetch_service_get_latest_block(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block() {
            fetch_service_get_block(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn difficulty() {
            assert_fetch_service_difficulty_matches_rpc(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn best_blockhash() {
            fetch_service_get_best_blockhash(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block_count() {
            fetch_service_get_block_count(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block_nullifiers() {
            fetch_service_get_block_nullifiers(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block_range() {
            fetch_service_get_block_range(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn block_range_nullifiers() {
            fetch_service_get_block_range_nullifiers(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn transaction_mined() {
            fetch_service_get_transaction_mined(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn transaction_mempool() {
            fetch_service_get_transaction_mempool(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_txids() {
            fetch_service_get_taddress_txids(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_balance() {
            fetch_service_get_taddress_balance(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn mempool_tx() {
            fetch_service_get_mempool_tx(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn mempool_stream() {
            fetch_service_get_mempool_stream(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn tree_state() {
            fetch_service_get_tree_state(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn latest_tree_state() {
            fetch_service_get_latest_tree_state(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn subtree_roots() {
            fetch_service_get_subtree_roots(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_utxos() {
            fetch_service_get_taddress_utxos(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn taddress_utxos_stream() {
            fetch_service_get_taddress_utxos_stream(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn lightd_info() {
            fetch_service_get_lightd_info(&ValidatorKind::Zebrad).await;
        }
    }
}
