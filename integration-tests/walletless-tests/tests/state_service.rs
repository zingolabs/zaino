use zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasParams;

#[allow(deprecated)]
use zaino_state::{
    FetchServiceSubscriber, LightWalletIndexer, StateServiceSubscriber, ZcashIndexer,
};
use zaino_testutils::{StateAndFetchServices, ValidatorExt};
use zaino_testutils::{ValidatorKind, ZEBRAD_TESTNET_CACHE_DIR};
use zcash_local_net::validator::zebrad::Zebrad;
use zebra_chain::parameters::NetworkKind;
use zebra_rpc::methods::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

/// Launch regtest state+fetch services with no chain cache.
async fn launch_regtest(enable_zaino: bool) -> StateAndFetchServices<Zebrad> {
    zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        None,
        enable_zaino,
        Some(NetworkKind::Regtest),
    )
    .await
}

/// Launch state+fetch services against the cached testnet chain (zaino off —
/// the testnet comparisons exercise only the standalone services).
async fn launch_testnet_cached() -> StateAndFetchServices<Zebrad> {
    zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
        &ValidatorKind::Zebrad,
        ZEBRAD_TESTNET_CACHE_DIR.clone(),
        false,
        Some(NetworkKind::Testnet),
    )
    .await
}

/// Assert the fetch- and state-service subscribers agree on a query, and
/// return the agreed value for follow-up assertions. `fetch_query` and
/// `state_query` are the same query spelled once per subscriber type (the two
/// subscribers are different types, so one closure cannot serve both); each
/// takes its subscriber by value (subscribers are `Clone`) so its future owns
/// it.
#[allow(deprecated)]
async fn assert_subscribers_agree<T, FFut, SFut>(
    services: &StateAndFetchServices<Zebrad>,
    fetch_query: impl FnOnce(FetchServiceSubscriber) -> FFut,
    state_query: impl FnOnce(StateServiceSubscriber) -> SFut,
) -> T
where
    T: std::fmt::Debug + PartialEq,
    FFut: std::future::Future<Output = T>,
    SFut: std::future::Future<Output = T>,
{
    let from_fetch = dbg!(fetch_query(services.fetch_subscriber.clone()).await);
    let from_state = dbg!(state_query(services.state_subscriber.clone()).await);
    assert_eq!(from_fetch, from_state);
    from_state
}

