//! Tests that compare the output of both `zcashd` and `zainod` through `FetchService`.

use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, ServiceConfig, StorageConfig};

#[allow(deprecated)]
use zaino_state::{
    BackendType, FetchService, FetchServiceConfig, FetchServiceSubscriber, ZcashIndexer,
    ZcashService as _,
};
use zaino_testutils::{from_inputs, Validator as _};
use zaino_testutils::{TestManager, ValidatorKind};
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::methods::{AddressStrings, GetAddressTxIdsRequest, GetInfo};

#[allow(deprecated)]
async fn create_test_manager_and_fetch_services(
    clients: bool,
) -> (
    TestManager<FetchService>,
    FetchService,
    FetchServiceSubscriber,
    FetchService,
    FetchServiceSubscriber,
) {
    println!("Launching test manager..");
    let test_manager = TestManager::<FetchService>::launch(
        &ValidatorKind::Zcashd,
        &BackendType::Fetch,
        None,
        None,
        None,
        true,
        true,
        clients,
    )
    .await
    .unwrap();

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Launching zcashd fetch service..");
    let zcashd_fetch_service = FetchService::spawn(FetchServiceConfig::new(
        test_manager.full_node_rpc_listen_address,
        None,
        None,
        None,
        ServiceConfig::default(),
        StorageConfig {
            database: DatabaseConfig {
                path: test_manager
                    .local_net
                    .data_dir()
                    .path()
                    .to_path_buf()
                    .join("zaino"),
                ..Default::default()
            },
            ..Default::default()
        },
        zaino_common::Network::Regtest(ActivationHeights::default()),
    ))
    .await
    .unwrap();
    let zcashd_subscriber = zcashd_fetch_service.get_subscriber().inner();

    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    println!("Launching zaino fetch service..");
    let zaino_fetch_service = FetchService::spawn(FetchServiceConfig::new(
        test_manager.full_node_rpc_listen_address,
        test_manager.json_server_cookie_dir.clone(),
        None,
        None,
        ServiceConfig::default(),
        StorageConfig {
            database: DatabaseConfig {
                path: test_manager
                    .local_net
                    .data_dir()
                    .path()
                    .to_path_buf()
                    .join("zaino"),
                ..Default::default()
            },
            ..Default::default()
        },
        zaino_common::Network::Regtest(ActivationHeights::default()),
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

#[allow(deprecated)]
async fn generate_blocks_and_poll_all_chain_indexes(
    n: u32,
    test_manager: &TestManager<FetchService>,
    zaino_subscriber: FetchServiceSubscriber,
    zcashd_subscriber: FetchServiceSubscriber,
) {
    test_manager.generate_blocks_and_poll(n).await;
    test_manager
        .generate_blocks_and_poll_indexer(0, &zaino_subscriber)
        .await;
    test_manager
        .generate_blocks_and_poll_indexer(0, &zcashd_subscriber)
        .await;
}

async fn launch_json_server_check_info() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false).await;
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
    let cleaned_zcashd_info = GetInfo::new(
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
    let cleaned_zaino_info = GetInfo::new(
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

async fn get_best_blockhash_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false).await;

    let zcashd_bbh = dbg!(zcashd_subscriber.get_best_blockhash().await.unwrap());
    let zaino_bbh = dbg!(zaino_subscriber.get_best_blockhash().await.unwrap());

    assert_eq!(zcashd_bbh, zaino_bbh);

    test_manager.close().await;
}

async fn get_block_count_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false).await;

    let zcashd_block_count = dbg!(zcashd_subscriber.get_block_count().await.unwrap());
    let zaino_block_count = dbg!(zaino_subscriber.get_block_count().await.unwrap());

    assert_eq!(zcashd_block_count, zaino_block_count);

    test_manager.close().await;
}

async fn validate_address_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false).await;

    // Using a testnet transparent address
    let address_string = "tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb";

    let address_with_script = "t3TAfQ9eYmXWGe3oPae1XKhdTxm8JvsnFRL";

    let zcashd_valid = zcashd_subscriber
        .validate_address(address_string.to_string())
        .await
        .unwrap();

    let zaino_valid = zaino_subscriber
        .validate_address(address_string.to_string())
        .await
        .unwrap();

    assert_eq!(zcashd_valid, zaino_valid, "Address should be valid");

    let zcashd_valid_script = zcashd_subscriber
        .validate_address(address_with_script.to_string())
        .await
        .unwrap();

    let zaino_valid_script = zaino_subscriber
        .validate_address(address_with_script.to_string())
        .await
        .unwrap();

    assert_eq!(
        zcashd_valid_script, zaino_valid_script,
        "Address should be valid"
    );

    test_manager.close().await;
}

