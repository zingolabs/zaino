use futures::StreamExt;
use zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasParams;
use zaino_proto::proto::service::{BlockId, BlockRange, TransparentAddressBlockFilter};
use zaino_state::ChainIndex as _;

use nonempty::NonEmpty;
#[allow(deprecated)]
use zaino_state::{
    FetchServiceSubscriber, LightWalletIndexer, StateService, StateServiceSubscriber, ZcashIndexer,
};
use zaino_testutils::{StateAndFetchServices, ValidatorExt};
use zaino_testutils::{TestManager, ValidatorKind};
use zcash_local_net::validator::zebrad::Zebrad;
use zcash_primitives::transaction::TxId;
use zebra_chain::parameters::NetworkKind;
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::methods::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

#[allow(deprecated)]
// NOTE: the fetch and state services each have a seperate chain index to the instance of zaino connected to the lightclients and may be out of sync
// the test manager now includes a service subscriber but not both fetch *and* state which are necessary for these tests.
// syncronicity is ensured in the following tests by calling `TestManager::generate_blocks_and_wait_for_tips`.
async fn create_test_manager_and_services<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    network: Option<NetworkKind>,
) -> (StateAndFetchServices<V>, wallet_tests::Clients) {
    create_test_manager_and_services_mining_to::<V>(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        validator,
        chain_cache,
        enable_zaino,
        network,
    )
    .await
}

/// [`create_test_manager_and_services`] with the miner's pool chosen by the
/// caller instead of [`zaino_testutils::SHIELDED_FUNDING_POOL`]. For tests
/// whose subject is the miner's coinbase footprint in a specific pool, or
/// whose large non-funding mines must stay on cheap transparent block
/// templates.
#[allow(deprecated)]
async fn create_test_manager_and_services_mining_to<V: ValidatorExt>(
    mine_to_pool: zaino_testutils::PoolType,
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
    enable_zaino: bool,
    network: Option<NetworkKind>,
) -> (StateAndFetchServices<V>, wallet_tests::Clients) {
    let svc = zaino_testutils::launch_state_and_fetch_services_mining_to(
        mine_to_pool,
        validator,
        chain_cache,
        enable_zaino,
        network,
    )
    .await;

    let clients = wallet_tests::build_clients_for(&svc.test_manager, validator);

    (svc, clients)
}

/// Sync the faucet and, on zebrad, generate blocks up to height 100 (computing
/// how many are still needed from the current tip). Shared setup for the
/// out-of-range block_range tests, which need a known tip but no funding.
#[allow(deprecated)]
async fn generate_up_to_height_100<V: ValidatorExt>(
    svc: &StateAndFetchServices<V>,
    clients: &mut wallet_tests::Clients,
    validator: &ValidatorKind,
) {
    clients.sync_faucet().await;

    // Test manager generates blocks on startup; only generate up to height 100.
    let chain_height = svc
        .state_subscriber
        .get_latest_block()
        .await
        .unwrap()
        .height as u32;
    let blocks_to_height_100 = 100 - chain_height;

    if matches!(validator, ValidatorKind::Zebrad) {
        svc.generate_blocks_and_wait_for_tips(blocks_to_height_100)
            .await;
        clients.sync_faucet().await;
    }
}

/// Fund the faucet (one coinbase batch) and send 250_000 to the recipient's
/// `pool` address, mining it in on both subscribers; returns the recipient
/// address and the send txid. The shared "fund and send one transaction" setup
/// the single-send state_service query tests share (the dual-subscriber
/// analogue of the fetch_service `fund_and_send`).
#[allow(deprecated)]
async fn fund_and_send<V: ValidatorExt>(
    svc: &StateAndFetchServices<V>,
    clients: &mut wallet_tests::Clients,
    validator: &ValidatorKind,
    pool: wallet_tests::Pool,
) -> (String, NonEmpty<TxId>) {
    let (recipient_taddr, recipient_ua, txid) = wallet_tests::fund_and_send_dual(
        &svc.test_manager,
        clients,
        validator,
        &svc.fetch_subscriber,
        &svc.state_subscriber,
        1,
        Some(pool),
    )
    .await;
    let recipient = if matches!(pool, wallet_tests::Pool::Transparent) {
        recipient_taddr
    } else {
        recipient_ua
    };
    (
        recipient,
        txid.expect("fund_and_send_dual sends a tx when given Some(pool)"),
    )
}

