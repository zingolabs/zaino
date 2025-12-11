//! These tests compare the output of `FetchService` with the output of `JsonRpcConnector`.

use futures::StreamExt as _;
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_proto::proto::service::{
    AddressList, BlockId, BlockRange, Exclude, GetAddressUtxosArg, GetSubtreeRootsArg,
    TransparentAddressBlockFilter, TxFilter,
};
use zaino_state::FetchServiceSubscriber;
#[allow(deprecated)]
use zaino_state::{FetchService, LightWalletIndexer, StatusType, ZcashIndexer};
use zaino_testutils::{TestManager, ValidatorExt, ValidatorKind};
use zebra_chain::parameters::subsidy::ParameterSubsidy as _;
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::client::ValidateAddressResponse;
use zebra_rpc::methods::{
    GetAddressBalanceRequest, GetAddressTxIdsRequest, GetBlock, GetBlockHash,
};
use zip32::AccountId;

#[allow(deprecated)]
async fn create_test_manager_and_fetch_service<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_clients: bool,
) -> (TestManager<V, FetchService>, FetchServiceSubscriber) {
    let mut test_manager = TestManager::<V, FetchService>::launch(
        validator,
        None,
        None,
        chain_cache,
        true,
        false,
        enable_clients,
    )
    .await
    .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();
    (test_manager, fetch_service_subscriber)
}