#[allow(deprecated)]
async fn state_service_check_info<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: NetworkKind,
) {
    let mut services = zaino_testutils::launch_state_and_fetch_services::<V>(
        validator,
        chain_cache,
        false,
        Some(network),
    )
    .await;

    if dbg!(network.to_string()) == *"Regtest" {
        services.generate_blocks_and_wait_for_tips(1).await;
    }

    let fetch_service_info = dbg!(services.fetch_subscriber.get_info().await.unwrap());
    let fetch_service_blockchain_info = dbg!(services
        .fetch_subscriber
        .get_blockchain_info()
        .await
        .unwrap());

    let state_service_info = dbg!(services.state_subscriber.get_info().await.unwrap());
    let state_service_blockchain_info = dbg!(services
        .state_subscriber
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

    services.test_manager.close().await;
}

async fn state_service_get_address_balance_testnet() {
    let mut services = launch_testnet_cached().await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";
    let address_request = GetAddressBalanceRequest::new(vec![address.to_string()]);
    let state_request = address_request.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.z_get_address_balance(address_request).await.unwrap() },
        |s| async move { s.z_get_address_balance(state_request).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_get_block_raw(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: NetworkKind,
) {
    let mut services = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
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
    let state_height = height.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.z_get_block(height, Some(0)).await.unwrap() },
        |s| async move { s.z_get_block(state_height, Some(0)).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_get_block_object(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    network: NetworkKind,
) {
    let mut services = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
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
    let state_height = height.clone();

    let block = assert_subscribers_agree(
        &services,
        |f| async move { f.z_get_block(height, Some(1)).await.unwrap() },
        |s| async move { s.z_get_block(state_height, Some(1)).await.unwrap() },
    )
    .await;

    let hash = match &block {
        zebra_rpc::methods::GetBlock::Raw(_) => panic!("expected object"),
        zebra_rpc::methods::GetBlock::Object(obj) => obj.hash().to_string(),
    };
    let state_service_get_block_by_hash = services
        .state_subscriber
        .z_get_block(hash.clone(), Some(1))
        .await
        .unwrap();
    assert_eq!(state_service_get_block_by_hash, block);

    services.test_manager.close().await;
}

async fn state_service_get_raw_mempool_testnet() {
    let mut services = launch_testnet_cached().await;

    assert_subscribers_agree(
        &services,
        |f| async move {
            let mut mempool = f.get_raw_mempool().await.unwrap();
            mempool.sort();
            mempool
        },
        |s| async move {
            let mut mempool = s.get_raw_mempool().await.unwrap();
            mempool.sort();
            mempool
        },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_z_get_treestate_testnet() {
    let mut services = launch_testnet_cached().await;

    assert_subscribers_agree(
        &services,
        |f| async move { f.z_get_treestate("3000000".to_string()).await.unwrap() },
        |s| async move { s.z_get_treestate("3000000".to_string()).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_z_get_subtrees_by_index_testnet() {
    let mut services = launch_testnet_cached().await;

    assert_subscribers_agree(
        &services,
        |f| async move {
            f.z_get_subtrees_by_index("sapling".to_string(), 0.into(), None)
                .await
                .unwrap()
        },
        |s| async move {
            s.z_get_subtrees_by_index("sapling".to_string(), 0.into(), None)
                .await
                .unwrap()
        },
    )
    .await;

    assert_subscribers_agree(
        &services,
        |f| async move {
            f.z_get_subtrees_by_index("orchard".to_string(), 0.into(), None)
                .await
                .unwrap()
        },
        |s| async move {
            s.z_get_subtrees_by_index("orchard".to_string(), 0.into(), None)
                .await
                .unwrap()
        },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_get_raw_transaction_testnet() {
    let mut services = launch_testnet_cached().await;

    let txid = "abb0399df392130baa45644c421fab553670a2d0d399c4dd776a8f7862ec289d".to_string();
    let state_txid = txid.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.get_raw_transaction(txid, None).await.unwrap() },
        |s| async move { s.get_raw_transaction(state_txid, None).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_get_address_tx_ids_testnet() {
    let mut services = launch_testnet_cached().await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";
    let address_request =
        GetAddressTxIdsRequest::new(vec![address.to_string()], Some(2000000), Some(3000000));
    let state_request = address_request.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.get_address_tx_ids(address_request).await.unwrap() },
        |s| async move { s.get_address_tx_ids(state_request).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_get_address_utxos_testnet() {
    let mut services = launch_testnet_cached().await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";
    let address_request = GetAddressBalanceRequest::new(vec![address.to_string()]);
    let state_request = address_request.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.z_get_address_utxos(address_request).await.unwrap() },
        |s| async move { s.z_get_address_utxos(state_request).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
}

async fn state_service_get_address_deltas_testnet() {
    let mut services = launch_testnet_cached().await;

    let address = "tmAkxrvJCN75Ty9YkiHccqc1hJmGZpggo6i";

    // Test simple response
    let simple_request =
        GetAddressDeltasParams::new_filtered(vec![address.to_string()], 2000000, 3000000, false);
    let state_simple_request = simple_request.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.get_address_deltas(simple_request).await.unwrap() },
        |s| async move { s.get_address_deltas(state_simple_request).await.unwrap() },
    )
    .await;

    // Test response with chain info
    let chain_info_params =
        GetAddressDeltasParams::new_filtered(vec![address.to_string()], 2000000, 3000000, true);
    let state_chain_info_params = chain_info_params.clone();

    assert_subscribers_agree(
        &services,
        |f| async move { f.get_address_deltas(chain_info_params).await.unwrap() },
        |s| async move { s.get_address_deltas(state_chain_info_params).await.unwrap() },
    )
    .await;

    services.test_manager.close().await;
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
            let services = launch_regtest(true).await;
            let mut chaintip_subscriber = services.state_subscriber.chaintip_update_subscriber();
            services
                .test_manager
                .generate_blocks_and_check_each(
                    5,
                    &services.fetch_subscriber,
                    &services.state_subscriber,
                    async |_| {
                        assert_eq!(
                            chaintip_subscriber.next_tip_hash().await.unwrap().0,
                            <[u8; 32]>::try_from(
                                services
                                    .state_subscriber
                                    .get_latest_block()
                                    .await
                                    .unwrap()
                                    .hash
                            )
                            .unwrap()
                        )
                    },
                )
                .await;
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
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_best_blockhash().await.unwrap() },
                |s| async move { s.get_best_blockhash().await.unwrap() },
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn block_count() {
            let mut services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_block_count().await.unwrap() },
                |s| async move { s.get_block_count().await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn mining_info() {
            let mut services = launch_regtest(false).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_mining_info().await.unwrap() },
                |s| async move { s.get_mining_info().await.unwrap() },
            )
            .await;

            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_mining_info().await.unwrap() },
                |s| async move { s.get_mining_info().await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn difficulty() {
            let mut services = launch_regtest(true).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_difficulty().await.unwrap() },
                |s| async move { s.get_difficulty().await.unwrap() },
            )
            .await;

            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_difficulty().await.unwrap() },
                |s| async move { s.get_difficulty().await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_network_sol_ps() {
            let mut services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_network_sol_ps(None, None).await.unwrap() },
                |s| async move { s.get_network_sol_ps(None, None).await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        /// A proper test would boot up multiple nodes at the same time, and ask each node
        /// for information about its peers. In the current state, this test does nothing.
        #[tokio::test(flavor = "multi_thread")]
        async fn peer_info() {
            let mut services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_peer_info().await.unwrap() },
                |s| async move { s.get_peer_info().await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        mod z {
            use super::*;

            #[allow(deprecated)]
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            pub(crate) async fn z_validate_address() {
                let mut services = zaino_testutils::launch_state_and_fetch_services::<Zebrad>(
                    &ValidatorKind::Zebrad,
                    None,
                    true,
                    None,
                )
                .await;

                walletless_tests::rpc::z_validate_address::run_z_validate_for(
                    &services.state_subscriber,
                    walletless_tests::rpc::z_validate_address::SaplingSuite::Standard,
                )
                .await;

                services.test_manager.close().await;
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
        use zaino_proto::proto::service::{BlockId, BlockRange, GetSubtreeRootsArg};
        use zebra_rpc::methods::GetBlock;

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_block() {
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(1).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_latest_block().await.unwrap() },
                |s| async move { s.get_latest_block().await.unwrap() },
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block() {
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            let second_block_by_height = BlockId {
                height: 2,
                hash: vec![],
            };
            let state_block_id = second_block_by_height.clone();
            let block_by_height = assert_subscribers_agree(
                &services,
                |f| async move { f.get_block(second_block_by_height).await.unwrap() },
                |s| async move { s.get_block(state_block_id).await.unwrap() },
            )
            .await;

            let second_block_by_hash = BlockId {
                height: 0,
                hash: block_by_height.hash.clone(),
            };
            let state_block_id = second_block_by_hash.clone();
            let block_by_hash = assert_subscribers_agree(
                &services,
                |f| async move { f.get_block(second_block_by_hash).await.unwrap() },
                |s| async move { s.get_block(state_block_id).await.unwrap() },
            )
            .await;

            assert_eq!(block_by_hash, block_by_height)
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() {
            let services = launch_regtest(true).await;

            const BLOCK_LIMIT: u32 = 10;

            services
                .test_manager
                .generate_blocks_and_check_each(
                    BLOCK_LIMIT,
                    &services.fetch_subscriber,
                    &services.state_subscriber,
                    async |i| {
                        let block = services
                            .fetch_subscriber
                            .z_get_block(i.to_string(), Some(1))
                            .await
                            .unwrap();

                        let block_hash = match block {
                            GetBlock::Object(block) => block.hash(),
                            GetBlock::Raw(_) => panic!("Expected block object"),
                        };

                        let fetch_service_get_block_header = services
                            .fetch_subscriber
                            .get_block_header(block_hash.to_string(), false)
                            .await
                            .unwrap();

                        let state_service_block_header_response = services
                            .state_subscriber
                            .get_block_header(block_hash.to_string(), false)
                            .await
                            .unwrap();
                        assert_eq!(
                            fetch_service_get_block_header,
                            state_service_block_header_response
                        );
                    },
                )
                .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_tree_state() {
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            let chain_height = dbg!(services.state_subscriber.chain_height().await.unwrap()).0;

            let treestate_by_height = BlockId {
                height: chain_height as u64,
                hash: vec![],
            };
            let state_block_id = treestate_by_height.clone();
            assert_subscribers_agree(
                &services,
                |f| async move { f.get_tree_state(treestate_by_height).await.unwrap() },
                |s| async move { s.get_tree_state(state_block_id).await.unwrap() },
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_subtree_roots() {
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(5).await;

            let sapling_subtree_roots_request = GetSubtreeRootsArg {
                start_index: 2,
                shielded_protocol: 0,
                max_entries: 0,
            };
            assert_subscribers_agree(
                &services,
                |f| async move {
                    f.get_subtree_roots(sapling_subtree_roots_request)
                        .await
                        .unwrap()
                        .map(Result::unwrap)
                        .collect::<Vec<_>>()
                        .await
                },
                |s| async move {
                    s.get_subtree_roots(sapling_subtree_roots_request)
                        .await
                        .unwrap()
                        .map(Result::unwrap)
                        .collect::<Vec<_>>()
                        .await
                },
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_latest_tree_state() {
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(2).await;

            assert_subscribers_agree(
                &services,
                |f| async move { f.get_latest_tree_state().await.unwrap() },
                |s| async move { s.get_latest_tree_state().await.unwrap() },
            )
            .await;
        }

        async fn get_block_range_helper(nullifiers_only: bool) {
            let services = launch_regtest(true).await;
            services.generate_blocks_and_wait_for_tips(6).await;

            let start_height: u64 = 2;
            let end_height: u64 = 5;
            if nullifiers_only {
                let request = BlockRange {
                    start: Some(BlockId {
                        height: start_height,
                        hash: vec![],
                    }),
                    end: Some(BlockId {
                        height: end_height,
                        hash: vec![],
                    }),
                    pool_types: zaino_testutils::all_pools_i32(),
                };
                let state_request = request.clone();
                // TODO(#1088): replace deprecated nullifier-range client usage.
                #[allow(deprecated)]
                assert_subscribers_agree(
                    &services,
                    |f| async move {
                        f.get_block_range_nullifiers(request)
                            .await
                            .unwrap()
                            .map(Result::unwrap)
                            .collect::<Vec<_>>()
                            .await
                    },
                    |s| async move {
                        s.get_block_range_nullifiers(state_request)
                            .await
                            .unwrap()
                            .map(Result::unwrap)
                            .collect::<Vec<_>>()
                            .await
                    },
                )
                .await;
            } else {
                let pools = zaino_testutils::all_pools_i32();
                let fetch_service_get_block_range = zaino_testutils::collect_block_range(
                    &services.fetch_subscriber,
                    start_height,
                    end_height,
                    pools.clone(),
                )
                .await;
                let state_service_get_block_range = zaino_testutils::collect_block_range(
                    &services.state_subscriber,
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