/// The best (nonfinalized) chaintip height as seen through the fetch service's
/// indexer snapshot.
#[allow(deprecated)]
async fn best_chaintip_height(subscriber: &FetchServiceSubscriber) -> u32 {
    let idx = &subscriber.indexer;
    let snapshot = idx.snapshot_nonfinalized_state().await.unwrap();
    u32::from(idx.best_chaintip(&snapshot).await.unwrap().height)
}

/// Get the faucet's transparent address and generate `blocks` blocks (waiting
/// on both subscribers); returns the address.
#[allow(deprecated)]
async fn generate_funded_taddr<V: ValidatorExt>(
    svc: &StateAndFetchServices<V>,
    clients: &wallet_tests::Clients,
    blocks: u32,
) -> String {
    let taddr = clients.get_faucet_address("transparent").await;
    svc.generate_blocks_and_wait_for_tips(blocks).await;
    taddr
}

/// Launch state+fetch services on regtest zebrad, fund the faucet by generating
/// `blocks` blocks, and build a query request from the faucet's transparent
/// address via `build_request`. Returns the full handle set and the request —
/// the shared `create_test_manager_and_services … → let request` preamble of
/// the `lightwallet_indexer` faucet-query tests, parameterized over the request
/// type so each test supplies only its own request shape. The owned fetch/state
/// services are returned because the caller must keep them alive for the
/// subscribers to work.
#[allow(deprecated)]
async fn launch_and_build_faucet_request<R>(
    blocks: u32,
    build_request: impl FnOnce(String) -> R,
) -> (StateAndFetchServices<Zebrad>, wallet_tests::Clients, R) {
    // These tests query the faucet taddr, which only coinbase funds — mining
    // must stay transparent or the queries compare empty against empty.
    let (svc, clients) = create_test_manager_and_services_mining_to::<Zebrad>(
        zaino_testutils::PoolType::Transparent,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(NetworkKind::Regtest),
    )
    .await;
    let taddr = generate_funded_taddr(&svc, &clients, blocks).await;
    let request = build_request(taddr);
    (svc, clients, request)
}

#[allow(deprecated)]
async fn state_service_get_address_balance<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    let (recipient_taddr, _txid) = fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Transparent,
    )
    .await;

    clients.sync_recipient().await;
    let recipient_balance = clients.recipient_balance().await;

    let fetch_service_balance = services
        .fetch_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();

    let state_service_balance = services
        .state_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&fetch_service_balance);
    dbg!(&state_service_balance);

    assert_eq!(
        wallet_tests::Pool::Transparent.received_balance(&recipient_balance),
        250_000,
    );
    assert_eq!(
        wallet_tests::Pool::Transparent.received_balance(&recipient_balance),
        fetch_service_balance.balance(),
    );
    assert_eq!(fetch_service_balance, state_service_balance);

    services.test_manager.close().await;
}

async fn state_service_get_raw_mempool<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    services.generate_blocks_and_wait_for_tips(1).await;

    wallet_tests::fund_faucet_dual(
        &services.test_manager,
        &mut clients,
        validator,
        &services.fetch_subscriber,
        &services.state_subscriber,
        2,
    )
    .await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    let mut fetch_service_mempool = services.fetch_subscriber.get_raw_mempool().await.unwrap();
    let mut state_service_mempool = services.state_subscriber.get_raw_mempool().await.unwrap();

    dbg!(&fetch_service_mempool);
    fetch_service_mempool.sort();

    dbg!(&state_service_mempool);
    state_service_mempool.sort();

    assert_eq!(fetch_service_mempool, state_service_mempool);

    services.test_manager.close().await;
}

