use futures::StreamExt as _;
use zaino_fetch::jsonrpc::connector::{test_node_and_return_url, JsonRpcConnector};
use zaino_proto::proto::service::{
    AddressList, BlockId, BlockRange, Exclude, GetAddressUtxosArg, GetSubtreeRootsArg,
    TransparentAddressBlockFilter, TxFilter,
};
use zaino_state::{
    config::FetchServiceConfig,
    fetch::{FetchService, FetchServiceSubscriber},
    indexer::{LightWalletIndexer as _, ZcashIndexer as _, ZcashService as _},
    status::StatusType,
};
use zaino_testutils::{TestManager, ZCASHD_CHAIN_CACHE_DIR, ZEBRAD_CHAIN_CACHE_DIR};
use zebra_chain::{parameters::Network, subtree::NoteCommitmentSubtreeIndex};
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest};
use zingo_infra_testutils::services::validator::Validator as _;

#[tokio::test]
async fn launch_fetch_service_zcashd_regtest_no_cache() {
    launch_fetch_service("zcashd", None).await;
}

#[tokio::test]
async fn launch_fetch_service_zcashd_regtest_with_cache() {
    launch_fetch_service("zcashd", ZCASHD_CHAIN_CACHE_DIR.clone()).await;
}

#[tokio::test]
async fn launch_fetch_service_zebrad_regtest_no_cache() {
    launch_fetch_service("zebrad", None).await;
}

#[tokio::test]
async fn launch_fetch_service_zebrad_regtest_with_cache() {
    launch_fetch_service("zebrad", ZEBRAD_CHAIN_CACHE_DIR.clone()).await;
}

async fn create_test_manager_and_fetch_service(
    validator: &str,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (TestManager, FetchService, FetchServiceSubscriber) {
    let test_manager = TestManager::launch(
        validator,
        None,
        chain_cache,
        enable_zaino,
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
        Network::new_regtest(Some(1), Some(1)),
        true,
        true,
    ))
    .await
    .unwrap();
    let subscriber = fetch_service.get_subscriber().inner();
    (test_manager, fetch_service, subscriber)
}

