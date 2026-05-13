use zaino_common::network::ActivationHeights;
use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_state::{ZcashIndexer, ZcashService};
use zaino_testutils::{TestManager, ValidatorExt, ValidatorKind};
use zainodlib::config::ZainodConfig;
use zainodlib::error::IndexerError;

#[allow(deprecated)]
async fn create_test_manager_and_connector<T, Service>(
    validator: &ValidatorKind,
    activation_heights: Option<ActivationHeights>,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    enable_clients: bool,
) -> (TestManager<T, Service>, JsonRpSeeConnector)
where
    T: ValidatorExt,
    Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
        + Send
        + Sync
        + 'static,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
{
    let test_manager = TestManager::<T, Service>::launch(
        validator,
        None,
        activation_heights,
        chain_cache,
        enable_zaino,
        false,
        enable_clients,
    )
    .await
    .unwrap();

    let json_service = JsonRpSeeConnector::new_with_basic_auth(
        test_node_and_return_url(
            &test_manager.full_node_rpc_listen_address.to_string(),
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
    (test_manager, json_service)
}

#[allow(deprecated)]
mod chain_query_interface {

    use std::time::Duration;

    use futures::TryStreamExt as _;
    use zaino_common::{CacheConfig, DatabaseConfig, ServiceConfig, StorageConfig};
    use zaino_state::{
        chain_index::{
            source::ValidatorConnector, NodeBackedChainIndex, NodeBackedChainIndexSubscriber,
            ShieldedPool,
        },
        test_dependencies::{chain_index::ChainIndex, BlockCacheConfig},
        FetchService, Height, StateService, StateServiceConfig, ZcashService,
    };
    use zcash_local_net::validator::{zcashd::Zcashd, zebrad::Zebrad};
    use zebra_chain::{
        parameters::{
            testnet::{ConfiguredActivationHeights, RegtestParameters},
            NetworkKind,
        },
        serialization::ZcashDeserializeInto,
    };

    use super::*;

    #[allow(deprecated)]
    async fn create_test_manager_and_chain_index<C, Service>(
        validator: &ValidatorKind,
        chain_cache: Option<std::path::PathBuf>,
        enable_zaino: bool,
        enable_clients: bool,
    ) -> (
        TestManager<C, Service>,
        JsonRpSeeConnector,
        Option<StateService>,
        NodeBackedChainIndex,
        NodeBackedChainIndexSubscriber,
    )
    where
        C: ValidatorExt,
        Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
            + Send
            + Sync
            + 'static,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, json_service) = create_test_manager_and_connector::<C, Service>(
            validator,
            None,
            chain_cache.clone(),
            enable_zaino,
            enable_clients,
        )
        .await;

        match validator {
            ValidatorKind::Zebrad => {
                let state_chain_cache_dir = match chain_cache {
                    Some(dir) => dir,
                    None => test_manager.data_dir.clone(),
                };
                let network = match test_manager.network {
                    NetworkKind::Regtest => {
                        let local_net_activation_heights =
                            test_manager.local_net.get_activation_heights().await;

                        zebra_chain::parameters::Network::new_regtest(RegtestParameters::from(
                            ConfiguredActivationHeights {
                                before_overwinter: local_net_activation_heights.overwinter(),
                                overwinter: local_net_activation_heights.overwinter(),
                                sapling: local_net_activation_heights.sapling(),
                                blossom: local_net_activation_heights.blossom(),
                                heartwood: local_net_activation_heights.heartwood(),
                                canopy: local_net_activation_heights.canopy(),
                                nu5: local_net_activation_heights.nu5(),
                                nu6: local_net_activation_heights.nu6(),
                                nu6_1: local_net_activation_heights.nu6_1(),
                                nu7: local_net_activation_heights.nu7(),
                            },
                        ))
                    }

                    NetworkKind::Testnet => zebra_chain::parameters::Network::new_default_testnet(),
                    NetworkKind::Mainnet => zebra_chain::parameters::Network::Mainnet,
                };
                // FIXME: when state service is integrated into chain index this initialization must change
                let state_service = StateService::spawn(StateServiceConfig::new(
                    zebra_state::Config {
                        cache_dir: state_chain_cache_dir,
                        ephemeral: false,
                        delete_old_database: true,
                        debug_stop_at_height: None,
                        debug_validity_check_interval: None,
                        // todo: does this matter?
                        should_backup_non_finalized_state: true,
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
                        cache: CacheConfig::default(),
                        database: DatabaseConfig {
                            path: test_manager
                                .data_dir
                                .as_path()
                                .to_path_buf()
                                .join("state-service-zaino"),
                            ..Default::default()
                        },
                    },
                    network.into(),
                    None,
                ))
                .await
                .unwrap();
                let config = BlockCacheConfig {
                    storage: StorageConfig {
                        database: DatabaseConfig {
                            path: test_manager
                                .data_dir
                                .as_path()
                                .to_path_buf()
                                .join("chain-index-zaino"),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    db_version: 1,
                    network: zaino_common::Network::Regtest(ActivationHeights::from(
                        test_manager.local_net.get_activation_heights().await,
                    )),
                };

                // **NOTE** The "fetch" backend is currently the backend used in the wild, and
                // by zallet, although we want to push the community to transition to the
                // "state" backend these tests using the "fetch" backend is currently useful
                // for debugging bugs raised byt zallet devs.
                let source = ValidatorConnector::Fetch(json_service.clone());
                // let source = ValidatorConnector::State(chain_index::source::State {
                //     read_state_service: state_service.read_state_service().clone(),
                //     mempool_fetcher: json_service.clone(),
                //     network: config.network,
                // });
                let chain_index = NodeBackedChainIndex::new(source, config).await.unwrap();

                let index_reader = chain_index.subscriber();
                tokio::time::sleep(Duration::from_secs(3)).await;

                (
                    test_manager,
                    json_service,
                    Some(state_service),
                    chain_index,
                    index_reader,
                )
            }
            ValidatorKind::Zcashd => {
                let config = BlockCacheConfig {
                    storage: StorageConfig {
                        database: DatabaseConfig {
                            path: test_manager
                                .data_dir
                                .as_path()
                                .to_path_buf()
                                .join("chain-index-zaino"),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    db_version: 1,
                    network: zaino_common::Network::Regtest(
                        test_manager.local_net.get_activation_heights().await.into(),
                    ),
                };
                let chain_index = NodeBackedChainIndex::new(
                    ValidatorConnector::Fetch(json_service.clone()),
                    config,
                )
                .await
                .unwrap();
                let index_reader = chain_index.subscriber();
                tokio::time::sleep(Duration::from_secs(3)).await;

                (test_manager, json_service, None, chain_index, index_reader)
            }
        }
    }

    // #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_zebrad() {
        get_block_range::<Zebrad, StateService>(&ValidatorKind::Zebrad).await
    }

    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_zcashd() {
        get_block_range::<Zcashd, FetchService>(&ValidatorKind::Zcashd).await
    }

    async fn get_block_range<C, Service>(validator: &ValidatorKind)
    where
        C: ValidatorExt,
        Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
            + Send
            + Sync
            + 'static,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false).await;

        test_manager
            .generate_blocks_and_wait_for_tip(5, &indexer)
            .await;
        let snapshot = indexer.snapshot_nonfinalized_state().await.unwrap();
        let range = indexer
            .get_block_range(&snapshot, Height::try_from(0).unwrap(), None)
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in range {
            block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
        }
    }

    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_large_chain_zebrad() {
        sync_large_chain::<Zebrad, StateService>(&ValidatorKind::Zebrad).await
    }

    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn sync_large_chain_zcashd() {
        sync_large_chain::<Zcashd, FetchService>(&ValidatorKind::Zcashd).await
    }

    async fn sync_large_chain<C, Service>(validator: &ValidatorKind)
    where
        C: ValidatorExt,
        Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
            + Send
            + Sync
            + 'static,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, json_service, option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false).await;

        test_manager
            .generate_blocks_and_wait_for_tip(5, &indexer)
            .await;
        if let Some(state_service) = option_state_service.as_ref() {
            test_manager
                .generate_blocks_and_wait_for_tip(0, state_service.get_subscriber().inner_ref())
                .await
        }

        test_manager
            .generate_blocks_and_wait_for_tip(150, &indexer)
            .await;
        if let Some(state_service) = option_state_service.as_ref() {
            test_manager
                .generate_blocks_and_wait_for_tip(0, state_service.get_subscriber().inner_ref())
                .await;
        }

        tokio::time::sleep(std::time::Duration::from_millis(5000)).await;

        let snapshot = indexer.snapshot_nonfinalized_state().await.unwrap();
        let chain_height = json_service.get_blockchain_info().await.unwrap().blocks.0;

        let finalised_start = Height::try_from(chain_height - 150).unwrap();
        let finalised_tip = Height::try_from(chain_height - 100).unwrap();
        let end = Height::try_from(chain_height - 50).unwrap();

        let finalized_blocks = indexer
            .get_block_range(&snapshot, finalised_start, Some(finalised_tip))
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in finalized_blocks {
            block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
        }

        let non_finalised_blocks = indexer
            .get_block_range(&snapshot, finalised_tip, Some(end))
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in non_finalised_blocks {
            block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
        }
    }

    // #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots_zebrad() {
        get_subtree_roots::<Zebrad, StateService>(&ValidatorKind::Zebrad).await
    }

    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_subtree_roots_zcashd() {
        get_subtree_roots::<Zcashd, FetchService>(&ValidatorKind::Zcashd).await
    }

    async fn get_subtree_roots<C, Service>(validator: &ValidatorKind)
    where
        C: ValidatorExt,
        Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
            + Send
            + Sync
            + 'static,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false).await;

        test_manager
            .generate_blocks_and_wait_for_tip(5, &indexer)
            .await;

        let test_pools = [ShieldedPool::Sapling, ShieldedPool::Orchard];
        let valid_start_index = 0;
        let max_entries = Some(0);

        // *** Test valid requests ***

        for pool in test_pools.clone() {
            let valid_chain_index_subtree_roots_response = indexer
                .get_subtree_roots(pool.clone(), valid_start_index, max_entries)
                .await
                .unwrap();

            let valid_validator_subtree_roots_response = json_service
                .get_subtrees_by_index(pool.pool_string(), valid_start_index, max_entries)
                .await
                .unwrap();
            let formatted_valid_validator_subtree_roots: Vec<([u8; 32], u32)> =
                valid_validator_subtree_roots_response
                    .subtrees
                    .into_iter()
                    .map(|subtree| {
                        // subtree.root is a hex string; decode to bytes and convert to array
                        let bytes = hex::decode(&subtree.root)
                            .expect("subtree root from validator is not valid hex");
                        let array: [u8; 32] = bytes
                            .as_slice()
                            .try_into()
                            .expect("received subtree root that is not 32 bytes");
                        (array, subtree.end_height.0)
                    })
                    .collect();

            assert_eq!(
                valid_chain_index_subtree_roots_response,
                formatted_valid_validator_subtree_roots
            );
        }

        // *** Test invalid requests ***

        let invalid_start_index = 10000;

        let valid_chain_index_subtree_roots_response = indexer
            .get_subtree_roots(test_pools[1].clone(), invalid_start_index, max_entries)
            .await
            .unwrap();

        let valid_validator_subtree_roots_response = json_service
            .get_subtrees_by_index(
                test_pools[1].pool_string(),
                invalid_start_index,
                max_entries,
            )
            .await
            .unwrap();
        let formatted_valid_validator_subtree_roots: Vec<([u8; 32], u32)> =
            valid_validator_subtree_roots_response
                .subtrees
                .into_iter()
                .map(|subtree| {
                    // subtree.root is a hex string; decode to bytes and convert to array
                    let bytes = hex::decode(&subtree.root)
                        .expect("subtree root from validator is not valid hex");
                    let array: [u8; 32] = bytes
                        .as_slice()
                        .try_into()
                        .expect("received subtree root that is not 32 bytes");
                    (array, subtree.end_height.0)
                })
                .collect();

        assert_eq!(
            valid_chain_index_subtree_roots_response,
            formatted_valid_validator_subtree_roots
        );
    }

    // #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated_zebrad() {
        get_mempool_stream_fresh_snapshot_repeated::<Zebrad, FetchService>(&ValidatorKind::Zebrad)
            .await
    }

    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream_fresh_snapshot_repeated_zcashd() {
        get_mempool_stream_fresh_snapshot_repeated::<Zcashd, FetchService>(&ValidatorKind::Zcashd)
            .await
    }

    async fn get_mempool_stream_fresh_snapshot_repeated<C, Service>(validator: &ValidatorKind)
    where
        C: ValidatorExt,
        Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
            + Send
            + Sync
            + 'static,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        use futures::StreamExt as _;
        use tokio::time::{timeout, Duration};

        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false).await;

        test_manager
            .generate_blocks_and_wait_for_tip(5, &indexer)
            .await;

        for iteration in 0..5 {
            let snapshot = indexer.snapshot_nonfinalized_state().await.unwrap();

            tokio::time::sleep(Duration::from_millis(500)).await;

            let mut mempool_stream =
                indexer
                    .get_mempool_stream(Some(&snapshot))
                    .unwrap_or_else(|| {
                        panic!("fresh snapshot unexpectedly returned None on iteration {iteration}")
                    });

            test_manager
                .generate_blocks_and_wait_for_tip(1, &indexer)
                .await;

            timeout(Duration::from_secs(20), async {
                while let Some(item) = mempool_stream.next().await {
                    item.expect("mempool stream yielded unexpected error");
                }
            })
            .await
            .expect("mempool stream did not close after chain tip changed");
        }
    }

    // #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn zallet_like_steady_state_loop_zebrad() {
        zallet_like_steady_state_loop::<Zebrad, FetchService>(&ValidatorKind::Zebrad).await
    }

    #[ignore = "prone to timeouts and hangs, to be fixed in chain index integration"]
    #[tokio::test(flavor = "multi_thread")]
    async fn zallet_like_steady_state_loop_zcashd() {
        zallet_like_steady_state_loop::<Zcashd, FetchService>(&ValidatorKind::Zcashd).await
    }

    async fn zallet_like_steady_state_loop<C, Service>(validator: &ValidatorKind)
    where
        C: ValidatorExt,
        Service: zaino_state::ZcashService<Config: TryFrom<ZainodConfig, Error = IndexerError>>
            + Send
            + Sync
            + 'static,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        use futures::{StreamExt as _, TryStreamExt as _};
        use tokio::time::{timeout, Duration};

        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false).await;

        test_manager
            .generate_blocks_and_wait_for_tip(5, &indexer)
            .await;

        let initial_snapshot = indexer.snapshot_nonfinalized_state().await.unwrap();
        let mut prev_tip = indexer.best_chaintip(&initial_snapshot).await.unwrap();

        for iteration in 0..5 {
            let snapshot = indexer.snapshot_nonfinalized_state().await.unwrap();
            let current_tip = indexer.best_chaintip(&snapshot).await.unwrap();

            let fork_point = indexer
            .find_fork_point(&snapshot, &prev_tip.hash)
            .await
            .unwrap()
            .unwrap_or_else(|| {
                panic!(
                    "no fork point found on iteration {iteration}: prev_tip=({:?}, {:?}) current_tip=({:?}, {:?})",
                    prev_tip.height,
                    prev_tip.hash,
                    current_tip.height,
                    current_tip.hash,
                )
            });

            assert!(
                fork_point.1 <= current_tip.height,
                "fork point height {:?} above current tip {:?} on iteration {iteration}",
                fork_point.1,
                current_tip.height,
            );

            if fork_point.1 < current_tip.height {
                let start_height = fork_point.1 + 1;
                let end_height = Some(current_tip.height);

                let blocks_to_apply = indexer
                    .get_block_range(&snapshot, start_height, end_height)
                    .unwrap_or_else(|| {
                        panic!(
                            "expected block range on iteration {iteration}: start={:?} end={:?}",
                            start_height, end_height,
                        )
                    });

                let applied_blocks = blocks_to_apply.try_collect::<Vec<_>>().await.unwrap();

                let expected_count = u32::from(current_tip.height) - u32::from(fork_point.1);

                assert_eq!(
                    applied_blocks.len(),
                    expected_count as usize,
                    "unexpected number of applied blocks on iteration {iteration}",
                );
            }

            let mut mempool_stream =
                indexer
                    .get_mempool_stream(Some(&snapshot))
                    .unwrap_or_else(|| {
                        panic!(
                            "fresh snapshot unexpectedly returned None on iteration {iteration}: \
                     current tip height={:?} hash={:?}, \
                     prev_tip height={:?} hash={:?}",
                            current_tip.height, current_tip.hash, prev_tip.height, prev_tip.hash,
                        )
                    });

            test_manager
                .generate_blocks_and_wait_for_tip(1, &indexer)
                .await;

            timeout(Duration::from_secs(20), async {
                while let Some(item) = mempool_stream.next().await {
                    item.expect("mempool stream yielded unexpected error");
                }
            })
            .await
            .expect("mempool stream did not close after chain tip changed");

            prev_tip = current_tip;
        }
    }
}
