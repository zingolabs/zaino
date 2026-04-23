use futures::StreamExt;
use zaino_common::network::ActivationHeights;
use zaino_common::{DatabaseConfig, ServiceConfig, StorageConfig};
use zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasParams;
use zaino_proto::proto::service::{BlockId, BlockRange, PoolType, TransparentAddressBlockFilter};
use zaino_state::ChainIndex as _;
use zaino_state::{LightWalletService, ZcashService};

#[allow(deprecated)]
use zaino_state::{
    FetchService, FetchServiceConfig, FetchServiceSubscriber, LightWalletIndexer, StateService,
    StateServiceConfig, StateServiceSubscriber, ZcashIndexer,
};
use zaino_testutils::{from_inputs, ValidatorExt};
use zaino_testutils::{TestManager, ValidatorKind, ZEBRAD_TESTNET_CACHE_DIR};
use zainodlib::config::ZainodConfig;
use zainodlib::error::IndexerError;
use zcash_local_net::validator::{zebrad::Zebrad, Validator};
use zebra_chain::parameters::NetworkKind;
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::methods::{GetAddressBalanceRequest, GetAddressTxIdsRequest, GetInfo};
use zip32::AccountId;

#[allow(deprecated)]
// NOTE: the fetch and state services each have a seperate chain index to the instance of zaino connected to the lightclients and may be out of sync
// the test manager now includes a service subscriber but not both fetch *and* state which are necessary for these tests.
// syncronicity is ensured in the following tests by calling `generate_blocks_and_poll_all_chain_indexes`.
async fn create_test_manager_and_services<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
    network: Option<NetworkKind>,
) -> (
    TestManager<V, StateService>,
    FetchService,
    FetchServiceSubscriber,
    StateService,
    StateServiceSubscriber,
) {
    let test_manager = TestManager::<V, StateService>::launch(
        validator,
        network,
        None,
        chain_cache.clone(),
        enable_zaino,
        false,
        enable_clients,
    )
    .await
    .unwrap();

    let network_type = match network {
        Some(NetworkKind::Mainnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            zaino_common::Network::Mainnet
        }
        Some(NetworkKind::Testnet) => {
            println!("Waiting for validator to spawn..");
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            zaino_common::Network::Testnet
        }
        _ => zaino_common::Network::Regtest({
            let activation_heights = test_manager.local_net.get_activation_heights().await;
            ActivationHeights {
                before_overwinter: activation_heights.overwinter(),
                overwinter: activation_heights.overwinter(),
                sapling: activation_heights.sapling(),
                blossom: activation_heights.blossom(),
                heartwood: activation_heights.heartwood(),
                canopy: activation_heights.canopy(),
                nu5: activation_heights.nu5(),
                nu6: activation_heights.nu6(),
                nu6_1: activation_heights.nu6_1(),
                nu7: activation_heights.nu7(),
            }
        }),
    };

    test_manager.local_net.print_stdout();

    let fetch_service = FetchService::spawn(FetchServiceConfig::new(
        test_manager.full_node_rpc_listen_address.to_string(),
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
        network_type,
        None,
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
            should_backup_non_finalized_state: false,
            debug_skip_non_finalized_state_backup_task: false,
        },
        test_manager.full_node_rpc_listen_address.to_string(),
        test_manager.full_node_grpc_listen_address,
        false,
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
        network_type,
        None,
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

#[allow(deprecated)]
async fn generate_blocks_and_poll_all_chain_indexes<V, Service>(
    n: u32,
    test_manager: &TestManager<V, Service>,
    fetch_service_subscriber: FetchServiceSubscriber,
    state_service_subscriber: StateServiceSubscriber,
) where
    V: ValidatorExt,
    Service: LightWalletService + Send + Sync + 'static,
    Service::Config: TryFrom<ZainodConfig, Error = IndexerError>,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
{
    test_manager.generate_blocks_and_poll(n).await;
    test_manager
        .generate_blocks_and_poll_indexer(0, &fetch_service_subscriber)
        .await;
    test_manager
        .generate_blocks_and_poll_indexer(0, &state_service_subscriber)
        .await;
}
async fn state_service_check_info<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: NetworkKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, chain_cache, false, false, Some(network))
        .await;

    if dbg!(network.to_string()) == *"Regtest" {
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
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
        0,
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
        0,
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

async fn state_service_get_address_balance<V: ValidatorExt>(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    clients.recipient.sync_and_await().await.unwrap();
    let recipient_balance = clients
        .recipient
        .account_balance(zip32::AccountId::ZERO)
        .await
        .unwrap();

    let fetch_service_balance = fetch_service_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();

    let state_service_balance = state_service_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&fetch_service_balance);
    dbg!(&state_service_balance);

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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
    )
    .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request = GetAddressBalanceRequest::new(vec![address.to_string()]);

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
    network: NetworkKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<Zebrad>(
        validator,
        chain_cache,
        false,
        false,
        Some(network),
    )
    .await;

    let height = match network {
        NetworkKind::Regtest => "1".to_string(),
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
    network: NetworkKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<Zebrad>(
        validator,
        chain_cache,
        false,
        false,
        Some(network),
    )
    .await;

    let height = match network {
        NetworkKind::Regtest => "1".to_string(),
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

async fn state_service_get_raw_mempool<V: ValidatorExt>(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
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

/// Tests whether that calls to `get_block_range` with the same block range are the same when
/// specifying the default `PoolType`s and passing and empty Vec to verify that the method falls
/// back to the default pools when these are not explicitly specified.
async fn state_service_get_block_range_returns_default_pools<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    let start_height: u64 = 100;
    let end_height: u64 = 103;

    let default_pools_request = BlockRange {
        start: Some(BlockId {
            height: start_height,
            hash: vec![],
        }),
        end: Some(BlockId {
            height: end_height,
            hash: vec![],
        }),
        pool_types: vec![],
    };

    let fetch_service_get_block_range = fetch_service_subscriber
        .get_block_range(default_pools_request.clone())
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    let explicit_default_pool_request = BlockRange {
        start: Some(BlockId {
            height: start_height,
            hash: vec![],
        }),
        end: Some(BlockId {
            height: end_height,
            hash: vec![],
        }),
        pool_types: vec![PoolType::Sapling as i32, PoolType::Orchard as i32],
    };

    let fetch_service_get_block_range_specifying_pools = fetch_service_subscriber
        .get_block_range(explicit_default_pool_request.clone())
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        fetch_service_get_block_range,
        fetch_service_get_block_range_specifying_pools
    );

    let state_service_get_block_range_specifying_pools = state_service_subscriber
        .get_block_range(explicit_default_pool_request)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    let state_service_get_block_range = state_service_subscriber
        .get_block_range(default_pools_request)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    assert_eq!(
        state_service_get_block_range,
        state_service_get_block_range_specifying_pools
    );

    // check that the block range is the same between fetch service and state service
    assert_eq!(fetch_service_get_block_range, state_service_get_block_range);

    let compact_block = state_service_get_block_range.last().unwrap();

    assert_eq!(compact_block.height, end_height);

    // the compact block has 1 transactions
    assert_eq!(compact_block.vtx.len(), 1);

    let shielded_tx = compact_block.vtx.first().unwrap();
    assert_eq!(shielded_tx.index, 1);
    // tranparent data should not be present when no pool types are requested
    assert_eq!(
        shielded_tx.vin,
        vec![],
        "transparent data should not be present when no pool types are specified in the request."
    );
    assert_eq!(
        shielded_tx.vout,
        vec![],
        "transparent data should not be present when no pool types are specified in the request."
    );
    test_manager.close().await;
}

/// tests whether the `GetBlockRange` RPC returns all pools when requested
async fn state_service_get_block_range_returns_all_pools<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        for _ in 1..4 {
            clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
            generate_blocks_and_poll_all_chain_indexes(
                1,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            clients.faucet.sync_and_await().await.unwrap();
        }
    };

    let recipient_transparent = clients.get_recipient_address("transparent").await;
    let deshielding_txid = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_transparent, 250_000, None)],
    )
    .await
    .unwrap()
    .head;

    let recipient_sapling = clients.get_recipient_address("sapling").await;
    let sapling_txid = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(&recipient_sapling, 250_000, None)],
    )
    .await
    .unwrap()
    .head;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let orchard_txid =
        from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
            .await
            .unwrap()
            .head;

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    let start_height: u64 = 100;
    let end_height: u64 = 106;

    let block_range = BlockRange {
        start: Some(BlockId {
            height: start_height,
            hash: vec![],
        }),
        end: Some(BlockId {
            height: end_height,
            hash: vec![],
        }),
        pool_types: vec![
            PoolType::Transparent as i32,
            PoolType::Sapling as i32,
            PoolType::Orchard as i32,
        ],
    };

    let fetch_service_get_block_range = fetch_service_subscriber
        .get_block_range(block_range.clone())
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    let state_service_get_block_range = state_service_subscriber
        .get_block_range(block_range)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    // check that the block range is the same
    assert_eq!(fetch_service_get_block_range, state_service_get_block_range);

    let compact_block = state_service_get_block_range.last().unwrap();

    assert_eq!(compact_block.height, end_height);

    // the compact block has 4 transactions (3 sent + coinbase)
    assert_eq!(compact_block.vtx.len(), 4);

    // transaction order is not guaranteed so it's necessary to look up for them by TXID
    let deshielding_tx = compact_block
        .vtx
        .iter()
        .find(|tx| tx.txid == deshielding_txid.as_ref().to_vec())
        .unwrap();

    assert!(
        !deshielding_tx.vout.is_empty(),
        "transparent data should be present when transaparent pool type is specified in the request."
    );

    // transaction order is not guaranteed so it's necessary to look up for them by TXID
    let sapling_tx = compact_block
        .vtx
        .iter()
        .find(|tx| tx.txid == sapling_txid.as_ref().to_vec())
        .unwrap();

    assert!(
        !sapling_tx.outputs.is_empty(),
        "sapling data should be present when all pool types are specified in the request."
    );

    let orchard_tx = compact_block
        .vtx
        .iter()
        .find(|tx| tx.txid == orchard_txid.as_ref().to_vec())
        .unwrap();

    assert!(
        !orchard_tx.actions.is_empty(),
        "orchard data should be present when all pool types are specified in the request."
    );

    test_manager.close().await;
}

