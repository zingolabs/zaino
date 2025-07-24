use zaino_fetch::jsonrpsee::connector::{test_node_and_return_url, JsonRpSeeConnector};
use zaino_state::{
    bench::chain_index::non_finalised_state::{BlockchainSource, NonFinalizedState},
    BackendType,
};
use zaino_testutils::{TestManager, ValidatorKind};

async fn create_test_manager_and_connector(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (TestManager, JsonRpSeeConnector) {
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
    (test_manager, json_service)
}

async fn create_test_manager_and_nfs(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (
    TestManager,
    JsonRpSeeConnector,
    zaino_state::bench::chain_index::non_finalised_state::NonFinalizedState,
) {
    let (test_manager, json_service) = create_test_manager_and_connector(
        validator,
        chain_cache,
        enable_zaino,
        zaino_no_sync,
        zaino_no_db,
        enable_clients,
    )
    .await;

    let network = match test_manager.network.to_string().as_str() {
        "Regtest" => zebra_chain::parameters::Network::new_regtest(
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
        "Testnet" => zebra_chain::parameters::Network::new_default_testnet(),
        "Mainnet" => zebra_chain::parameters::Network::Mainnet,
        _ => panic!("Incorrect newtork type found."),
    };

    let non_finalized_state =
        NonFinalizedState::initialize(BlockchainSource::Fetch(json_service.clone()), network)
            .await
            .unwrap();

    (test_manager, json_service, non_finalized_state)
}

#[tokio::test]
async fn nfs_simple_sync() {
    let (test_manager, _json_service, non_finalized_state) =
        create_test_manager_and_nfs(&ValidatorKind::Zebrad, None, true, false, false, true).await;

    let snapshot = non_finalized_state.get_snapshot();
    assert_eq!(
        snapshot.best_tip.0,
        zaino_state::Height::try_from(1).unwrap()
    );

    test_manager.generate_blocks_with_delay(5).await;
    non_finalized_state.sync().await.unwrap();
    let snapshot = non_finalized_state.get_snapshot();
    assert_eq!(
        snapshot.best_tip.0,
        zaino_state::Height::try_from(6).unwrap()
    );
}

mod chain_query_interface {

    use futures::TryStreamExt as _;
    use zaino_state::bench::chain_index::interface::{ChainIndex, NodeBackedChainIndex};
    use zebra_chain::serialization::ZcashDeserializeInto;

    use super::*;

    async fn create_test_manager_and_chain_index(
        validator: &ValidatorKind,
        chain_cache: Option<std::path::PathBuf>,
        enable_zaino: bool,
        zaino_no_sync: bool,
        zaino_no_db: bool,
        enable_clients: bool,
    ) -> (TestManager, JsonRpSeeConnector, NodeBackedChainIndex) {
        let (test_manager, json_service) = create_test_manager_and_connector(
            validator,
            chain_cache,
            enable_zaino,
            zaino_no_sync,
            zaino_no_db,
            enable_clients,
        )
        .await;

        let network = match test_manager.network.to_string().as_str() {
            "Regtest" => zebra_chain::parameters::Network::new_regtest(
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
            "Testnet" => zebra_chain::parameters::Network::new_default_testnet(),
            "Mainnet" => zebra_chain::parameters::Network::Mainnet,
            _ => panic!("Incorrect newtork type found."),
        };

        let chain_index =
            NodeBackedChainIndex::new(BlockchainSource::Fetch(json_service.clone()), network)
                .await
                .unwrap();

        (test_manager, json_service, chain_index)
    }

    #[tokio::test]
    async fn get_block_range() {
        let (test_manager, _json_service, chain_index) = create_test_manager_and_chain_index(
            &ValidatorKind::Zebrad,
            None,
            true,
            false,
            false,
            true,
        )
        .await;

        // this delay had to increase. Maybe we tweak sync loop rerun time?
        test_manager.generate_blocks_with_delay(5).await;
        let snapshot = chain_index.snapshot_nonfinalized_state();
        assert_eq!(snapshot.blocks.len(), 6);
        let range = chain_index
            .get_block_range(&snapshot, None, None)
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        for block in range {
            let block = block
                .zcash_deserialize_into::<zebra_chain::block::Block>()
                .unwrap();
        }
    }
}
