use zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasParams;

use zaino_state::{LightWalletIndexer, ZcashIndexer};
use zaino_testutils::ValidatorExt;
use zaino_testutils::{ValidatorKind, ZEBRAD_TESTNET_CACHE_DIR};
use zcash_local_net::validator::{zebrad::Zebrad, Validator};
use zebra_chain::parameters::NetworkKind;
use zebra_rpc::methods::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

#[allow(deprecated)]
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
    ) = zaino_testutils::launch_state_and_fetch_services::<V>(
        validator,
        chain_cache,
        false,
        Some(network),
    )
    .await;

    if dbg!(network.to_string()) == *"Regtest" {
        test_manager
            .generate_blocks_and_wait_for_tips(
                1,
                &fetch_service_subscriber,
                &state_service_subscriber,
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
    let cleaned_fetch_info = zaino_testutils::get_info_with_zeroed_timestamp(fetch_service_info);

    let cleaned_state_info = zaino_testutils::get_info_with_zeroed_timestamp(state_service_info);

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

async fn state_service_get_address_balance_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        validator,
        chain_cache,
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
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        validator,
        chain_cache,
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

async fn state_service_get_raw_mempool_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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

async fn state_service_z_get_treestate_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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

async fn state_service_z_get_subtrees_by_index_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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

async fn state_service_get_raw_transaction_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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

async fn state_service_get_address_tx_ids_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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

async fn state_service_get_address_utxos_testnet() {
    let (
        mut test_manager,
        _fetch_service,
        fetch_service_subscriber,
        _state_service,
        state_service_subscriber,
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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
    ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            let mut chaintip_subscriber = state_service_subscriber.chaintip_update_subscriber();
            for _ in 0..5 {
                test_manager
                    .generate_blocks_and_wait_for_tips(
                        1,
                        &fetch_service_subscriber,
                        &state_service_subscriber,
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
        use zcash_local_net::validator::zebrad::Zebrad;

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_utxos_testnet() {
            state_service_get_address_utxos_testnet().await;
        }

        #[ignore = "requires fully synced testnet."]
        #[tokio::test(flavor = "multi_thread")]
        async fn address_tx_ids_testnet() {
            state_service_get_address_tx_ids_testnet().await;
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
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

            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
                )
                .await;

            let fetch_service_peer_info = fetch_service_subscriber.get_peer_info().await.unwrap();
            let state_service_peer_info = state_service_subscriber.get_peer_info().await.unwrap();
            assert_eq!(fetch_service_peer_info, state_service_peer_info);

            test_manager.close().await;
        }

        mod z {
            use super::*;

            #[allow(deprecated)]
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            pub(crate) async fn z_validate_address() {
                let (
                    mut test_manager,
                    _fetch_service,
                    _fetch_service_subscriber,
                    _state_service,
                    state_service_subscriber,
                ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                    &ValidatorKind::Zebrad,
                    None,
                    true,
                    None,
                )
                .await;

                walletless_tests::rpc::z_validate_address::run_z_validate_for(
                    &state_service_subscriber,
                    walletless_tests::rpc::z_validate_address::SaplingSuite::Standard,
                )
                .await;

                test_manager.close().await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn subtrees_by_index_testnet() {
                state_service_z_get_subtrees_by_index_testnet().await;
            }

            #[ignore = "requires fully synced testnet."]
            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn treestate_testnet() {
                state_service_z_get_treestate_testnet().await;
            }
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
    }

    pub(crate) mod lightwallet_indexer {
        use futures::StreamExt as _;
        use zaino_proto::proto::service::{BlockId, BlockRange, GetSubtreeRootsArg, PoolType};
        use zebra_rpc::methods::GetBlock;

        use super::*;
        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_block() {
            let (
                test_manager,
                _fetch_service,
                fetch_service_subscriber,
                _state_service,
                state_service_subscriber,
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    1,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            const BLOCK_LIMIT: u32 = 10;

            for i in 0..BLOCK_LIMIT {
                test_manager
                    .generate_blocks_and_wait_for_tips(
                        1,
                        &fetch_service_subscriber,
                        &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    5,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    2,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
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
            ) = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            test_manager
                .generate_blocks_and_wait_for_tips(
                    6,
                    &fetch_service_subscriber,
                    &state_service_subscriber,
                )
                .await;

            let start_height: u64 = 2;
            let end_height: u64 = 5;
            let start = Some(BlockId {
                height: start_height,
                hash: vec![],
            });
            let end = Some(BlockId {
                height: end_height,
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
                // TODO(#1088): replace deprecated nullifier-range client usage.
                #[allow(deprecated)]
                {
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
                }
            } else {
                let pools = vec![
                    PoolType::Transparent as i32,
                    PoolType::Sapling as i32,
                    PoolType::Orchard as i32,
                ];
                let fetch_service_get_block_range = zaino_testutils::collect_block_range(
                    &fetch_service_subscriber,
                    start_height,
                    end_height,
                    pools.clone(),
                )
                .await;
                let state_service_get_block_range = zaino_testutils::collect_block_range(
                    &state_service_subscriber,
                    start_height,
                    end_height,
                    pools,
                )
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
    }
}