// tests whether the `GetBlockRange` returns all blocks until the first requested block in the
// range can't be bound
async fn state_service_get_block_range_out_of_range_test_upper_bound<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    // Test manager generates blocks on startup, check current height to ensure we only generate up to height 100
    let chain_height = state_service_subscriber
        .get_latest_block()
        .await
        .unwrap()
        .height as u32;
    let block_required_height_100 = 100 - chain_height;

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            block_required_height_100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let start_height: u64 = 1;
    let end_height: u64 = 106;

    let block_range = BlockRange {
        start: Some(BlockId {
            height: start_height,
            hash: vec![],
        }),
        end: Some(BlockId {
            height: end_height,
            hash: vec![],
        }),
        pool_types: vec![
            PoolType::Transparent as i32,
            PoolType::Sapling as i32,
            PoolType::Orchard as i32,
        ],
    };

    let mut fetch_service_stream = fetch_service_subscriber
        .get_block_range(block_range.clone())
        .await
        .expect("get_block_range call itself should not fail");

    let mut fetch_service_blocks = Vec::new();
    let mut fetch_service_terminal_error = None;

    while let Some(item) = fetch_service_stream.next().await {
        match item {
            Ok(block) => fetch_service_blocks.push(block),
            Err(e) => {
                fetch_service_terminal_error = Some(e);
                break;
            }
        }
    }

    let mut state_service_stream = state_service_subscriber
        .get_block_range(block_range)
        .await
        .expect("get_block_range call itself should not fail");

    let mut state_service_blocks = Vec::new();
    let mut state_service_terminal_error = None;

    while let Some(item) = state_service_stream.next().await {
        match item {
            Ok(block) => state_service_blocks.push(block),
            Err(e) => {
                state_service_terminal_error = Some(e);
                break;
            }
        }
    }
    // check that the block range is the same
    assert_eq!(fetch_service_blocks, state_service_blocks);

    let compact_block = state_service_blocks.last().unwrap();

    assert!(compact_block.height < end_height);

    assert_eq!(fetch_service_blocks.len(), 100);

    // Assert – then an error, not a clean end-of-stream
    let _ = state_service_terminal_error
        .expect("state service stream should terminate with an error, not cleanly");
    let _ = fetch_service_terminal_error
        .expect("fetch service stream should terminate with an error, not cleanly");

    test_manager.close().await;
}