async fn z_get_address_balance_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(true).await;

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
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

    clients.recipient.sync_and_await().await.unwrap();
    let recipient_balance = clients
        .recipient
        .account_balance(zip32::AccountId::ZERO)
        .await
        .unwrap();

    let zcashd_service_balance = zcashd_subscriber
        .z_get_address_balance(AddressStrings::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();

    let zaino_service_balance = zaino_subscriber
        .z_get_address_balance(AddressStrings::new(vec![recipient_taddr]))
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&zcashd_service_balance);
    dbg!(&zaino_service_balance);

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
        zcashd_service_balance.balance(),
    );
    assert_eq!(zcashd_service_balance, zaino_service_balance);

    test_manager.close().await;
}

async fn z_get_block_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(false).await;

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
        zebra_rpc::methods::GetBlock::Object(obj) => obj.hash().to_string(),
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
        create_test_manager_and_fetch_services(true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

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

async fn get_mempool_info_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

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

    let zcashd_subscriber_mempool_info = zcashd_subscriber.get_mempool_info().await.unwrap();
    let zaino_subscriber_mempool_info = zaino_subscriber.get_mempool_info().await.unwrap();

    assert_eq!(
        zcashd_subscriber_mempool_info,
        zaino_subscriber_mempool_info
    );

    test_manager.close().await;
}

