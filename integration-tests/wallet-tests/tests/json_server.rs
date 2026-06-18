//! Tests that compare the output of both `zcashd` and `zainod` through `FetchService`.

use nonempty::NonEmpty;
#[allow(deprecated)]
use zaino_state::{ChainIndex, FetchService, FetchServiceSubscriber, ZcashIndexer};
use zaino_testutils::{TestManager, ValidatorKind};
use zcash_local_net::logs::LogsToStdoutAndStderr as _;
use zcash_local_net::validator::zcashd::Zcashd;
use zcash_primitives::transaction::TxId;
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::client::GetAddressBalanceRequest;
use zebra_rpc::methods::GetAddressTxIdsRequest;

#[allow(deprecated)]
async fn create_zcashd_test_manager_and_fetch_services() -> (
    TestManager<Zcashd, FetchService>,
    FetchService,
    FetchServiceSubscriber,
    FetchService,
    FetchServiceSubscriber,
    wallet_tests::Clients,
) {
    let (
        test_manager,
        zcashd_fetch_service,
        zcashd_subscriber,
        zaino_fetch_service,
        zaino_subscriber,
    ) = zaino_testutils::launch_zcashd_dual_fetch_services().await;

    let clients = wallet_tests::build_clients(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
        wallet_tests::default_heights(&ValidatorKind::Zcashd),
    );
    (
        test_manager,
        zcashd_fetch_service,
        zcashd_subscriber,
        zaino_fetch_service,
        zaino_subscriber,
        clients,
    )
}

/// Sync the faucet, fetch the recipient's transparent and unified addresses,
/// and mine one block on both subscribers; if `send` is `Some(pool)`, also send
/// 250_000 to that pool's recipient address (mined in by that block) and return
/// its txid. Returns `(transparent_addr, unified_addr, sent_txid)`. The shared
/// preamble of the json_server `_inner` tests: the send-and-mine tests pass
/// `Some(pool)`; the mempool tests pass `None` and broadcast (unmined)
/// themselves afterward.
#[allow(deprecated)]
async fn jsonrpc_fund(
    test_manager: &TestManager<Zcashd, FetchService>,
    clients: &mut wallet_tests::Clients,
    zaino_subscriber: &FetchServiceSubscriber,
    zcashd_subscriber: &FetchServiceSubscriber,
    send: Option<wallet_tests::Pool>,
) -> (String, String, Option<NonEmpty<TxId>>) {
    wallet_tests::fund_and_send_dual(
        test_manager,
        clients,
        &ValidatorKind::Zcashd,
        zaino_subscriber,
        zcashd_subscriber,
        1,
        send,
    )
    .await
}

#[allow(deprecated)]
async fn z_get_address_balance_inner() {
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (recipient_taddr, _recipient_ua, _txid) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Transparent),
    )
    .await;

    clients.sync_recipient().await;
    let recipient_balance = clients.recipient_balance().await;

    let zcashd_service_balance = zcashd_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();

    let zaino_service_balance = zaino_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();

    dbg!(&recipient_balance);
    dbg!(&zcashd_service_balance);
    dbg!(&zaino_service_balance);

    assert_eq!(
        wallet_tests::Pool::Transparent.received_balance(&recipient_balance),
        250_000,
    );
    assert_eq!(
        wallet_tests::Pool::Transparent.received_balance(&recipient_balance),
        zcashd_service_balance.balance(),
    );
    assert_eq!(zcashd_service_balance, zaino_service_balance);

    test_manager.close().await;
}

async fn get_raw_mempool_inner() {
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (recipient_taddr, recipient_ua, _txid) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        None,
    )
    .await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;

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
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (recipient_taddr, recipient_ua, _txid) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        None,
    )
    .await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;

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
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Orchard),
    )
    .await;

    let chain_height = dbg!(zaino_subscriber.chain_height().await.unwrap()).0;

    let zcashd_treestate = dbg!(zcashd_subscriber
        .z_get_treestate(chain_height.to_string())
        .await
        .unwrap());

    let zaino_treestate = dbg!(zaino_subscriber
        .z_get_treestate(chain_height.to_string())
        .await
        .unwrap());

    assert_eq!(zcashd_treestate, zaino_treestate);

    test_manager.close().await;
}

async fn z_get_subtrees_by_index_inner() {
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Orchard),
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
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (_recipient_taddr, _recipient_ua, tx) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Orchard),
    )
    .await;
    let tx = tx.expect("jsonrpc_fund sends a tx when given Some(pool)");

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

async fn get_tx_out_inner() {
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (recipient_taddr, _recipient_ua, _txid) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Transparent),
    )
    .await;

    let zcashd_utxos = zcashd_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, txid, output_index, ..) = zcashd_utxos[0].into_parts();

    let zcashd_tx_out = zcashd_subscriber
        .get_tx_out(txid.to_string(), output_index.index(), Some(true))
        .await
        .unwrap();
    let zaino_tx_out = zaino_subscriber
        .get_tx_out(txid.to_string(), output_index.index(), Some(true))
        .await
        .unwrap();

    assert_eq!(zcashd_tx_out, zaino_tx_out);

    let zcashd_missing_tx_out = zcashd_subscriber
        .get_tx_out(txid.to_string(), output_index.index() + 100, None)
        .await
        .unwrap();
    let zaino_missing_tx_out = zaino_subscriber
        .get_tx_out(txid.to_string(), output_index.index() + 100, None)
        .await
        .unwrap();

    assert_eq!(zcashd_missing_tx_out, zaino_missing_tx_out);

    test_manager.close().await;
}

async fn get_address_tx_ids_inner() {
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (recipient_taddr, _recipient_ua, tx) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Transparent),
    )
    .await;
    let tx = tx.expect("jsonrpc_fund sends a tx when given Some(pool)");

    let chain_height: u32 = {
        let idx = &zcashd_subscriber.indexer;
        let snapshot = idx.snapshot_nonfinalized_state().await.unwrap();
        u32::from(idx.best_chaintip(&snapshot).await.unwrap().height)
    };
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
    let (
        mut test_manager,
        _zcashd_service,
        zcashd_subscriber,
        _zaino_service,
        zaino_subscriber,
        mut clients,
    ) = create_zcashd_test_manager_and_fetch_services().await;

    let (recipient_taddr, _recipient_ua, txid_1) = jsonrpc_fund(
        &test_manager,
        &mut clients,
        &zaino_subscriber,
        &zcashd_subscriber,
        Some(wallet_tests::Pool::Transparent),
    )
    .await;
    let txid_1 = txid_1.expect("jsonrpc_fund sends a tx when given Some(pool)");

    clients.sync_faucet().await;

    let zcashd_utxos = zcashd_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, zcashd_txid, ..) = zcashd_utxos[0].into_parts();

    let zaino_utxos = zaino_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr]))
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

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_address_balance() {
            z_get_address_balance_inner().await;
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
        async fn get_tx_out() {
            get_tx_out_inner().await;
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