// tests whether the `GetBlockRange` returns all blocks until the first requested block in the
// range can't be bound
async fn state_service_get_block_range_out_of_range_test_lower_bound<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    // Test manager generates blocks on startup, check current height to ensure we only generate up to height 100
    let chain_height = state_service_subscriber
        .get_latest_block()
        .await
        .unwrap()
        .height as u32;
    let block_required_height_100 = 100 - chain_height;

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            block_required_height_100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let start_height: u64 = 106;
    let end_height: u64 = 1;

    let block_range = BlockRange {
        start: Some(BlockId {
            height: start_height,
            hash: vec![],
        }),
        end: Some(BlockId {
            height: end_height,
            hash: vec![],
        }),
        pool_types: vec![
            PoolType::Transparent as i32,
            PoolType::Sapling as i32,
            PoolType::Orchard as i32,
        ],
    };

    let mut fetch_service_stream = fetch_service_subscriber
        .get_block_range(block_range.clone())
        .await
        .expect("get_block_range call itself should not fail");

    let mut fetch_service_blocks = Vec::new();
    let mut fetch_service_terminal_error = None;

    while let Some(item) = fetch_service_stream.next().await {
        match item {
            Ok(block) => fetch_service_blocks.push(block),
            Err(e) => {
                fetch_service_terminal_error = Some(e);
                break;
            }
        }
    }

    let mut state_service_stream = state_service_subscriber
        .get_block_range(block_range)
        .await
        .expect("get_block_range call itself should not fail");

    let mut state_service_blocks = Vec::new();
    let mut state_service_terminal_error = None;

    while let Some(item) = state_service_stream.next().await {
        match item {
            Ok(block) => state_service_blocks.push(block),
            Err(e) => {
                state_service_terminal_error = Some(e);
                break;
            }
        }
    }
    // check that the block range is the same
    assert_eq!(fetch_service_blocks, state_service_blocks);

    assert!(fetch_service_blocks.is_empty());

    // Assert – then an error, not a clean end-of-stream
    let _ = state_service_terminal_error
        .expect("state service stream should terminate with an error, not cleanly");
    let _ = fetch_service_terminal_error
        .expect("fetch service stream should terminate with an error, not cleanly");
    // assert!(
    //     matches!(err, ZainoStateError::BlockOutOfRange { .. }),
    //     "unexpected error variant: {err:?}"
    // );

    test_manager.close().await;
}