async fn z_get_treestate_inner() {
    let (mut test_manager, _zcashd_service, zcashd_subscriber, _zaino_service, zaino_subscriber) =
        create_test_manager_and_fetch_services(true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

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
        create_test_manager_and_fetch_services(true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

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
        create_test_manager_and_fetch_services(true).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");

    clients.faucet.sync_and_await().await.unwrap();

    let recipient_ua = &clients.get_recipient_address("unified").await;
    let tx = from_inputs::quick_send(&mut clients.faucet, vec![(recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

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
        create_test_manager_and_fetch_services(true).await;

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
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

    let chain_height = zcashd_subscriber
        .block_cache
        .get_chain_height()
        .await
        .unwrap()
        .0;
    dbg!(&chain_height);

    let zcashd_txids = zcashd_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr.clone()],
            Some(chain_height - 2),
            Some(chain_height),
        ))
        .await
        .unwrap();

    let zaino_txids = zaino_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr],
            Some(chain_height - 2),
            Some(chain_height),
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
        create_test_manager_and_fetch_services(true).await;

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
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        zaino_subscriber.clone(),
        zcashd_subscriber.clone(),
    )
    .await;

    clients.faucet.sync_and_await().await.unwrap();

    let zcashd_utxos = zcashd_subscriber
        .z_get_address_utxos(AddressStrings::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, zcashd_txid, ..) = zcashd_utxos[0].into_parts();

    let zaino_utxos = zaino_subscriber
        .z_get_address_utxos(AddressStrings::new(vec![recipient_taddr]))
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

// TODO: This module should not be called `zcashd`
mod zcashd {
    use super::*;

    pub(crate) mod zcash_indexer {
        use zaino_state::LightWalletIndexer;
        use zebra_rpc::methods::GetBlock;

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn check_info_no_cookie() {
            launch_json_server_check_info().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn check_info_with_cookie() {
            launch_json_server_check_info().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_address_balance() {
            z_get_address_balance_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_best_blockhash() {
            get_best_blockhash_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_count() {
            get_block_count_inner().await;
        }

        /// Checks that the difficulty is the same between zcashd and zaino.
        ///
        /// This tests generates blocks and checks that the difficulty is the same between zcashd and zaino
        /// after each block is generated.
        #[tokio::test(flavor = "multi_thread")]
        async fn get_difficulty() {
            let (
                mut test_manager,
                _zcashd_service,
                zcashd_subscriber,
                _zaino_service,
                zaino_subscriber,
            ) = create_test_manager_and_fetch_services(false).await;

            const BLOCK_LIMIT: i32 = 10;

            for _ in 0..BLOCK_LIMIT {
                let zcashd_difficulty = zcashd_subscriber.get_difficulty().await.unwrap();
                let zaino_difficulty = zaino_subscriber.get_difficulty().await.unwrap();

                assert_eq!(zcashd_difficulty, zaino_difficulty);

                generate_blocks_and_poll_all_chain_indexes(
                    1,
                    &test_manager,
                    zaino_subscriber.clone(),
                    zcashd_subscriber.clone(),
                )
                .await;
            }

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_deltas() {
            let (
                mut test_manager,
                _zcashd_service,
                zcashd_subscriber,
                _zaino_service,
                zaino_subscriber,
            ) = create_test_manager_and_fetch_services(false).await;

            const BLOCK_LIMIT: i32 = 10;

            for _ in 0..BLOCK_LIMIT {
                let current_block = zcashd_subscriber.get_latest_block().await.unwrap();

                let block_hash_bytes: [u8; 32] = current_block.hash.as_slice().try_into().unwrap();

                let block_hash = zebra_chain::block::Hash::from(block_hash_bytes);

                let zcashd_deltas = zcashd_subscriber
                    .get_block_deltas(block_hash.to_string())
                    .await
                    .unwrap();
                let zaino_deltas = zaino_subscriber
                    .get_block_deltas(block_hash.to_string())
                    .await
                    .unwrap();

                assert_eq!(zcashd_deltas, zaino_deltas);

                test_manager.local_net.generate_blocks(1).await.unwrap();
            }

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mining_info() {
            let (
                mut test_manager,
                _zcashd_service,
                zcashd_subscriber,
                _zaino_service,
                zaino_subscriber,
            ) = create_test_manager_and_fetch_services(false).await;

            const BLOCK_LIMIT: i32 = 10;

            for _ in 0..BLOCK_LIMIT {
                let zcashd_mining_info = zcashd_subscriber.get_mining_info().await.unwrap();
                let zaino_mining_info = zaino_subscriber.get_mining_info().await.unwrap();

                assert_eq!(zcashd_mining_info, zaino_mining_info);

                test_manager.local_net.generate_blocks(1).await.unwrap();
            }

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_peer_info() {
            let (
                mut test_manager,
                _zcashd_service,
                zcashd_subscriber,
                _zaino_service,
                zaino_subscriber,
            ) = create_test_manager_and_fetch_services(false).await;

            let zcashd_peer_info = zcashd_subscriber.get_peer_info().await.unwrap();
            let zaino_peer_info = zaino_subscriber.get_peer_info().await.unwrap();

            assert_eq!(zcashd_peer_info, zaino_peer_info);

            generate_blocks_and_poll_all_chain_indexes(
                1,
                &test_manager,
                zaino_subscriber.clone(),
                zcashd_subscriber.clone(),
            )
            .await;

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_subsidy() {
            let (
                mut test_manager,
                _zcashd_service,
                zcashd_subscriber,
                _zaino_service,
                zaino_subscriber,
            ) = create_test_manager_and_fetch_services(false).await;

            generate_blocks_and_poll_all_chain_indexes(
                1,
                &test_manager,
                zaino_subscriber.clone(),
                zcashd_subscriber.clone(),
            )
            .await;

            let zcashd_block_subsidy = zcashd_subscriber.get_block_subsidy(1).await.unwrap();
            let zaino_block_subsidy = zaino_subscriber.get_block_subsidy(1).await.unwrap();

            assert_eq!(zcashd_block_subsidy, zaino_block_subsidy);

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn validate_address() {
            validate_address_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_block() {
            z_get_block_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() {
            let (
                test_manager,
                _zcashd_service,
                zcashd_subscriber,
                _zaino_service,
                zaino_subscriber,
            ) = create_test_manager_and_fetch_services(false).await;

            const BLOCK_LIMIT: u32 = 10;

            for i in 0..BLOCK_LIMIT {
                test_manager.local_net.generate_blocks(1).await.unwrap();
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

                let block = zcashd_subscriber
                    .z_get_block(i.to_string(), Some(1))
                    .await
                    .unwrap();

                let block_hash = match block {
                    GetBlock::Object(block) => block.hash(),
                    GetBlock::Raw(_) => panic!("Expected block object"),
                };

                let zcashd_get_block_header = zcashd_subscriber
                    .get_block_header(block_hash.to_string(), false)
                    .await
                    .unwrap();

                let zainod_block_header_response = zaino_subscriber
                    .get_block_header(block_hash.to_string(), false)
                    .await
                    .unwrap();
                assert_eq!(zcashd_get_block_header, zainod_block_header_response);
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_mempool() {
            get_raw_mempool_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_info() {
            get_mempool_info_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_treestate() {
            z_get_treestate_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_subtrees_by_index() {
            z_get_subtrees_by_index_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_transaction() {
            get_raw_transaction_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_tx_ids() {
            get_address_tx_ids_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_address_utxos() {
            z_get_address_utxos_inner().await;
        }
    }
}
