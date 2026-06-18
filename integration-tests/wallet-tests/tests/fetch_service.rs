//! These tests compare the output of `FetchService` with the output of `JsonRpcConnector`.

use futures::StreamExt as _;
use hex::ToHex as _;
use nonempty::NonEmpty;
use zaino_proto::proto::service::{
    AddressList, BlockId, BlockRange, GetAddressUtxosArg, GetMempoolTxRequest,
    TransparentAddressBlockFilter, TxFilter,
};
use zaino_state::ChainIndex;
use zaino_state::FetchServiceSubscriber;
#[allow(deprecated)]
use zaino_state::{FetchService, LightWalletIndexer, ZcashIndexer};
use zaino_testutils::{PollableTip, TestManager, ValidatorExt, ValidatorKind};
use zcash_primitives::transaction::TxId;
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::methods::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

#[allow(deprecated)]
async fn create_test_manager_and_fetch_service<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
) -> (
    TestManager<V, FetchService>,
    FetchServiceSubscriber,
    wallet_tests::Clients,
) {
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber_mining_to(
            zaino_testutils::SHIELDED_FUNDING_POOL,
            validator,
            chain_cache,
        )
        .await;
    let clients = wallet_tests::build_clients_for(&test_manager, validator);
    (test_manager, fetch_service_subscriber, clients)
}

/// Launch, fund the faucet, and send 250_000 to the recipient's transparent,
/// sapling, and unified addresses (one tx each), then mine. Returns the
/// manager, subscriber, clients, and the three txids — the shared setup for the
/// get_block_range tests, which differ only in the pools requested and asserted.
#[allow(deprecated)]
async fn block_range_fixture<V: ValidatorExt>(
    validator: &ValidatorKind,
) -> (
    TestManager<V, FetchService>,
    FetchServiceSubscriber,
    wallet_tests::Clients,
    TxId,
    TxId,
    TxId,
) {
    let (test_manager, fetch_service_subscriber, mut clients) =
        create_test_manager_and_fetch_service::<V>(validator, None).await;

    let (deshielding_txid, sapling_txid, orchard_txid) = wallet_tests::fund_and_send_to_all_pools(
        &test_manager,
        &mut clients,
        validator,
        &fetch_service_subscriber,
        &fetch_service_subscriber,
    )
    .await;

    (
        test_manager,
        fetch_service_subscriber,
        clients,
        deshielding_txid,
        sapling_txid,
        orchard_txid,
    )
}

/// Launch, fund the faucet, then broadcast (without mining) one transparent and
/// one orchard send into the mempool, waiting briefly for the broadcaster and
/// subscribers to observe them. Returns the manager, subscriber, clients, and
/// the two txids (transparent first, then unified) — the shared setup for the
/// mempool query tests.
#[allow(deprecated)]
async fn fund_and_fill_mempool<V: ValidatorExt>(
    validator: &ValidatorKind,
) -> (
    TestManager<V, FetchService>,
    FetchServiceSubscriber,
    wallet_tests::Clients,
    NonEmpty<TxId>,
    NonEmpty<TxId>,
) {
    let (test_manager, fetch_service_subscriber, mut clients) =
        create_test_manager_and_fetch_service::<V>(validator, None).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, &fetch_service_subscriber)
        .await;
    wallet_tests::fund_faucet_dual(
        &test_manager,
        &mut clients,
        validator,
        &fetch_service_subscriber,
        &fetch_service_subscriber,
        2,
    )
    .await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    let recipient_ua = clients.get_recipient_address("unified").await;
    let transparent_txid = clients.send_from_faucet(&recipient_taddr, 250_000).await;
    let unified_txid = clients.send_from_faucet(&recipient_ua, 250_000).await;

    // Allow the broadcaster and subscribers to observe the new transactions.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    (
        test_manager,
        fetch_service_subscriber,
        clients,
        transparent_txid,
        unified_txid,
    )
}