async fn state_service_z_get_treestate<V: ValidatorExt>(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    let chain_height = dbg!(state_service_subscriber.chain_height().await.unwrap()).0;

    let fetch_service_treestate = dbg!(fetch_service_subscriber
        .z_get_treestate(chain_height.to_string())
        .await
        .unwrap());

    let state_service_treestate = dbg!(state_service_subscriber
        .z_get_treestate(chain_height.to_string())
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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
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

async fn state_service_z_get_subtrees_by_index<V: ValidatorExt>(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
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

use zcash_local_net::logs::LogsToStdoutAndStderr;
async fn state_service_get_raw_transaction<V: ValidatorExt + LogsToStdoutAndStderr>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let recipient_ua = clients.get_recipient_address("unified").await.to_string();
    let tx = from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await
        .unwrap();

    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
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

async fn state_service_get_address_transactions_regtest<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.local_net.generate_blocks(100).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
        clients.faucet.sync_and_await().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        test_manager.local_net.generate_blocks(1).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        clients.faucet.sync_and_await().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };

    let tx = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    test_manager.local_net.generate_blocks(1).await.unwrap();
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let chain_height: u32 = fetch_service_subscriber
        .indexer
        .snapshot_nonfinalized_state()
        .best_tip
        .height
        .into();
    dbg!(&chain_height);

    let state_service_txids = state_service_subscriber
        .get_taddress_transactions(TransparentAddressBlockFilter {
            address: recipient_taddr,
            range: Some(BlockRange {
                start: Some(BlockId {
                    height: (chain_height - 2) as u64,
                    hash: vec![],
                }),
                end: Some(BlockId {
                    height: chain_height as u64,
                    hash: vec![],
                }),
                pool_types: vec![
                    PoolType::Transparent as i32,
                    PoolType::Sapling as i32,
                    PoolType::Orchard as i32,
                ],
            }),
        })
        .await
        .unwrap();

    dbg!(&tx);

    dbg!(&state_service_txids);
    assert!(state_service_txids.count().await > 0);

    test_manager.close().await;
}
async fn state_service_get_address_tx_ids<V: ValidatorExt>(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let tx = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    let chain_height = fetch_service_subscriber
        .indexer
        .snapshot_nonfinalized_state()
        .best_tip
        .height
        .into();

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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
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

async fn state_service_get_address_utxos<V: ValidatorExt>(validator: &ValidatorKind) {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<V>(validator, None, true, true, None).await;

    let mut clients = test_manager
        .clients
        .take()
        .expect("Clients are not initialized");
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.faucet.sync_and_await().await.unwrap();

    if matches!(validator, ValidatorKind::Zebrad) {
        generate_blocks_and_poll_all_chain_indexes(
            100,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
        generate_blocks_and_poll_all_chain_indexes(
            1,
            &test_manager,
            fetch_service_subscriber.clone(),
            state_service_subscriber.clone(),
        )
        .await;
        clients.faucet.sync_and_await().await.unwrap();
    };

    let txid_1 = from_inputs::quick_send(
        &mut clients.faucet,
        vec![(recipient_taddr.as_str(), 250_000, None)],
    )
    .await
    .unwrap();
    generate_blocks_and_poll_all_chain_indexes(
        1,
        &test_manager,
        fetch_service_subscriber.clone(),
        state_service_subscriber.clone(),
    )
    .await;

    clients.faucet.sync_and_await().await.unwrap();

    let fetch_service_utxos = fetch_service_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    let state_service_utxos = state_service_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr]))
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
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
    )
    .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    let address_request = GetAddressBalanceRequest::new(vec![address.to_string()]);

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