/// Tests whether that calls to `get_block_range` with the same block range are the same when
/// specifying the default `PoolType`s and passing and empty Vec to verify that the method falls
/// back to the default pools when these are not explicitly specified.
async fn state_service_get_block_range_returns_default_pools<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Orchard,
    )
    .await;

    let start_height: u64 = 1;
    // fund_and_send mines the send into the block at the chain tip.
    let end_height: u64 = best_chaintip_height(&services.fetch_subscriber).await as u64;

    let fetch_service_get_block_range = zaino_testutils::collect_block_range(
        &services.fetch_subscriber,
        start_height,
        end_height,
        vec![],
    )
    .await;

    let fetch_service_get_block_range_specifying_pools = zaino_testutils::collect_block_range(
        &services.fetch_subscriber,
        start_height,
        end_height,
        zaino_testutils::shielded_pools_i32(),
    )
    .await;

    assert_eq!(
        fetch_service_get_block_range,
        fetch_service_get_block_range_specifying_pools
    );

    let state_service_get_block_range_specifying_pools = zaino_testutils::collect_block_range(
        &services.state_subscriber,
        start_height,
        end_height,
        zaino_testutils::shielded_pools_i32(),
    )
    .await;

    let state_service_get_block_range = zaino_testutils::collect_block_range(
        &services.state_subscriber,
        start_height,
        end_height,
        vec![],
    )
    .await;

    assert_eq!(
        state_service_get_block_range,
        state_service_get_block_range_specifying_pools
    );

    // check that the block range is the same between fetch service and state service
    assert_eq!(fetch_service_get_block_range, state_service_get_block_range);

    let compact_block = state_service_get_block_range.last().unwrap();

    assert_eq!(compact_block.height, end_height);

    // The block holds the shielded coinbase (the miner address is a shielded
    // pool, so the coinbase appears in pool-filtered ranges) and the send.
    assert_eq!(compact_block.vtx.len(), 2);

    let sent_tx = compact_block.vtx.last().unwrap();
    assert_eq!(sent_tx.index, 1);
    // tranparent data should not be present when no pool types are requested
    for tx in &compact_block.vtx {
        assert_eq!(
            tx.vin,
            vec![],
            "transparent data should not be present when no pool types are specified in the request."
        );
        assert_eq!(
            tx.vout,
            vec![],
            "transparent data should not be present when no pool types are specified in the request."
        );
    }
    services.test_manager.close().await;
}

/// tests whether the `GetBlockRange` RPC returns all pools when requested
async fn state_service_get_block_range_returns_all_pools<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    let (deshielding_txid, sapling_txid, orchard_txid) = wallet_tests::fund_and_send_to_all_pools(
        &services.test_manager,
        &mut clients,
        validator,
        &services.fetch_subscriber,
        &services.state_subscriber,
    )
    .await;

    let start_height: u64 = 1;
    // The sends above land in the block mined last — the chain tip.
    let end_height: u64 = best_chaintip_height(&services.fetch_subscriber).await as u64;
    let all_pools = zaino_testutils::all_pools_i32();

    let fetch_service_get_block_range = zaino_testutils::collect_block_range(
        &services.fetch_subscriber,
        start_height,
        end_height,
        all_pools.clone(),
    )
    .await;

    let state_service_get_block_range = zaino_testutils::collect_block_range(
        &services.state_subscriber,
        start_height,
        end_height,
        all_pools,
    )
    .await;

    // check that the block range is the same
    assert_eq!(fetch_service_get_block_range, state_service_get_block_range);

    let compact_block = state_service_get_block_range.last().unwrap();

    assert_eq!(compact_block.height, end_height);

    // the compact block has 4 transactions (3 sent + coinbase)
    assert_eq!(compact_block.vtx.len(), 4);

    wallet_tests::assert_pool_present(
        compact_block,
        &deshielding_txid,
        wallet_tests::Pool::Transparent,
    );
    wallet_tests::assert_pool_present(compact_block, &sapling_txid, wallet_tests::Pool::Sapling);
    wallet_tests::assert_pool_present(compact_block, &orchard_txid, wallet_tests::Pool::Orchard);

    services.test_manager.close().await;
}