async fn launch_fetch_service<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
) {
    let (mut test_manager, fetch_service_subscriber) =
        create_test_manager_and_fetch_service::<V>(validator, chain_cache, false).await;
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

#[allow(deprecated)]
async fn fetch_service_get_address_balance<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_address = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    dbg!(clients
        .faucet
        .account_balance(AccountId::ZERO)
        .await
        .unwrap());
    dbg!(clients.faucet.transaction_summaries(false).await.unwrap());

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_address.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    clients.recipient.sync_and_await().await.unwrap();
    let recipient_balance = clients
        .recipient
        .account_balance(zip32::AccountId::ZERO)
        .await
        .unwrap();

    let fetch_service_balance = fetch_service_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_address]))
        .await
        .unwrap();

    dbg!(recipient_balance.clone());
    dbg!(fetch_service_balance);

    assert_eq!(
        recipient_balance
            .confirmed_transparent_balance
            .unwrap()
            .into_u64(),
        250_000,
    );
    assert_eq!(
        recipient_balance
            .confirmed_transparent_balance
            .unwrap()
            .into_u64(),
        fetch_service_balance.balance(),
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_raw<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_object<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_raw_mempool<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

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

    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua: String = clients.get_recipient_address("unified").await;
    let recipient_taddr: String = clients.get_recipient_address("transparent").await;
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

// `getmempoolinfo` computed from local Broadcast state for all validators
#[allow(deprecated)]
pub async fn test_get_mempool_info<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;
    clients.faucet.sync_and_await().await.unwrap();

    // Zebra cannot mine directly to Orchard in this setup, so shield funds first.
    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();

        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();

        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    }

    let recipient_unified_address = clients.get_recipient_address("unified").await;
    let recipient_transparent_address = clients.get_recipient_address("transparent").await;

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_transparent_address, 250_000, None)],
    )
    .await
    .unwrap();

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_unified_address, 250_000, None)],
    )
    .await
    .unwrap();

    // Allow the broadcaster and subscribers to observe new transactions.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Internal method now used for all validators.
    let info = fetch_service_subscriber.get_mempool_info().await.unwrap();

    // Derive expected values directly from the current mempool contents.
    let entries = fetch_service_subscriber.mempool.get_mempool().await;

    // Size
    assert_eq!(info.size, entries.len() as u64);
    assert!(info.size >= 1);

    // Bytes: sum of SerializedTransaction lengths
    let expected_bytes: u64 = entries
        .iter()
        .map(|(_, value)| value.serialized_tx.as_ref().as_ref().len() as u64)
        .sum();

    // Key heap bytes: sum of txid String capacities
    let expected_key_heap_bytes: u64 = entries
        .iter()
        .map(|(key, _)| key.txid.capacity() as u64)
        .sum();

    let expected_usage = expected_bytes.saturating_add(expected_key_heap_bytes);

    assert!(info.bytes > 0);
    assert_eq!(info.bytes, expected_bytes);

    assert!(info.usage >= info.bytes);
    assert_eq!(info.usage, expected_usage);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_z_get_treestate<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        // TODO: investigate why 101 blocks are needed instead of the previous 100 blocks (chain index integration related?)
        test_manager
            .generate_blocks_and_poll_indexer(101, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    dbg!(fetch_service_subscriber
        .z_get_treestate("2".to_string())
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_z_get_subtrees_by_index<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
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

    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    dbg!(fetch_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_raw_transaction<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    dbg!(fetch_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_address_tx_ids<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_address_utxos<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let txid_1 = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    clients.faucet.sync_and_await().await.unwrap();

    let fetch_service_utxos = fetch_service_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    dbg!(&txid_1);
    dbg!(&fetch_service_utxos);
    assert_eq!(txid_1.first().to_string(), fetch_service_txid.to_string());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_latest_block<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

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

    let fetch_service_get_latest_block =
        dbg!(fetch_service_subscriber.get_latest_block().await.unwrap());

    let json_service_blockchain_info = json_service.get_blockchain_info().await.unwrap();

    let json_service_get_latest_block = dbg!(BlockId {
        height: json_service_blockchain_info.blocks.0 as u64,
        hash: json_service_blockchain_info.best_block_hash.0.to_vec(),
    });

    assert_eq!(fetch_service_get_latest_block.height, 3);
    assert_eq!(
        fetch_service_get_latest_block,
        json_service_get_latest_block
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn assert_fetch_service_difficulty_matches_rpc<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let fetch_service_get_difficulty = fetch_service_subscriber.get_difficulty().await.unwrap();

    let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

    let rpc_difficulty_response = jsonrpc_client.get_difficulty().await.unwrap();
    assert_eq!(fetch_service_get_difficulty, rpc_difficulty_response.0);
}

#[allow(deprecated)]
async fn assert_fetch_service_mininginfo_matches_rpc<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let fetch_service_mining_info = fetch_service_subscriber.get_mining_info().await.unwrap();

    let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

    let rpc_mining_info_response = jsonrpc_client.get_mining_info().await.unwrap();
    assert_eq!(fetch_service_mining_info, rpc_mining_info_response);
}

#[allow(deprecated)]
async fn assert_fetch_service_peerinfo_matches_rpc<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let fetch_service_get_peer_info = fetch_service_subscriber.get_peer_info().await.unwrap();

    let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

    let rpc_peer_info_response = jsonrpc_client.get_peer_info().await.unwrap();

    dbg!(&rpc_peer_info_response);
    dbg!(&fetch_service_get_peer_info);
    assert_eq!(fetch_service_get_peer_info, rpc_peer_info_response);
}

#[allow(deprecated)]
async fn fetch_service_get_block_subsidy<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let first_halving_height = fetch_service_subscriber
        .network()
        .to_zebra_network()
        .height_for_first_halving();
    let block_limit = match validator {
        // Block generation is more expensive in zcashd, and 10 is sufficient
        ValidatorKind::Zcashd => 10,
        // To stay consistent with zcashd, ten successful examples. Any calls
        // below the first halving height should fail.
        ValidatorKind::Zebrad => first_halving_height.0 + 10,
    };

    for i in 0..block_limit {
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        // Zebrad does not support the founders' reward block subsidy
        if i < first_halving_height.0 && validator == &ValidatorKind::Zebrad {
            assert!(fetch_service_subscriber.get_block_subsidy(i).await.is_err());
            continue;
        }
        let fetch_service_get_block_subsidy =
            fetch_service_subscriber.get_block_subsidy(i).await.unwrap();

        let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

        let rpc_block_subsidy_response = jsonrpc_client.get_block_subsidy(i).await.unwrap();
        assert_eq!(fetch_service_get_block_subsidy, rpc_block_subsidy_response);
    }
}

#[allow(deprecated)]
async fn fetch_service_get_block<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

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

#[allow(deprecated)]
async fn fetch_service_get_block_header<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    const BLOCK_LIMIT: u32 = 10;

    for i in 0..BLOCK_LIMIT {
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;

        let block = fetch_service_subscriber
            .z_get_block(i.to_string(), Some(1))
            .await
            .unwrap();

        let block_hash = match block {
            GetBlock::Object(block) => block.hash(),
            GetBlock::Raw(_) => panic!("Expected block object"),
        };

        let fetch_service_get_block_header = fetch_service_subscriber
            .get_block_header(block_hash.to_string(), false)
            .await
            .unwrap();

        let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

        let rpc_block_header_response = jsonrpc_client
            .get_block_header(block_hash.to_string(), false)
            .await
            .unwrap();

        let fetch_service_get_block_header_verbose = fetch_service_subscriber
            .get_block_header(block_hash.to_string(), true)
            .await
            .unwrap();

        let rpc_block_header_response_verbose = jsonrpc_client
            .get_block_header(block_hash.to_string(), true)
            .await
            .unwrap();

        assert_eq!(fetch_service_get_block_header, rpc_block_header_response);
        assert_eq!(
            fetch_service_get_block_header_verbose,
            rpc_block_header_response_verbose
        );
    }
}