async fn state_service_get_address_deltas_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = create_test_manager_and_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        false,
        Some(NetworkKind::Testnet),
    )
    .await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    // Test simple response
    let simple_request =
        GetAddressDeltasParams::new_filtered(vec![address.to_string()], 2000000, 3000000, false);

    let fetch_service_simple_deltas = dbg!(
        fetch_service_subscriber
            .get_address_deltas(simple_request.clone())
            .await
    )
    .unwrap();

    let state_service_simple_deltas = dbg!(
        state_service_subscriber
            .get_address_deltas(simple_request)
            .await
    )
    .unwrap();

    assert_eq!(fetch_service_simple_deltas, state_service_simple_deltas);

    // Test response with chain info
    let chain_info_params =
        GetAddressDeltasParams::new_filtered(vec![address.to_string()], 2000000, 3000000, true);

    let fetch_service_chain_info_deltas = dbg!(
        fetch_service_subscriber
            .get_address_deltas(chain_info_params.clone())
            .await
    )
    .unwrap();

    let state_service_chain_info_deltas = dbg!(
        state_service_subscriber
            .get_address_deltas(chain_info_params)
            .await
    )
    .unwrap();

    assert_eq!(
        fetch_service_chain_info_deltas,
        state_service_chain_info_deltas
    );

    test_manager.close().await;
}

mod zebra {

    use super::*;

    pub(crate) mod check_info {

        use super::*;
        use zaino_testutils::ZEBRAD_CHAIN_CACHE_DIR;
        use zcash_local_net::validator::zebrad::Zebrad;