/// Launch state+fetch services mining to `PoolType::Transparent` and bring
/// the chain to height 100 — the shared preamble of the out-of-range
/// block_range tests, which need a known tip but no funding. Transparent
/// because the ~100-block mine would cost a halo2 proof per block under a
/// shielded miner, for no benefit (nothing spends). The owned services and
/// clients are returned because the caller must keep them alive for the
/// subscribers to work.
#[allow(deprecated)]
async fn launch_transparent_with_known_tip<V: ValidatorExt>(
    validator: &ValidatorKind,
) -> (StateAndFetchServices<V>, wallet_tests::Clients) {
    let (svc, mut clients) = create_test_manager_and_services_mining_to::<V>(
        zaino_testutils::PoolType::Transparent,
        validator,
        None,
        true,
        None,
    )
    .await;

    generate_up_to_height_100(&svc, &mut clients, validator).await;

    (svc, clients)
}

// tests whether the `GetBlockRange` returns all blocks until the first requested block in the
// range can't be bound
async fn state_service_get_block_range_out_of_range_test_upper_bound<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (mut services, _clients) = launch_transparent_with_known_tip::<V>(validator).await;

    let all_pools = zaino_testutils::all_pools_i32();
    let end_height: u64 = 106;

    let (fetch_service_blocks, fetch_errored) = zaino_testutils::drain_block_range(
        &services.fetch_subscriber,
        1,
        end_height,
        all_pools.clone(),
    )
    .await;
    let (state_service_blocks, state_errored) =
        zaino_testutils::drain_block_range(&services.state_subscriber, 1, end_height, all_pools)
            .await;

    // check that the block range is the same
    assert_eq!(fetch_service_blocks, state_service_blocks);

    let compact_block = state_service_blocks.last().unwrap();
    assert!(compact_block.height < end_height);
    assert_eq!(fetch_service_blocks.len(), 100);

    // ...then an error, not a clean end-of-stream
    assert!(
        state_errored,
        "state service stream should terminate with an error, not cleanly"
    );
    assert!(
        fetch_errored,
        "fetch service stream should terminate with an error, not cleanly"
    );

    services.test_manager.close().await;
}

// tests whether the `GetBlockRange` returns all blocks until the first requested block in the
// range can't be bound
async fn state_service_get_block_range_out_of_range_test_lower_bound<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (mut services, _clients) = launch_transparent_with_known_tip::<V>(validator).await;

    let all_pools = zaino_testutils::all_pools_i32();

    let (fetch_service_blocks, fetch_errored) =
        zaino_testutils::drain_block_range(&services.fetch_subscriber, 106, 1, all_pools.clone())
            .await;
    let (state_service_blocks, state_errored) =
        zaino_testutils::drain_block_range(&services.state_subscriber, 106, 1, all_pools).await;

    // check that the block range is the same
    assert_eq!(fetch_service_blocks, state_service_blocks);
    assert!(fetch_service_blocks.is_empty());

    // ...then an error, not a clean end-of-stream
    assert!(
        state_errored,
        "state service stream should terminate with an error, not cleanly"
    );
    assert!(
        fetch_errored,
        "fetch service stream should terminate with an error, not cleanly"
    );
    // assert!(
    //     matches!(err, ZainoStateError::BlockOutOfRange { .. }),
    //     "unexpected error variant: {err:?}"
    // );

    services.test_manager.close().await;
}

async fn state_service_z_get_treestate<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Orchard,
    )
    .await;

    let chain_height = dbg!(services.state_subscriber.chain_height().await.unwrap()).0;

    let fetch_service_treestate = dbg!(services
        .fetch_subscriber
        .z_get_treestate(chain_height.to_string())
        .await
        .unwrap());

    let state_service_treestate = dbg!(services
        .state_subscriber
        .z_get_treestate(chain_height.to_string())
        .await
        .unwrap());

    assert_eq!(fetch_service_treestate, state_service_treestate);

    services.test_manager.close().await;
}

