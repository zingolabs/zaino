use zaino_common::network::ActivationHeights;
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_state::{ZcashIndexer, ZcashService};
use zaino_testutils::{TestManager, ValidatorExt, ValidatorKind};
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
    Service: zaino_testutils::TestService,
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

    let json_service = test_manager.full_node_jsonrpc_connector().await;
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
        test_dependencies::{chain_index::ChainIndex, ChainIndexConfig},
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
        ephemeral: bool,
    ) -> (
        TestManager<C, Service>,
        JsonRpSeeConnector,
        Option<StateService>,
        NodeBackedChainIndex,
        NodeBackedChainIndexSubscriber,
    )
    where
        C: ValidatorExt,
        Service: zaino_testutils::TestService,
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
                                nu6_2: local_net_activation_heights.nu6_2(),
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
                    false,
                    network.into(),
                    None,
                ))
                .await
                .unwrap();
                let config = ChainIndexConfig {
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
                    ephemeral,
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
                let config = ChainIndexConfig {
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
                    ephemeral,
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
        Service: zaino_testutils::TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false, false)
                .await;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn ephemeral_serves_finalised_blocks_zebrad() {
        ephemeral_serves_finalised_blocks::<Zebrad, StateService>(&ValidatorKind::Zebrad).await
    }

    /// Ephemeral mode on regtest: the chain index opens no persistent
    /// finalised-state database and serves finalised reads straight from the
    /// validator via the ephemeral passthrough.
    ///
    /// In ephemeral mode `db_height` is `0`, so the non-finalised cache retains
    /// blocks down to `tip - MAX_NFS_DEPTH` (110). We therefore generate well
    /// past that depth and query a height below `tip - 110`, so the reads are
    /// genuinely served by the ephemeral *finalised* passthrough rather than the
    /// non-finalised cache. The test then:
    /// - fetches a finalised chain (indexed) block by height, re-fetches it by
    ///   its hash, and asserts the two are identical;
    /// - streams compact blocks across the finalised / non-finalised boundary;
    /// - asserts nothing was persisted to disk.
    async fn ephemeral_serves_finalised_blocks<C, Service>(validator: &ValidatorKind)
    where
        C: ValidatorExt,
        Service: zaino_testutils::TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        use zaino_proto::proto::utils::PoolTypeFilter;

        let (test_manager, json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false, true)
                .await;

        // Generate well past MAX_NFS_DEPTH (110) so low heights are evicted from
        // the non-finalised cache and served by the ephemeral finalised passthrough.
        test_manager
            .generate_blocks_and_wait_for_tip(150, &indexer)
            .await;
        let snapshot = indexer.snapshot_nonfinalized_state().await.unwrap();
        let chain_height: u32 = json_service.get_blockchain_info().await.unwrap().blocks.0;

        // `start_height` is below `tip - 110` (evicted from the NFS cache, served
        // by the passthrough); `end_height` is above `tip - 100` (non-finalised).
        let start_height: u32 = chain_height - 120;
        let end_height: u32 = chain_height - 40;
        let finalised_height = Height::try_from(start_height).unwrap();

        // --- chain (indexed) block: fetch by height, then by its hash ---
        let block_by_height = indexer
            .get_indexed_block_by_height(&snapshot, &finalised_height)
            .await
            .unwrap()
            .expect("ephemeral passthrough must serve a finalised chain block by height");
        let block_by_hash = indexer
            .get_indexed_block_by_hash(&snapshot, block_by_height.hash())
            .await
            .unwrap()
            .expect("ephemeral passthrough must serve the same chain block by hash");
        assert_eq!(
            block_by_height, block_by_hash,
            "chain block fetched by height and by hash must be the same block"
        );

        // --- compact block stream across the finalised / non-finalised boundary ---
        let stream = indexer
            .get_compact_block_stream(
                &snapshot,
                Height::try_from(start_height).unwrap(),
                Height::try_from(end_height).unwrap(),
                PoolTypeFilter::includes_all(),
            )
            .await
            .unwrap()
            .expect("ephemeral mode must serve a compact block stream across the boundary");
        let streamed = stream.try_collect::<Vec<_>>().await.unwrap();

        let expected_count = (end_height - start_height + 1) as usize;
        assert_eq!(
            streamed.len(),
            expected_count,
            "stream must cover the full inclusive range across the finalised boundary"
        );
        for (offset, compact_block) in streamed.iter().enumerate() {
            let streamed_height = u32::try_from(compact_block.height).unwrap();
            assert_eq!(streamed_height, start_height + offset as u32);
        }

        // Ephemeral mode must persist nothing: no chain-index database directory.
        let chain_index_db_dir = test_manager.data_dir.as_path().join("chain-index-zaino");
        assert!(
            !chain_index_db_dir.exists(),
            "ephemeral mode must not create a persistent chain-index database at \
             {chain_index_db_dir:?}"
        );
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
        Service: zaino_testutils::TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, json_service, option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false, false)
                .await;

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
        Service: zaino_testutils::TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        let (test_manager, json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false, false)
                .await;

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
        Service: zaino_testutils::TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        use futures::StreamExt as _;
        use tokio::time::{timeout, Duration};

        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false, false)
                .await;

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
        Service: zaino_testutils::TestService,
        IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
        <Service as ZcashService>::Subscriber: zaino_testutils::PollableTip,
    {
        use futures::{StreamExt as _, TryStreamExt as _};
        use tokio::time::{timeout, Duration};

        let (test_manager, _json_service, _option_state_service, _chain_index, indexer) =
            create_test_manager_and_chain_index::<C, Service>(validator, None, false, false, false)
                .await;

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