        #[tokio::test(flavor = "multi_thread")]
        async fn regtest_no_cache() {
            state_service_check_info::<Zebrad>(&ValidatorKind::Zebrad, None, NetworkKind::Regtest)
                .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn state_service_chaintip_update_subscriber() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            let mut chaintip_subscriber = state_service_subscriber.chaintip_update_subscriber();
            for _ in 0..5 {
                generate_blocks_and_poll_all_chain_indexes(
                    1,
                    &test_manager,
                    fetch_service_subscriber.clone(),
                    state_service_subscriber.clone(),
                )
                .await;
                assert_eq!(
                    chaintip_subscriber.next_tip_hash().await.unwrap().0,
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

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zcashd::check_info::regtest_no_cache."]
        async fn regtest_with_cache() {
            state_service_check_info::<Zebrad>(
                &ValidatorKind::Zebrad,
                ZEBRAD_CHAIN_CACHE_DIR.clone(),
                NetworkKind::Regtest,
            )
            .await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn testnet() {
            state_service_check_info::<Zebrad>(
                &ValidatorKind::Zebrad,
                ZEBRAD_TESTNET_CACHE_DIR.clone(),
                NetworkKind::Testnet,
            )
            .await;
        }
    }

    pub(crate) mod get {

        use super::*;
        use zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasResponse;
        use zcash_local_net::validator::zebrad::Zebrad;

        #[tokio::test(flavor = "multi_thread")]
        async fn address_utxos() {
            state_service_get_address_utxos::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_utxos_testnet() {
            state_service_get_address_utxos_testnet().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn taddress_transactions_regtest() {
            state_service_get_address_transactions_regtest::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn address_tx_ids_regtest() {
            state_service_get_address_tx_ids::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_tx_ids_testnet() {
            state_service_get_address_tx_ids_testnet().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn raw_transaction_regtest() {
            state_service_get_raw_transaction::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn raw_transaction_testnet() {
            state_service_get_raw_transaction_testnet().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn best_blockhash() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let fetch_service_bbh =
                dbg!(fetch_service_subscriber.get_best_blockhash().await.unwrap());
            let state_service_bbh =
                dbg!(state_service_subscriber.get_best_blockhash().await.unwrap());
            assert_eq!(fetch_service_bbh, state_service_bbh);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_count() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let fetch_service_block_count =
                dbg!(fetch_service_subscriber.get_block_count().await.unwrap());
            let state_service_block_count =
                dbg!(state_service_subscriber.get_block_count().await.unwrap());
            assert_eq!(fetch_service_block_count, state_service_block_count);

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn mining_info() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                false,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;

            let initial_fetch_service_mining_info =
                fetch_service_subscriber.get_mining_info().await.unwrap();
            let initial_state_service_mining_info =
                state_service_subscriber.get_mining_info().await.unwrap();
            assert_eq!(
                initial_fetch_service_mining_info,
                initial_state_service_mining_info
            );

            test_manager.local_net.generate_blocks(2).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            let final_fetch_service_mining_info =
                fetch_service_subscriber.get_mining_info().await.unwrap();
            let final_state_service_mining_info =
                state_service_subscriber.get_mining_info().await.unwrap();

            assert_eq!(
                final_fetch_service_mining_info,
                final_state_service_mining_info
            );

            test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn difficulty() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
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

            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_network_sol_ps() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;

            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let initial_fetch_service_get_network_sol_ps = fetch_service_subscriber
                .get_network_sol_ps(None, None)
                .await
                .unwrap();
            let initial_state_service_get_network_sol_ps = state_service_subscriber
                .get_network_sol_ps(None, None)
                .await
                .unwrap();
            assert_eq!(
                initial_fetch_service_get_network_sol_ps,
                initial_state_service_get_network_sol_ps
            );

            test_manager.close().await;
        }

        /// A proper test would boot up multiple nodes at the same time, and ask each node
        /// for information about its peers. In the current state, this test does nothing.
        #[tokio::test(flavor = "multi_thread")]
        async fn peer_info() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;

            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let fetch_service_peer_info = fetch_service_subscriber.get_peer_info().await.unwrap();
            let state_service_peer_info = state_service_subscriber.get_peer_info().await.unwrap();
            assert_eq!(fetch_service_peer_info, state_service_peer_info);

            test_manager.close().await;
        }

        mod z {
            use zcash_local_net::validator::zebrad::Zebrad;

            use super::*;

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn get_block_range_default_request_returns_no_t_data_regtest() {
                state_service_get_block_range_returns_default_pools::<Zebrad>(
                    &ValidatorKind::Zebrad,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn get_block_range_default_request_returns_all_pools_regtest() {
                state_service_get_block_range_returns_all_pools::<Zebrad>(&ValidatorKind::Zebrad)
                    .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn get_block_range_out_of_range_test_upper_bound_regtest() {
                state_service_get_block_range_out_of_range_test_upper_bound::<Zebrad>(
                    &ValidatorKind::Zebrad,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn get_block_range_out_of_range_test_lower_bound_regtest() {
                state_service_get_block_range_out_of_range_test_lower_bound::<Zebrad>(
                    &ValidatorKind::Zebrad,
                )
                .await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn subtrees_by_index_regtest() {
                state_service_z_get_subtrees_by_index::<Zebrad>(&ValidatorKind::Zebrad).await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn subtrees_by_index_testnet() {
                state_service_z_get_subtrees_by_index_testnet().await;
            }

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn treestate_regtest() {
                state_service_z_get_treestate::<Zebrad>(&ValidatorKind::Zebrad).await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn treestate_testnet() {
                state_service_z_get_treestate_testnet().await;
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn raw_mempool_regtest() {
            state_service_get_raw_mempool::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        /// `getmempoolinfo` computed from local Broadcast state
        #[tokio::test(flavor = "multi_thread")]
        #[allow(deprecated)]
        async fn get_mempool_info() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                None,
            )
            .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            let recipient_taddr = clients.get_recipient_address("transparent").await;

            clients.faucet.sync_and_await().await.unwrap();

            generate_blocks_and_poll_all_chain_indexes(
                100,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;
            clients.faucet.sync_and_await().await.unwrap();
            clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();
            generate_blocks_and_poll_all_chain_indexes(
                1,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;
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
                .map(|(_, v)| v.serialized_tx.as_ref().as_ref().len() as u64)
                .sum();

            let expected_key_heap_bytes: u64 =
                entries.iter().map(|(k, _)| k.txid.capacity() as u64).sum();

            let expected_usage = expected_bytes.saturating_add(expected_key_heap_bytes);

            assert!(info.bytes > 0);
            assert_eq!(info.bytes, expected_bytes);

            assert!(info.usage >= info.bytes);
            assert_eq!(info.usage, expected_usage);

            // Optional: when exactly one tx, its serialized length must equal `bytes`
            if info.size == 1 {
                let (_, mem_value) = entries[0].clone();
                assert_eq!(
                    mem_value.serialized_tx.as_ref().as_ref().len() as u64,
                    expected_bytes
                );
            }

            test_manager.close().await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn raw_mempool_testnet() {
            state_service_get_raw_mempool_testnet().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_object_regtest() {
            state_service_get_block_object(&ValidatorKind::Zebrad, None, NetworkKind::Regtest)
                .await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_object_testnet() {
            state_service_get_block_object(
                &ValidatorKind::Zebrad,
                ZEBRAD_TESTNET_CACHE_DIR.clone(),
                NetworkKind::Testnet,
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_raw_regtest() {
            state_service_get_block_raw(&ValidatorKind::Zebrad, None, NetworkKind::Regtest).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn block_raw_testnet() {
            state_service_get_block_raw(
                &ValidatorKind::Zebrad,
                ZEBRAD_TESTNET_CACHE_DIR.clone(),
                NetworkKind::Testnet,
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn address_balance_regtest() {
            state_service_get_address_balance::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_balance_testnet() {
            state_service_get_address_balance_testnet().await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test]
        async fn address_deltas_testnet() {
            state_service_get_address_deltas_testnet().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn address_deltas() {
            address_deltas::main().await;
        }

        mod address_deltas;
    }

    pub(crate) mod lightwallet_indexer {
        use futures::StreamExt as _;
        use zaino_proto::proto::{
            service::{
                AddressList, BlockId, BlockRange, GetAddressUtxosArg, GetSubtreeRootsArg, PoolType,
                TxFilter,
            },
            utils::pool_types_into_i32_vec,
        };
        use zebra_rpc::methods::{GetAddressTxIdsRequest, GetBlock};

        use super::*;
        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_block() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                1,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let fetch_service_block =
                dbg!(fetch_service_subscriber.get_latest_block().await.unwrap());
            let state_service_block =
                dbg!(state_service_subscriber.get_latest_block().await.unwrap());
            assert_eq!(fetch_service_block, state_service_block);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;

            const BLOCK_LIMIT: u32 = 10;

            for i in 0..BLOCK_LIMIT {
                generate_blocks_and_poll_all_chain_indexes(
                    1,
                    &test_manager,
                    fetch_service_subscriber.clone(),
                    state_service_subscriber.clone(),
                )
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

                let state_service_block_header_response = state_service_subscriber
                    .get_block_header(block_hash.to_string(), false)
                    .await
                    .unwrap();
                assert_eq!(
                    fetch_service_get_block_header,
                    state_service_block_header_response
                );
            }
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_tree_state() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let chain_height = dbg!(state_service_subscriber.chain_height().await.unwrap()).0;

            let second_treestate_by_height = BlockId {
                height: chain_height as u64,
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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_subtree_roots() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                5,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let sapling_subtree_roots_request = GetSubtreeRootsArg {
                start_index: 2,
                shielded_protocol: 0,
                max_entries: 0,
            };
            let fetch_service_sapling_subtree_roots = fetch_service_subscriber
                .get_subtree_roots(sapling_subtree_roots_request)
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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_tree_state() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

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
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                false,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                6,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let start = Some(BlockId {
                height: 2,
                hash: vec![],
            });
            let end = Some(BlockId {
                height: 5,
                hash: vec![],
            });
            let request = BlockRange {
                start,
                end,
                pool_types: vec![
                    PoolType::Transparent as i32,
                    PoolType::Sapling as i32,
                    PoolType::Orchard as i32,
                ],
            };
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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_full() {
            get_block_range_helper(false).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_range_nullifiers() {
            get_block_range_helper(true).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            generate_blocks_and_poll_all_chain_indexes(
                100,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            clients.faucet.sync_and_await().await.unwrap();
            clients.faucet.quick_shield(AccountId::ZERO).await.unwrap();

            generate_blocks_and_poll_all_chain_indexes(
                2,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

            let block = BlockId {
                height: 103,
                hash: vec![],
            };
            let state_service_block_by_height = state_service_subscriber
                .get_block(block.clone())
                .await
                .unwrap();
            let coinbase_tx = state_service_block_by_height.vtx.first().unwrap();
            let hash = coinbase_tx.txid.clone();
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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_txids() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            let clients = test_manager.clients.take().unwrap();
            let taddr = clients.get_faucet_address("transparent").await;
            generate_blocks_and_poll_all_chain_indexes(
                100,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos_stream() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            let taddr = clients.get_faucet_address("transparent").await;
            generate_blocks_and_poll_all_chain_indexes(
                5,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;
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
                    .transaction_summaries(false)
                    .await
                    .unwrap()
                    .txids()[1]
                    .as_ref()
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            let mut clients = test_manager
                .clients
                .take()
                .expect("Clients are not initialized");
            let taddr = clients.get_faucet_address("transparent").await;
            generate_blocks_and_poll_all_chain_indexes(
                5,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;
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
                    .transaction_summaries(false)
                    .await
                    .unwrap()
                    .txids()[1]
                    .as_ref()
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_balance() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            let clients = test_manager.clients.take().unwrap();
            let taddr = clients.get_faucet_address("transparent").await;
            generate_blocks_and_poll_all_chain_indexes(
                5,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

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

        #[tokio::test(flavor = "multi_thread")]
        async fn get_transparent_data_from_compact_block_when_requested() {
            let (
                mut test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            let clients = test_manager.clients.take().unwrap();
            let taddr = clients.get_faucet_address("transparent").await;
            generate_blocks_and_poll_all_chain_indexes(
                5,
                &test_manager,
                fetch_service_subscriber.clone(),
                state_service_subscriber.clone(),
            )
            .await;

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

            let chain_height = state_service_subscriber
                .get_latest_block()
                .await
                .unwrap()
                .height;

            // NOTE / TODO: Zaino can not currently serve non standard script types in compact blocks,
            // because of this it does not return the script pub key for the coinbase transaction of the
            // genesis block. We should decide whether / how to fix this.
            //
            // For this reason this test currently does not fetch the genesis block.
            //
            // Issue: https://github.com/zingolabs/zaino/issues/818
            //
            // To see bug update start height of get_block_range to 0.
            let compact_block_range = state_service_subscriber
                .get_block_range(BlockRange {
                    start: Some(BlockId {
                        height: 1,
                        hash: Vec::new(),
                    }),
                    end: Some(BlockId {
                        height: chain_height,
                        hash: Vec::new(),
                    }),
                    pool_types: pool_types_into_i32_vec(
                        [PoolType::Transparent, PoolType::Sapling, PoolType::Orchard].to_vec(),
                    ),
                })
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .await;

            for cb in compact_block_range.into_iter() {
                for tx in cb.vtx {
                    dbg!(&tx);
                    // script pub key of this transaction is not empty
                    assert!(!tx.vout.first().unwrap().script_pub_key.is_empty());
                }
            }
        }
    }
}