async fn state_service_z_get_subtrees_by_index<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Orchard,
    )
    .await;

    let fetch_service_subtrees = dbg!(services
        .fetch_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    let state_service_subtrees = dbg!(services
        .state_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    assert_eq!(fetch_service_subtrees, state_service_subtrees);

    services.test_manager.close().await;
}

use zcash_local_net::logs::LogsToStdoutAndStderr;
async fn state_service_get_raw_transaction<V: ValidatorExt + LogsToStdoutAndStderr>(
    validator: &ValidatorKind,
) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    let (_recipient_ua, tx) = fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Orchard,
    )
    .await;

    services.test_manager.local_net.print_stdout();

    let fetch_service_transaction = dbg!(services
        .fetch_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    let state_service_transaction = dbg!(services
        .state_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    assert_eq!(fetch_service_transaction, state_service_transaction);

    services.test_manager.close().await;
}

async fn state_service_get_address_transactions_regtest<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    wallet_tests::fund_faucet_dual(
        &services.test_manager,
        &mut clients,
        validator,
        &services.fetch_subscriber,
        &services.state_subscriber,
        1,
    )
    .await;

    let tx = clients
        .send_from_faucet(recipient_taddr.as_str(), 250_000)
        .await;
    services.generate_blocks_and_wait_for_tips(1).await;

    let chain_height = best_chaintip_height(&services.fetch_subscriber).await;
    dbg!(&chain_height);

    let state_service_txids = services
        .state_subscriber
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
                pool_types: zaino_testutils::all_pools_i32(),
            }),
        })
        .await
        .unwrap();

    dbg!(&tx);

    dbg!(&state_service_txids);
    assert!(state_service_txids.count().await > 0);

    services.test_manager.close().await;
}
async fn state_service_get_address_tx_ids<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    let (recipient_taddr, tx) = fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Transparent,
    )
    .await;

    let chain_height = best_chaintip_height(&services.fetch_subscriber).await;
    dbg!(&chain_height);

    let fetch_service_txids = services
        .fetch_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr.clone()],
            Some(chain_height - 2),
            Some(chain_height),
        ))
        .await
        .unwrap();

    let state_service_txids = services
        .state_subscriber
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

    services.test_manager.close().await;
}

async fn state_service_get_address_utxos<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut services, mut clients) =
        create_test_manager_and_services::<V>(validator, None, true, None).await;

    let (recipient_taddr, txid_1) = fund_and_send(
        &services,
        &mut clients,
        validator,
        wallet_tests::Pool::Transparent,
    )
    .await;

    clients.sync_faucet().await;

    let fetch_service_utxos = services
        .fetch_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, fetch_service_txid, ..) = fetch_service_utxos[0].into_parts();

    let state_service_utxos = services
        .state_subscriber
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

    services.test_manager.close().await;
}

mod zebra {

    use super::*;

    pub(crate) mod get {

        use super::*;
        use zaino_fetch::jsonrpsee::response::address_deltas::GetAddressDeltasResponse;
        use zcash_local_net::validator::zebrad::Zebrad;