async fn launch_fetch_service(validator: &str, chain_cache: Option<std::path::PathBuf>) {
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

#[tokio::test]
async fn fetch_service_get_address_balance_zcashd() {
    fetch_service_get_address_balance("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_address_balance_zebrad() {
    fetch_service_get_address_balance("zebrad").await;
}

async fn fetch_service_get_address_balance(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
        .z_get_address_balance(AddressStrings::new_valid(vec![recipient_address]).unwrap())
        .await
        .unwrap();

    dbg!(recipient_balance.clone());
    dbg!(fetch_service_balance);

    assert_eq!(recipient_balance.transparent_balance.unwrap(), 250_000,);
    assert_eq!(
        recipient_balance.transparent_balance.unwrap(),
        fetch_service_balance.balance,
    );

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_block_raw_zcashd() {
    fetch_service_get_block_raw("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_block_raw_zebrad() {
    fetch_service_get_block_raw("zebrad").await;
}

async fn fetch_service_get_block_raw(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, false, true, true, false).await;

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_block_object_zcashd() {
    fetch_service_get_block_object("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_block_object_zebrad() {
    fetch_service_get_block_object("zebrad").await;
}

async fn fetch_service_get_block_object(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, false, true, true, false).await;

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_raw_mempool_zcashd() {
    fetch_service_get_raw_mempool("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_raw_mempool_zebrad() {
    fetch_service_get_raw_mempool("zebrad").await;
}

async fn fetch_service_get_raw_mempool(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    let clients = test_manager
        .clients
        .as_ref()
        .expect("Clients are not initialized");

    let json_service = JsonRpcConnector::new_with_basic_auth(
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
    let mut json_service_mempool = json_service.get_raw_mempool().await.unwrap().transactions;

    dbg!(&fetch_service_mempool);
    dbg!(&json_service_mempool);
    json_service_mempool.sort();
    fetch_service_mempool.sort();
    assert_eq!(json_service_mempool, fetch_service_mempool);

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_z_get_treestate_zcashd() {
    fetch_service_z_get_treestate("zcashd").await;
}

#[tokio::test]
async fn fetch_service_z_get_treestate_zebrad() {
    fetch_service_z_get_treestate("zebrad").await;
}

async fn fetch_service_z_get_treestate(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    dbg!(fetch_service_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_z_get_subtrees_by_index_zcashd() {
    fetch_service_z_get_subtrees_by_index("zcashd").await;
}

#[tokio::test]
async fn fetch_service_z_get_subtrees_by_index_zebrad() {
    fetch_service_z_get_subtrees_by_index("zebrad").await;
}

async fn fetch_service_z_get_subtrees_by_index(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    dbg!(fetch_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_raw_transaction_zcashd() {
    fetch_service_get_raw_transaction("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_raw_transaction_zebrad() {
    fetch_service_get_raw_transaction("zebrad").await;
}

async fn fetch_service_get_raw_transaction(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    dbg!(fetch_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_address_tx_ids_zcashd() {
    fetch_service_get_address_tx_ids("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_address_tx_ids_zebrad() {
    fetch_service_get_address_tx_ids("zebrad").await;
}

async fn fetch_service_get_address_tx_ids(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
            vec![recipient_address],
            chain_height - 2,
            chain_height,
        ))
        .await
        .unwrap();

    dbg!(&tx);
    dbg!(&fetch_service_txids);
    assert_eq!(tx.first().to_string(), fetch_service_txids[0]);

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_address_utxos_zcashd() {
    fetch_service_get_address_utxos("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_address_utxos_zebrad() {
    fetch_service_get_address_utxos("zebrad").await;
}

async fn fetch_service_get_address_utxos(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
        .z_get_address_utxos(AddressStrings::new_valid(vec![recipient_address]).unwrap())
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    dbg!(&txid_1);
    dbg!(&fetch_service_utxos);
    assert_eq!(txid_1.first().to_string(), fetch_service_txid.to_string());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_latest_block_zcashd() {
    fetch_service_get_latest_block("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_latest_block_zebrad() {
    fetch_service_get_latest_block("zebrad").await;
}

async fn fetch_service_get_latest_block(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let json_service = JsonRpcConnector::new_with_basic_auth(
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
        hash: json_service_blockchain_info
            .best_block_hash
            .bytes_in_display_order()
            .to_vec(),
    });

    assert_eq!(fetch_service_get_latest_block.height, 2);
    assert_eq!(
        fetch_service_get_latest_block,
        json_service_get_latest_block
    );

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_block_zcashd() {
    fetch_service_get_block("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_block_zebrad() {
    fetch_service_get_block("zebrad").await;
}

async fn fetch_service_get_block(validator: &str) {
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

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_block_nullifiers_zcashd() {
    fetch_service_get_block_nullifiers("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_block_nullifiers_zebrad() {
    fetch_service_get_block_nullifiers("zebrad").await;
}

async fn fetch_service_get_block_nullifiers(validator: &str) {
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

#[tokio::test]
async fn fetch_service_get_block_range_zcashd() {
    fetch_service_get_block_range("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_block_range_zebrad() {
    fetch_service_get_block_range("zebrad").await;
}

async fn fetch_service_get_block_range(validator: &str) {
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

#[tokio::test]
async fn fetch_service_get_block_range_nullifiers_zcashd() {
    fetch_service_get_block_range_nullifiers("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_block_range_nullifiers_zebrad() {
    fetch_service_get_block_range_nullifiers("zebrad").await;
}

async fn fetch_service_get_block_range_nullifiers(validator: &str) {
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

#[tokio::test]
async fn fetch_service_get_transaction_mined_zcashd() {
    fetch_service_get_transaction_mined("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_transaction_mined_zebrad() {
    fetch_service_get_transaction_mined("zebrad").await;
}

async fn fetch_service_get_transaction_mined(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

#[tokio::test]
async fn fetch_service_get_transaction_mempool_zcashd() {
    fetch_service_get_transaction_mempool("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_transaction_mempool_zebrad() {
    fetch_service_get_transaction_mempool("zebrad").await;
}

async fn fetch_service_get_transaction_mempool(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

#[tokio::test]
async fn fetch_service_get_taddress_txids_zcashd() {
    fetch_service_get_taddress_txids("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_taddress_txids_zebrad() {
    fetch_service_get_taddress_txids("zebrad").await;
}

async fn fetch_service_get_taddress_txids(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
        vec![(&recipient_address, 250_000, None)],
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
        address: recipient_address,
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

#[tokio::test]
async fn fetch_service_get_taddress_balance_zcashd() {
    fetch_service_get_taddress_balance("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_taddress_balance_zebrad() {
    fetch_service_get_taddress_balance("zebrad").await;
}

async fn fetch_service_get_taddress_balance(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
        vec![(&recipient_address, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    clients.recipient.do_sync(true).await.unwrap();
    let balance = clients.recipient.do_balance().await;

    let address_list = AddressList {
        addresses: vec![recipient_address],
    };

    let fetch_service_balance = fetch_service_subscriber
        .get_taddress_balance(address_list.clone())
        .await
        .unwrap();

    dbg!(&fetch_service_balance);
    assert_eq!(
        fetch_service_balance.value_zat as u64,
        balance.transparent_balance.unwrap()
    );

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_mempool_tx_zcashd() {
    fetch_service_get_mempool_tx("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_mempool_tx_zebrad() {
    fetch_service_get_mempool_tx("zebrad").await;
}

async fn fetch_service_get_mempool_tx(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;
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

    let tx_1 = zingolib::testutils::lightclient::from_inputs::quick_send(
        &clients.faucet,
        vec![(
            &clients.get_recipient_address("transparent").await,
            250_000,
            None,
        )],
    )
    .await
    .unwrap();
    let tx_2 = zingolib::testutils::lightclient::from_inputs::quick_send(
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

#[tokio::test]
async fn fetch_service_get_mempool_stream_zcashd() {
    fetch_service_get_mempool_stream("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_mempool_stream_zebrad() {
    fetch_service_get_mempool_stream("zebrad").await;
}

async fn fetch_service_get_mempool_stream(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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

    let fetch_service_handle = tokio::spawn(async move {
        let fetch_service_stream = fetch_service_subscriber.get_mempool_stream().await.unwrap();
        let fetch_service_mempool_tx: Vec<_> = fetch_service_stream.collect().await;
        fetch_service_mempool_tx
            .into_iter()
            .filter_map(|result| result.ok())
            .collect::<Vec<_>>()
    });

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

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
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let fetch_mempool_tx = fetch_service_handle.await.unwrap();

    let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
    sorted_fetch_mempool_tx.sort_by_key(|tx| tx.data.clone());

    dbg!(sorted_fetch_mempool_tx);

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_tree_state_zcashd() {
    fetch_service_get_tree_state("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_tree_state_zebrad() {
    fetch_service_get_tree_state("zebrad").await;
}

async fn fetch_service_get_tree_state(validator: &str) {
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

#[tokio::test]
async fn fetch_service_get_latest_tree_state_zcashd() {
    fetch_service_get_latest_tree_state("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_latest_tree_state_zebrad() {
    fetch_service_get_latest_tree_state("zebrad").await;
}

async fn fetch_service_get_latest_tree_state(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    dbg!(fetch_service_subscriber
        .get_latest_tree_state()
        .await
        .unwrap());

    test_manager.close().await;
}

#[tokio::test]
async fn fetch_service_get_subtree_roots_zcashd() {
    fetch_service_get_subtree_roots("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_subtree_roots_zebrad() {
    fetch_service_get_subtree_roots("zebrad").await;
}

async fn fetch_service_get_subtree_roots(validator: &str) {
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

#[tokio::test]
async fn fetch_service_get_taddress_utxos_zcashd() {
    fetch_service_get_taddress_utxos("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_taddress_utxos_zebrad() {
    fetch_service_get_taddress_utxos("zebrad").await;
}

async fn fetch_service_get_taddress_utxos(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
        vec![(&recipient_address, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let utxos_arg = GetAddressUtxosArg {
        addresses: vec![recipient_address],
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

#[tokio::test]
async fn fetch_service_get_taddress_utxos_stream_zcashd() {
    fetch_service_get_taddress_utxos_stream("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_taddress_utxos_stream_zebrad() {
    fetch_service_get_taddress_utxos_stream("zebrad").await;
}

async fn fetch_service_get_taddress_utxos_stream(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

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
        vec![(&recipient_address, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let utxos_arg = GetAddressUtxosArg {
        addresses: vec![recipient_address],
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

#[tokio::test]
async fn fetch_service_get_lightd_info_zcashd() {
    fetch_service_get_lightd_info("zcashd").await;
}

#[tokio::test]
async fn fetch_service_get_lightd_info_zebrad() {
    fetch_service_get_lightd_info("zebrad").await;
}

async fn fetch_service_get_lightd_info(validator: &str) {
    let (mut test_manager, _fetch_service, fetch_service_subscriber) =
        create_test_manager_and_fetch_service(validator, None, true, true, true, true).await;

    dbg!(fetch_service_subscriber.get_lightd_info().await.unwrap());

    test_manager.close().await;
}