#[allow(deprecated)]
async fn fetch_service_get_best_blockhash<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();
    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(5, &fetch_service_subscriber)
        .await;

    let inspected_block: GetBlock = fetch_service_subscriber
        // Some(verbosity) : 1 for JSON Object, 2 for tx data as JSON instead of hex
        .z_get_block("7".to_string(), Some(1))
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

#[allow(deprecated)]
async fn fetch_service_get_block_count<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(5, &fetch_service_subscriber)
        .await;

    let block_id = BlockId {
        height: 7,
        hash: Vec::new(),
    };

    let fetch_service_get_block_count =
        dbg!(fetch_service_subscriber.get_block_count().await.unwrap());

    assert_eq!(fetch_service_get_block_count.0 as u64, block_id.height);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_validate_address<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    // scriptpubkey: "76a914000000000000000000000000000000000000000088ac"
    let expected_validation = ValidateAddressResponse::new(
        true,
        Some("tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_string()),
        Some(false),
    );
    let fetch_service_validate_address = fetch_service_subscriber
        .validate_address("tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_string())
        .await
        .unwrap();

    assert_eq!(fetch_service_validate_address, expected_validation);

    // scriptpubkey: "a914000000000000000000000000000000000000000087"
    let expected_validation_script = ValidateAddressResponse::new(
        true,
        Some("t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2".to_string()),
        Some(true),
    );

    let fetch_service_validate_address_script = fetch_service_subscriber
        .validate_address("t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2".to_string())
        .await
        .unwrap();

    assert_eq!(
        fetch_service_validate_address_script,
        expected_validation_script
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_nullifiers<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

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

#[allow(deprecated)]
async fn fetch_service_get_block_range<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(10, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_block_range_nullifiers<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    test_manager
        .generate_blocks_and_poll_indexer(10, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_transaction_mined<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_ua, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_transaction_mempool<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
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

#[allow(deprecated)]
async fn fetch_service_get_taddress_txids<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_taddress_balance<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    clients.recipient.sync_and_await().await.unwrap();
    let balance = clients
        .recipient
        .account_balance(zip32::AccountId::ZERO)
        .await
        .unwrap();

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
        balance.confirmed_transparent_balance.unwrap().into_u64()
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_mempool_tx<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
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

    let tx1_bytes = *tx_1.first().as_ref();
    let tx2_bytes = *tx_2.first().as_ref();
    let mut sorted_txids = [tx1_bytes, tx2_bytes];
    sorted_txids.sort_by_key(|hash| *hash);

    assert_eq!(sorted_fetch_mempool_tx[0].hash, sorted_txids[0]);
    assert_eq!(sorted_fetch_mempool_tx[1].hash, sorted_txids[1]);
    assert_eq!(sorted_fetch_mempool_tx.len(), 2);

    let exclude_list = Exclude {
        txid: vec![sorted_txids[0][8..].to_vec()],
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
    assert_eq!(sorted_exclude_fetch_mempool_tx.len(), 1);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_mempool_stream<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let fetch_service_subscriber_2 = fetch_service_subscriber.clone();
    let fetch_service_handle = tokio::spawn(async move {
        let fetch_service_stream = fetch_service_subscriber_2
            .get_mempool_stream()
            .await
            .unwrap();
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
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

    let fetch_mempool_tx = fetch_service_handle.await.unwrap();

    let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
    sorted_fetch_mempool_tx.sort_by_key(|tx| tx.data.clone());

    dbg!(sorted_fetch_mempool_tx);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_tree_state<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

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

#[allow(deprecated)]
async fn fetch_service_get_latest_tree_state<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    dbg!(fetch_service_subscriber
        .get_latest_tree_state()
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_subtree_roots<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

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

#[allow(deprecated)]
async fn fetch_service_get_taddress_utxos<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let tx = zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_taddress_utxos_stream<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, true)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager
            .generate_blocks_and_poll_indexer(100, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager
            .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
            .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    zaino_testutils::from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_taddr, 250_000, None)],
    )
    .await
    .unwrap();
    test_manager
        .generate_blocks_and_poll_indexer(1, &fetch_service_subscriber)
        .await;

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

#[allow(deprecated)]
async fn fetch_service_get_lightd_info<V: ValidatorExt>(validator: &ValidatorKind) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    dbg!(fetch_service_subscriber.get_lightd_info().await.unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn assert_fetch_service_getnetworksols_matches_rpc<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let mut test_manager =
        TestManager::<V, FetchService>::launch(validator, None, None, None, true, false, false)
            .await
            .unwrap();

    let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

    let fetch_service_get_networksolps = fetch_service_subscriber
        .get_network_sol_ps(None, None)
        .await
        .unwrap();

    let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

    let rpc_getnetworksolps_response = jsonrpc_client.get_network_sol_ps(None, None).await.unwrap();
    assert_eq!(fetch_service_get_networksolps, rpc_getnetworksolps_response);
}

mod zcashd {

    use super::*;
    use zcash_local_net::validator::zcashd::Zcashd;

    mod launch {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn regtest_no_cache() {
            launch_fetch_service::<Zcashd>(&ValidatorKind::Zcashd, None).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zcashd::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() {
            launch_fetch_service::<Zcashd>(
                &ValidatorKind::Zcashd,
                zaino_testutils::ZCASHD_CHAIN_CACHE_DIR.clone(),
            )
            .await;
        }
    }

    mod validation {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn validate_address() {
            fetch_service_validate_address::<Zcashd>(&ValidatorKind::Zcashd).await;
        }
    }

    mod get {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn address_balance() {
            fetch_service_get_address_balance::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_raw() {
            fetch_service_get_block_raw::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_object() {
            fetch_service_get_block_object::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn raw_mempool() {
            fetch_service_get_raw_mempool::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mempool_info() {
            test_get_mempool_info::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        mod z {

            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn get_treestate() {
                fetch_service_z_get_treestate::<Zcashd>(&ValidatorKind::Zcashd).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn subtrees_by_index() {
                fetch_service_z_get_subtrees_by_index::<Zcashd>(&ValidatorKind::Zcashd).await;
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn raw_transaction() {
            fetch_service_get_raw_transaction::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn address_tx_ids() {
            fetch_service_get_address_tx_ids::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn address_utxos() {
            fetch_service_get_address_utxos::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_block() {
            fetch_service_get_latest_block::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block() {
            fetch_service_get_block::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_header() {
            fetch_service_get_block_header::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn difficulty() {
            assert_fetch_service_difficulty_matches_rpc::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[allow(deprecated)]
        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_deltas() {
            let mut test_manager = TestManager::<Zcashd, FetchService>::launch(
                &ValidatorKind::Zcashd,
                None,
                None,
                None,
                true,
                false,
                false,
            )
            .await
            .unwrap();

            let fetch_service_subscriber = test_manager.service_subscriber.take().unwrap();

            let current_block = fetch_service_subscriber.get_latest_block().await.unwrap();

            let block_hash_bytes: [u8; 32] = current_block.hash.as_slice().try_into().unwrap();

            let block_hash = zebra_chain::block::Hash::from(block_hash_bytes);

            // Note: we need an 'expected' block hash in order to query its deltas.
            // Having a predictable or test vector chain is the way to go here.
            let fetch_service_block_deltas = fetch_service_subscriber
                .get_block_deltas(block_hash.to_string())
                .await
                .unwrap();

            let jsonrpc_client = JsonRpSeeConnector::new_with_basic_auth(
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

            let rpc_block_deltas = jsonrpc_client
                .get_block_deltas(block_hash.to_string())
                .await
                .unwrap();

            assert_eq!(fetch_service_block_deltas, rpc_block_deltas);
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mining_info() {
            assert_fetch_service_mininginfo_matches_rpc::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn peer_info() {
            assert_fetch_service_peerinfo_matches_rpc::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_subsidy() {
            fetch_service_get_block_subsidy::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn best_blockhash() {
            fetch_service_get_best_blockhash::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_count() {
            fetch_service_get_block_count::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_nullifiers() {
            fetch_service_get_block_nullifiers::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range() {
            fetch_service_get_block_range::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range_nullifiers() {
            fetch_service_get_block_range_nullifiers::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn transaction_mined() {
            fetch_service_get_transaction_mined::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn transaction_mempool() {
            fetch_service_get_transaction_mempool::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_txids() {
            fetch_service_get_taddress_txids::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_balance() {
            fetch_service_get_taddress_balance::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mempool_tx() {
            fetch_service_get_mempool_tx::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mempool_stream() {
            fetch_service_get_mempool_stream::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn tree_state() {
            fetch_service_get_tree_state::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_tree_state() {
            fetch_service_get_latest_tree_state::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn subtree_roots() {
            fetch_service_get_subtree_roots::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_utxos() {
            fetch_service_get_taddress_utxos::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_utxos_stream() {
            fetch_service_get_taddress_utxos_stream::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn lightd_info() {
            fetch_service_get_lightd_info::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test]
        pub(crate) async fn get_network_sol_ps() {
            assert_fetch_service_getnetworksols_matches_rpc::<Zcashd>(&ValidatorKind::Zcashd).await;
        }
    }
}

mod zebrad {

    use super::*;
    use zcash_local_net::validator::zebrad::Zebrad;

    mod launch {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn regtest_no_cache() {
            launch_fetch_service::<Zebrad>(&ValidatorKind::Zebrad, None).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zebrad::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() {
            launch_fetch_service::<Zebrad>(
                &ValidatorKind::Zebrad,
                zaino_testutils::ZEBRAD_CHAIN_CACHE_DIR.clone(),
            )
            .await;
        }
    }

    mod validation {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn validate_address() {
            fetch_service_validate_address::<Zebrad>(&ValidatorKind::Zebrad).await;
        }
    }

    mod get {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn address_balance() {
            fetch_service_get_address_balance::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_raw() {
            fetch_service_get_block_raw::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_object() {
            fetch_service_get_block_object::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn raw_mempool() {
            fetch_service_get_raw_mempool::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mempool_info() {
            test_get_mempool_info::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        mod z {

            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn treestate() {
                fetch_service_z_get_treestate::<Zebrad>(&ValidatorKind::Zebrad).await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn subtrees_by_index() {
                fetch_service_z_get_subtrees_by_index::<Zebrad>(&ValidatorKind::Zebrad).await;
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn raw_transaction() {
            fetch_service_get_raw_transaction::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn address_tx_ids() {
            fetch_service_get_address_tx_ids::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn address_utxos() {
            fetch_service_get_address_utxos::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_block() {
            fetch_service_get_latest_block::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block() {
            fetch_service_get_block::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_header() {
            fetch_service_get_block_header::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn difficulty() {
            assert_fetch_service_difficulty_matches_rpc::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mining_info() {
            assert_fetch_service_mininginfo_matches_rpc::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn peer_info() {
            assert_fetch_service_peerinfo_matches_rpc::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_subsidy() {
            fetch_service_get_block_subsidy::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn best_blockhash() {
            fetch_service_get_best_blockhash::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_count() {
            fetch_service_get_block_count::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_nullifiers() {
            fetch_service_get_block_nullifiers::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range() {
            fetch_service_get_block_range::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn block_range_nullifiers() {
            fetch_service_get_block_range_nullifiers::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn transaction_mined() {
            fetch_service_get_transaction_mined::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn transaction_mempool() {
            fetch_service_get_transaction_mempool::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_txids() {
            fetch_service_get_taddress_txids::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_balance() {
            fetch_service_get_taddress_balance::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mempool_tx() {
            fetch_service_get_mempool_tx::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn mempool_stream() {
            fetch_service_get_mempool_stream::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn tree_state() {
            fetch_service_get_tree_state::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn latest_tree_state() {
            fetch_service_get_latest_tree_state::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn subtree_roots() {
            fetch_service_get_subtree_roots::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_utxos() {
            fetch_service_get_taddress_utxos::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn taddress_utxos_stream() {
            fetch_service_get_taddress_utxos_stream::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn lightd_info() {
            fetch_service_get_lightd_info::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test]
        pub(crate) async fn get_network_sol_ps() {
            assert_fetch_service_getnetworksols_matches_rpc::<Zebrad>(&ValidatorKind::Zebrad).await;
        }
    }
}