        #[tokio::test(flavor = "multi_thread")]
        async fn address_utxos() {
            state_service_get_address_utxos::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn taddress_transactions_regtest() {
            state_service_get_address_transactions_regtest::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn address_tx_ids_regtest() {
            state_service_get_address_tx_ids::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn raw_transaction_regtest() {
            state_service_get_raw_transaction::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        mod z {
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

            #[tokio::test(flavor = "multi_thread")]
            pub(crate) async fn treestate_regtest() {
                state_service_z_get_treestate::<Zebrad>(&ValidatorKind::Zebrad).await;
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
            let (mut services, mut clients) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                None,
            )
            .await;

            let recipient_taddr = clients.get_recipient_address("transparent").await;

            wallet_tests::fund_faucet_dual(
                &services.test_manager,
                &mut clients,
                &ValidatorKind::Zebrad,
                &services.fetch_subscriber,
                &services.state_subscriber,
                1,
            )
            .await;

            clients
                .send_from_faucet(recipient_taddr.as_str(), 250_000)
                .await;

            // Let the broadcaster/subscribers observe the new tx
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            // Call the internal mempool info method
            let info = services.state_subscriber.get_mempool_info().await.unwrap();

            // Derive expected values directly from the current mempool contents
            let entries = services.state_subscriber.mempool.get_mempool().await;

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

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn address_balance_regtest() {
            state_service_get_address_balance::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn address_deltas() {
            address_deltas::main().await;
        }

        mod address_deltas;
    }

    pub(crate) mod lightwallet_indexer {
        use futures::StreamExt as _;
        use zaino_proto::proto::service::{AddressList, BlockId, GetAddressUtxosArg, TxFilter};
        use zebra_rpc::methods::GetAddressTxIdsRequest;

        use super::*;
        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction() {
            let (services, _clients) = create_test_manager_and_services::<Zebrad>(
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;
            // This test only inspects a coinbase transaction — any mined
            // block supplies one; it needs no funded wallets and no
            // particular height.
            services.generate_blocks_and_wait_for_tips(1).await;

            let block = BlockId {
                height: best_chaintip_height(&services.fetch_subscriber).await as u64,
                hash: vec![],
            };
            let state_service_block_by_height = services
                .state_subscriber
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
            let fetch_service_raw_transaction = services
                .fetch_subscriber
                .get_transaction(request.clone())
                .await
                .unwrap();
            let state_service_raw_transaction = services
                .state_subscriber
                .get_transaction(request)
                .await
                .unwrap();
            assert_eq!(fetch_service_raw_transaction, state_service_raw_transaction);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_txids() {
            let (services, _clients, request) = launch_and_build_faucet_request(100, |taddr| {
                GetAddressTxIdsRequest::new(vec![taddr], Some(2), Some(5))
            })
            .await;

            let state_service_taddress_txids = services
                .state_subscriber
                .get_address_tx_ids(request.clone())
                .await
                .unwrap();
            dbg!(&state_service_taddress_txids);
            let fetch_service_taddress_txids = services
                .fetch_subscriber
                .get_address_tx_ids(request)
                .await
                .unwrap();
            dbg!(&fetch_service_taddress_txids);
            assert_eq!(fetch_service_taddress_txids, state_service_taddress_txids);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos_stream() {
            let (services, mut clients, request) =
                launch_and_build_faucet_request(5, |taddr| GetAddressUtxosArg {
                    addresses: vec![taddr],
                    start_height: 2,
                    max_entries: 3,
                })
                .await;
            let state_service_address_utxos_streamed = services
                .state_subscriber
                .get_address_utxos_stream(request.clone())
                .await
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
                .await;
            let fetch_service_address_utxos_streamed = services
                .fetch_subscriber
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
            clients.sync_faucet().await;
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
            let (services, mut clients, request) =
                launch_and_build_faucet_request(5, |taddr| GetAddressUtxosArg {
                    addresses: vec![taddr],
                    start_height: 2,
                    max_entries: 3,
                })
                .await;
            let state_service_address_utxos = services
                .state_subscriber
                .get_address_utxos(request.clone())
                .await
                .unwrap();
            let fetch_service_address_utxos = services
                .fetch_subscriber
                .get_address_utxos(request)
                .await
                .unwrap();
            assert_eq!(fetch_service_address_utxos, state_service_address_utxos);
            clients.sync_faucet().await;
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
            let (services, _clients, request) =
                launch_and_build_faucet_request(5, |taddr| AddressList {
                    addresses: vec![taddr],
                })
                .await;

            let state_service_taddress_balance = services
                .state_subscriber
                .get_taddress_balance(request.clone())
                .await
                .unwrap();
            let fetch_service_taddress_balance = services
                .fetch_subscriber
                .get_taddress_balance(request)
                .await
                .unwrap();
            assert_eq!(
                fetch_service_taddress_balance,
                state_service_taddress_balance
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_transparent_data_from_compact_block_when_requested() {
            let (services, _clients) = create_test_manager_and_services_mining_to::<Zebrad>(
                // The assertion below requires every tx to carry a transparent
                // vout; the miner's transparent coinbase is that data source,
                // so coinbase must land on the miner taddr.
                zaino_testutils::PoolType::Transparent,
                &ValidatorKind::Zebrad,
                None,
                true,
                Some(NetworkKind::Regtest),
            )
            .await;

            services.generate_blocks_and_wait_for_tips(5).await;

            let chain_height = services
                .state_subscriber
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
            let compact_block_range = zaino_testutils::collect_block_range(
                &services.state_subscriber,
                1,
                chain_height,
                zaino_testutils::all_pools_i32(),
            )
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