/// Launch, fund the faucet (one coinbase batch), and send 250_000 to the
/// recipient's `pool` address, mining it in. Returns the manager, subscriber,
/// clients, and the send txid — the shared setup for the mined query tests.
#[allow(deprecated)]
async fn fund_and_send<V: ValidatorExt>(
    validator: &ValidatorKind,
    pool: wallet_tests::Pool,
) -> (
    TestManager<V, FetchService>,
    FetchServiceSubscriber,
    wallet_tests::Clients,
    NonEmpty<TxId>,
) {
    let (test_manager, fetch_service_subscriber, mut clients) =
        create_test_manager_and_fetch_service::<V>(validator, None).await;
    let (_recipient_taddr, _recipient_ua, txid) = wallet_tests::fund_and_send_dual(
        &test_manager,
        &mut clients,
        validator,
        &fetch_service_subscriber,
        &fetch_service_subscriber,
        1,
        Some(pool),
    )
    .await;
    let tx = txid.expect("fund_and_send_dual sends a tx when given Some(pool)");
    (test_manager, fetch_service_subscriber, clients, tx)
}

#[allow(deprecated)]
async fn fetch_service_get_address_balance<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, mut clients, _tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_address = clients.get_recipient_address("transparent").await;

    clients.sync_recipient().await;
    let recipient_balance = clients.recipient_balance().await;

    let fetch_service_balance = fetch_service_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_address]))
        .await
        .unwrap();

    dbg!(recipient_balance.clone());
    dbg!(fetch_service_balance);

    assert_eq!(
        wallet_tests::Pool::Transparent.received_balance(&recipient_balance),
        250_000,
    );
    assert_eq!(
        wallet_tests::Pool::Transparent.received_balance(&recipient_balance),
        fetch_service_balance.balance(),
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_raw_mempool<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, _clients, _transparent_txid, _unified_txid) =
        fund_and_fill_mempool::<V>(validator).await;

    let json_service = test_manager.full_node_jsonrpc_connector().await;

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
    let (mut test_manager, fetch_service_subscriber, _clients, _transparent_txid, _unified_txid) =
        fund_and_fill_mempool::<V>(validator).await;

    // Internal method now used for all validators.
    let info = fetch_service_subscriber.get_mempool_info().await.unwrap();

    // Derive expected values directly from the current mempool contents.

    let keys = fetch_service_subscriber
        .indexer
        .get_mempool_txids()
        .await
        .unwrap();

    let values = fetch_service_subscriber
        .indexer
        .get_mempool_transactions(Vec::new())
        .await
        .unwrap();

    // Size
    assert_eq!(info.size, values.len() as u64);
    assert!(info.size >= 1);

    // Bytes: sum of SerializedTransaction lengths
    let expected_bytes: u64 = values.iter().map(|entry| entry.len() as u64).sum();

    // Key heap bytes: sum of txid String capacities
    let expected_key_heap_bytes: u64 = keys
        .iter()
        .map(|key| key.encode_hex::<String>().capacity() as u64)
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
    let (mut test_manager, fetch_service_subscriber, _clients, _tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Orchard).await;

    let chain_height = dbg!(fetch_service_subscriber.chain_height().await.unwrap()).0;

    dbg!(fetch_service_subscriber
        .z_get_treestate(chain_height.to_string())
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_z_get_subtrees_by_index<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, _clients, _tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Orchard).await;

    dbg!(fetch_service_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_raw_transaction<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, _clients, tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Orchard).await;

    dbg!(fetch_service_subscriber
        .get_raw_transaction(tx.first().to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_address_tx_ids<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, clients, tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let chain_height = fetch_service_subscriber.chain_height().await.unwrap().0;
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
    let (mut test_manager, fetch_service_subscriber, mut clients, txid_1) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.sync_faucet().await;

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
async fn fetch_service_get_block_range_returns_all_pools<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        fetch_service_subscriber,
        _clients,
        deshielding_txid,
        sapling_txid,
        orchard_txid,
    ) = block_range_fixture::<V>(validator).await;

    let start_height: u64 = 1;
    // The fixture's sends land in the block it mines last — the chain tip.
    let end_height: u64 = fetch_service_subscriber.tip_height().await;

    let fetch_service_get_block_range = zaino_testutils::collect_block_range(
        &fetch_service_subscriber,
        start_height,
        end_height,
        zaino_testutils::all_pools_i32(),
    )
    .await;

    let compact_block = fetch_service_get_block_range.last().unwrap();

    assert_eq!(compact_block.height, end_height);

    // Transparent tx are included in compact blocks unless pools are specified,
    // so expect 4 (3 sent tx + coinbase).
    assert_eq!(compact_block.vtx.len(), 4);

    wallet_tests::assert_pool_present(
        compact_block,
        &deshielding_txid,
        wallet_tests::Pool::Transparent,
    );
    wallet_tests::assert_pool_present(compact_block, &sapling_txid, wallet_tests::Pool::Sapling);
    wallet_tests::assert_pool_present(compact_block, &orchard_txid, wallet_tests::Pool::Orchard);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_range_no_pools_returns_sapling_orchard<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (
        mut test_manager,
        fetch_service_subscriber,
        _clients,
        deshielding_txid,
        sapling_txid,
        orchard_txid,
    ) = block_range_fixture::<V>(validator).await;

    let start_height: u64 = 1;
    // The fixture's sends land in the block it mines last — the chain tip.
    let end_height: u64 = fetch_service_subscriber.tip_height().await;

    let fetch_service_get_block_range = zaino_testutils::collect_block_range(
        &fetch_service_subscriber,
        start_height,
        end_height,
        vec![],
    )
    .await;

    let compact_block = fetch_service_get_block_range.last().unwrap();

    assert_eq!(compact_block.height, end_height);

    // Both validators mine shielded coinbase, so the pool-filtered block
    // shows the 3 sends plus the coinbase tx.
    assert_eq!(compact_block.vtx.len(), 4);

    // No pools requested: transparent data is omitted, sapling/orchard default in.
    wallet_tests::assert_pool_absent(
        compact_block,
        &deshielding_txid,
        wallet_tests::Pool::Transparent,
    );
    wallet_tests::assert_pool_present(compact_block, &sapling_txid, wallet_tests::Pool::Sapling);
    wallet_tests::assert_pool_present(compact_block, &orchard_txid, wallet_tests::Pool::Orchard);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_transaction_mined<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, _clients, tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Orchard).await;

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
    let (mut test_manager, fetch_service_subscriber, mut clients) =
        create_test_manager_and_fetch_service::<V>(validator, None).await;
    wallet_tests::fund_faucet_dual(
        &test_manager,
        &mut clients,
        validator,
        &fetch_service_subscriber,
        &fetch_service_subscriber,
        1,
    )
    .await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let tx = clients.send_from_faucet(&recipient_ua, 250_000).await;

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
    let (mut test_manager, fetch_service_subscriber, clients, tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let chain_height = fetch_service_subscriber.chain_height().await.unwrap().0;
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
            pool_types: zaino_testutils::all_pools_i32(),
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
    let (mut test_manager, fetch_service_subscriber, mut clients, _tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    clients.sync_recipient().await;
    let balance = clients.recipient_balance().await;

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
        wallet_tests::Pool::Transparent.received_balance(&balance)
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_mempool_tx<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, _clients, tx_1, tx_2) =
        fund_and_fill_mempool::<V>(validator).await;

    let exclude_list_empty = GetMempoolTxRequest {
        exclude_txid_suffixes: Vec::new(),
        pool_types: Vec::new(),
    };

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
    sorted_fetch_mempool_tx.sort_by_key(|tx| tx.txid.clone());

    // Transaction IDs from quick_send are already in internal byte order,
    // which matches what the mempool returns, so no reversal needed
    let tx1_bytes = *tx_1.first().as_ref();
    let tx2_bytes = *tx_2.first().as_ref();

    let mut sorted_txids = [tx1_bytes, tx2_bytes];
    sorted_txids.sort_by_key(|hash| *hash);

    assert_eq!(sorted_fetch_mempool_tx[0].txid, sorted_txids[0]);
    assert_eq!(sorted_fetch_mempool_tx[1].txid, sorted_txids[1]);
    assert_eq!(sorted_fetch_mempool_tx.len(), 2);

    let exclude_list = GetMempoolTxRequest {
        exclude_txid_suffixes: vec![sorted_txids[0][8..].to_vec()],
        pool_types: vec![],
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
    sorted_exclude_fetch_mempool_tx.sort_by_key(|tx| tx.txid.clone());

    assert_eq!(sorted_exclude_fetch_mempool_tx[0].txid, sorted_txids[1]);
    assert_eq!(sorted_exclude_fetch_mempool_tx.len(), 1);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_mempool_stream<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, mut clients) =
        create_test_manager_and_fetch_service::<V>(validator, None).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, &fetch_service_subscriber)
        .await;
    wallet_tests::fund_faucet_dual(
        &test_manager,
        &mut clients,
        validator,
        &fetch_service_subscriber,
        &fetch_service_subscriber,
        2,
    )
    .await;

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
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, &fetch_service_subscriber)
        .await;

    let fetch_mempool_tx = fetch_service_handle.await.unwrap();

    let mut sorted_fetch_mempool_tx = fetch_mempool_tx.clone();
    sorted_fetch_mempool_tx.sort_by_key(|tx| tx.data.clone());

    dbg!(sorted_fetch_mempool_tx);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_taddress_utxos<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber, clients, tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

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
    let (mut test_manager, fetch_service_subscriber, clients, _tx) =
        fund_and_send::<V>(validator, wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

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

/// Generates a validator's `mod get` fetch_service test wrappers: one
/// `#[tokio::test]` per `fetch_service_*` helper, turbofished to `$validator`
/// and `$kind`. A macro rather than a fn because each wrapper must be a
/// discoverable `#[tokio::test]` item, which a function cannot emit.
macro_rules! fetch_service_tests {
    ($modname:ident, $validator:ty, $kind:expr) => {
        mod $modname {
            use super::*;

            mod get {
                use super::*;

                zaino_testutils::validator_tests!(
                    $validator,
                    $kind,
                    address_balance => fetch_service_get_address_balance,
                    raw_mempool => fetch_service_get_raw_mempool,
                    mempool_info => test_get_mempool_info,
                    raw_transaction => fetch_service_get_raw_transaction,
                    address_tx_ids => fetch_service_get_address_tx_ids,
                    address_utxos => fetch_service_get_address_utxos,
                    block_range_no_pool_type_returns_sapling_orchard
                        => fetch_service_get_block_range_no_pools_returns_sapling_orchard,
                    block_range_returns_all_pools_when_requested
                        => fetch_service_get_block_range_returns_all_pools,
                    transaction_mined => fetch_service_get_transaction_mined,
                    transaction_mempool => fetch_service_get_transaction_mempool,
                    taddress_txids => fetch_service_get_taddress_txids,
                    taddress_balance => fetch_service_get_taddress_balance,
                    mempool_tx => fetch_service_get_mempool_tx,
                    mempool_stream => fetch_service_get_mempool_stream,
                    taddress_utxos => fetch_service_get_taddress_utxos,
                    taddress_utxos_stream => fetch_service_get_taddress_utxos_stream,
                );

                mod z {
                    use super::*;

                    zaino_testutils::validator_tests!(
                        $validator,
                        $kind,
                        treestate => fetch_service_z_get_treestate,
                        subtrees_by_index => fetch_service_z_get_subtrees_by_index,
                    );
                }
            }
        }
    };
}

fetch_service_tests!(
    zcashd,
    zcash_local_net::validator::zcashd::Zcashd,
    ValidatorKind::Zcashd
);
fetch_service_tests!(
    zebrad,
    zcash_local_net::validator::zebrad::Zebrad,
    ValidatorKind::Zebrad
);
