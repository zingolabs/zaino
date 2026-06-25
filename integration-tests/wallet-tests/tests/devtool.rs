//! Devtool-backed ports of the wallet-to-validator tests, run against a Zaino
//! launched by `TestManager`. As the zcash-devtool client
//! ([`wallet_tests::devtool`]) reaches capability parity with the zingolib
//! `Clients`, the matching tests in `wallet_to_validator.rs` migrate here and
//! the zingolib versions are eventually retired (zingolabs/infrastructure#269).
//!
//! Zebrad only: the zcashd matrix is deferred (see below).
//!
//! Covered (the full zebrad fetch + state query/send surface):
//! - sends to each pool, `send_to_all`, shielding, mining-reward receipt;
//! - `get_transaction` (mined / mempool), `get_raw_transaction`;
//! - mempool: `get_raw_mempool`, `get_mempool_tx`, `get_mempool_stream`;
//! - address queries: `get_address_tx_ids`, `get_address_utxos`,
//!   `get_address_balance`, the `get_taddress_*` family (recipient and
//!   faucet-coinbase variants), `get_address_transactions_regtest`;
//! - tree state: `z_get_treestate`, `z_get_subtrees_by_index`;
//! - block range: default/all pools and the out-of-range edge cases;
//! - compact-block transparent data;
//! - `connect_to_node_get_info` (wallet `get_info` smoke).
//! Dual `*_fetch_vs_state` tests assert the fetch and state backends agree.
//!
//! Deferred, with the capability each waits on:
//! - `send_to_transparent` (heavy / finalization) — runnable now via orchard
//!   funding, but the load-bearing 99-block advance across the finalized /
//!   non-finalized boundary costs a halo2 proof per block; waits on cheap
//!   filler-block mining (round-3 spec P2).
//! - `monitor_unverified_mempool` — unconfirmed (mempool) wallet balances;
//!   devtool sync is block-based (round-3 spec P3 — likely stays on zingolib,
//!   the indexer-side mempool views above already cover the surface).
//! - the zcashd matrix (`json_server`, zcashd send/query) — the devtool wallet
//!   rejects zcashd's default regtest activation heights at construction
//!   (round-2 spec P0); `json_server` is additionally zcashd-bound (its
//!   reference subscriber *is* zcashd).
//! - the `test_vectors` chain builder — transparent-coinbase shielding
//!   (round-2 spec P1).
//! - `get_mempool_info` — recomputes expected sizes from
//!   `FetchServiceSubscriber` internals; low value over the mempool surfaces
//!   already covered.
//!
//! Requires a `zcash-devtool` binary built with `--features regtest_support`
//! in `TEST_BINARIES_DIR`/`PATH`, alongside the usual validator binaries.

use wallet_tests::devtool::DevtoolClients;
use zaino_proto::proto::service::{
    AddressList, BlockId, BlockRange, GetAddressUtxosArg, TransparentAddressBlockFilter, TxFilter,
};
use zaino_state::{LightWalletIndexer, ZcashIndexer, ZcashService};
use zaino_testutils::{PollableTip, TestManager, TestService, ValidatorKind};
use zainodlib::error::IndexerError;
use zcash_local_net::validator::zebrad::Zebrad;
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_rpc::methods::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

/// Launch an orchard-mining zebrad + Zaino on the `Service` backend, build
/// devtool faucet/recipient wallets against it, mine `coinbase_blocks` blocks
/// past the launch, and sync the faucet. Each mined block past the launch is
/// an orchard coinbase note for the faucet (height 1 is the
/// sapling-activation coinbase), so `coinbase_blocks` is the number of
/// spendable orchard notes the faucet ends with — one per send a test makes
/// before mining (devtool will not chain unconfirmed change). The shared
/// launch+fund preamble of the ports below.
async fn launch_and_fund_faucet<Service>(
    coinbase_blocks: u32,
) -> (TestManager<Zebrad, Service>, DevtoolClients)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let test_manager = TestManager::<Zebrad, Service>::launch_mining_to(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        None,
        None,
        true,
        false,
        false,
    )
    .await
    .expect("launch TestManager");

    let mut clients = wallet_tests::devtool::build_clients(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    test_manager
        .generate_blocks_and_wait_for_tip(coinbase_blocks, test_manager.subscriber())
        .await;
    clients.sync_faucet().await;

    (test_manager, clients)
}

/// Launch an orchard-mining zebrad + Zaino on the `Service` backend and build
/// devtool faucet/recipient wallets against it, without mining or syncing — the
/// minimal preamble for tests that only exercise wallet↔server connectivity.
async fn launch_and_build_clients<Service>() -> (TestManager<Zebrad, Service>, DevtoolClients)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let test_manager = TestManager::<Zebrad, Service>::launch_mining_to(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        None,
        None,
        true,
        false,
        false,
    )
    .await
    .expect("launch TestManager");

    let clients = wallet_tests::devtool::build_clients(
        test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    (test_manager, clients)
}

/// Port of `connect_to_node_get_info` (wallet_to_validator, zebrad): the faucet
/// and recipient wallets can report node/server info without erroring. Smoke
/// test — the original discards the result. (`chain_name` is "test" for regtest
/// and `server_uri` has no trailing slash, so this asserts neither.)
async fn connect_to_node_get_info<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, clients) = launch_and_build_clients::<Service>().await;

    clients.get_info_faucet().await;
    clients.get_info_recipient().await;

    test_manager.close().await;
}

/// Port of `wallet_to_validator::check_received_mining_reward` (zebrad): the
/// faucet's synced wallet sees the orchard coinbase note.
async fn receives_mining_reward<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, clients) = launch_and_fund_faucet::<Service>(1).await;

    let faucet_balance = dbg!(clients.faucet_balance().await);
    assert!(
        faucet_balance.orchard_spendable > 0,
        "faucet should hold a spendable orchard coinbase note, got {faucet_balance:?}"
    );

    test_manager.close().await;
}

/// Port of the `assert_send_to_pool` family from `wallet_to_validator`
/// (zebrad): send 250_000 from the faucet (spending its orchard coinbase) to
/// the recipient's `pool` address, mine it in, and assert the recipient's
/// synced wallet shows the receipt in that pool. Covers `send_to_orchard`
/// (unified), `send_to_sapling`, and the basic transparent receipt — the
/// per-pool address wiring is the new surface under test. (The original
/// `send_to_transparent` additionally mines across the finalization boundary
/// under transparent mining; that variant is deferred, as it exercises the
/// transparent-coinbase detection path devtool has not yet been run against.)
async fn send_to_pool<Service>(pool: wallet_tests::Pool)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(1).await;

    let recipient = clients.get_recipient_address(pool.address_kind()).await;
    let txid = clients.send_from_faucet(&recipient, 250_000).await;
    dbg!(txid);

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    assert_eq!(
        pool.spendable_balance(&clients.recipient_balance().await),
        250_000
    );

    test_manager.close().await;
}

/// Port of `send_to_all` (wallet_to_validator, zebrad): one faucet funds a send
/// to all three pools at once; each recipient pool reports 250_000.
///
/// The original mined to a transparent miner and ran a 100-block "heavy mine"
/// after the sends; here the faucet is funded by orchard coinbase notes (the
/// mine-to-orchard funding pattern, no shield) and the sends are confirmed with
/// a single block. The 100-block mine is not load-bearing for the per-pool
/// balance assertions — the sends confirm in one block, and received funds are
/// regular (non-coinbase) outputs with no maturity rule — and 100 orchard
/// blocks would cost 100 halo2 proofs against the net-speedup criterion.
async fn send_to_all<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    // Three orchard notes — one per send (devtool will not chain unconfirmed change).
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(3).await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_zaddr = clients.get_recipient_address("sapling").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;
    clients.send_from_faucet(&recipient_zaddr, 250_000).await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    let balance = clients.recipient_balance().await;
    assert_eq!(
        wallet_tests::Pool::Orchard.spendable_balance(&balance),
        250_000
    );
    assert_eq!(
        wallet_tests::Pool::Sapling.spendable_balance(&balance),
        250_000
    );
    assert_eq!(
        wallet_tests::Pool::Transparent.spendable_balance(&balance),
        250_000
    );

    test_manager.close().await;
}

/// Port of `wallet_to_validator::shield_for_validator` (zebrad): the faucet
/// sends 250_000 to the recipient's transparent address; the recipient
/// confirms the transparent receipt, shields it to orchard, and confirms the
/// orchard balance net of the ZIP-317 fee (250_000 − 15_000 = 235_000 — the
/// first devtool exercise of `shield`, and the constant flagged as needing
/// re-verification against devtool's note selection).
async fn shield_for_validator<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(1).await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    assert_eq!(
        wallet_tests::Pool::Transparent.spendable_balance(&clients.recipient_balance().await),
        250_000
    );

    clients.shield_recipient().await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;
    clients.sync_recipient().await;

    assert_eq!(
        wallet_tests::Pool::Orchard.spendable_balance(&clients.recipient_balance().await),
        235_000
    );

    test_manager.close().await;
}

/// Launch, fund the faucet with two orchard notes, and broadcast (without
/// mining) one transparent and one unified send into the mempool. Returns the
/// manager and the two broadcast txids (transparent, then unified). The
/// devtool analogue of `fund_and_fill_mempool`.
async fn fund_and_fill_mempool<Service>() -> (TestManager<Zebrad, Service>, String, String)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    // Two orchard notes — one per unmined send.
    let (test_manager, mut clients) = launch_and_fund_faucet::<Service>(2).await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    let recipient_ua = clients.get_recipient_address("unified").await;
    let transparent_txid = clients.send_from_faucet(&recipient_taddr, 250_000).await;
    let unified_txid = clients.send_from_faucet(&recipient_ua, 250_000).await;

    // Allow the broadcaster and the indexer to observe the unmined transactions.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    (test_manager, transparent_txid, unified_txid)
}

/// Fund the faucet, send 250_000 to the recipient's `pool` address, mine it
/// in, and return the manager, clients (for address lookup), and the broadcast
/// txid hex. The txid is in display (reversed) order — devtool prints it that
/// way, which matches the txid strings zaino's address queries return, so no
/// conversion is needed for those comparisons.
async fn fund_and_send_to<Service>(
    pool: wallet_tests::Pool,
) -> (TestManager<Zebrad, Service>, DevtoolClients, String)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (test_manager, mut clients) = launch_and_fund_faucet::<Service>(1).await;

    let recipient = clients.get_recipient_address(pool.address_kind()).await;
    let txid_hex = clients.send_from_faucet(&recipient, 250_000).await;

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;

    (test_manager, clients, txid_hex)
}

/// Port of `fetch_service_get_address_tx_ids` (zebrad): after a transparent
/// send to the recipient, `get_address_tx_ids` over the recipient's taddr
/// returns the send's txid.
async fn get_address_tx_ids<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, clients, txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    // A range start a couple of blocks below the tip, covering the send block.
    let start = test_manager.subscriber().tip_height().await as u32 - 2;
    let txids = test_manager
        .subscriber()
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr],
            Some(start),
            None,
        ))
        .await
        .unwrap();

    dbg!(&txid_hex, &txids);
    assert_eq!(txid_hex.trim(), txids[0]);

    test_manager.close().await;
}

/// Port of `fetch_service_get_address_utxos` (zebrad): after a transparent
/// send to the recipient, `z_get_address_utxos` over the recipient's taddr
/// returns a utxo whose txid is the send's.
async fn get_address_utxos<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, clients, txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let utxos = test_manager
        .subscriber()
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();
    let (_, utxo_txid, ..) = utxos[0].into_parts();

    dbg!(&txid_hex, &utxo_txid);
    assert_eq!(txid_hex.trim(), utxo_txid.to_string());

    test_manager.close().await;
}

/// Port of `fetch_service_z_get_treestate` (zebrad, smoke): `z_get_treestate`
/// at the tip height succeeds.
async fn z_get_treestate<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, _clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Orchard).await;

    let tip = test_manager.subscriber().tip_height().await;
    dbg!(test_manager
        .subscriber()
        .z_get_treestate(tip.to_string())
        .await
        .unwrap());

    test_manager.close().await;
}

/// Port of `fetch_service_z_get_subtrees_by_index` (zebrad, smoke):
/// `z_get_subtrees_by_index` for orchard succeeds.
async fn z_get_subtrees_by_index<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, _clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Orchard).await;

    dbg!(test_manager
        .subscriber()
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap());

    test_manager.close().await;
}

/// Port of `fetch_service_get_raw_transaction` (zebrad, smoke):
/// `get_raw_transaction` for the orchard send's txid succeeds.
async fn get_raw_transaction<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, _clients, txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Orchard).await;

    dbg!(test_manager
        .subscriber()
        .get_raw_transaction(txid_hex.trim().to_string(), Some(1))
        .await
        .unwrap());

    test_manager.close().await;
}

/// Port of `fetch_service_get_taddress_txids` (zebrad, smoke):
/// `get_taddress_txids` over the recipient's taddr and a height range around
/// the send succeeds.
async fn get_taddress_txids<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    use futures::StreamExt as _;

    let (mut test_manager, clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let tip = test_manager.subscriber().tip_height().await;
    let block_filter = TransparentAddressBlockFilter {
        address: recipient_taddr,
        range: Some(BlockRange {
            start: Some(BlockId {
                height: tip - 2,
                hash: Vec::new(),
            }),
            end: Some(BlockId {
                height: tip,
                hash: Vec::new(),
            }),
            pool_types: zaino_testutils::all_pools_i32(),
        }),
    };

    let stream_items: Vec<_> = test_manager
        .subscriber()
        .get_taddress_txids(block_filter)
        .await
        .unwrap()
        .collect()
        .await;
    let txids: Vec<_> = stream_items.into_iter().filter_map(|r| r.ok()).collect();
    dbg!(&txids);

    test_manager.close().await;
}

/// Port of `fetch_service_get_taddress_utxos` (zebrad, smoke):
/// `get_address_utxos` over the recipient's taddr succeeds.
async fn get_taddress_utxos<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let utxos_arg = GetAddressUtxosArg {
        addresses: vec![recipient_taddr],
        start_height: 0,
        max_entries: 0,
    };
    dbg!(test_manager
        .subscriber()
        .get_address_utxos(utxos_arg)
        .await
        .unwrap());

    test_manager.close().await;
}

/// Port of `fetch_service_get_taddress_utxos_stream` (zebrad, smoke):
/// `get_address_utxos_stream` over the recipient's taddr succeeds.
async fn get_taddress_utxos_stream<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    use futures::StreamExt as _;

    let (mut test_manager, clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let utxos_arg = GetAddressUtxosArg {
        addresses: vec![recipient_taddr],
        start_height: 0,
        max_entries: 0,
    };
    let stream_items: Vec<_> = test_manager
        .subscriber()
        .get_address_utxos_stream(utxos_arg)
        .await
        .unwrap()
        .collect()
        .await;
    let utxos: Vec<_> = stream_items.into_iter().filter_map(|r| r.ok()).collect();
    dbg!(utxos);

    test_manager.close().await;
}

/// Port of `fetch_service_get_raw_mempool` (zebrad): with two transactions
/// broadcast but unmined, the indexer's `get_raw_mempool` matches the
/// validator's own JSON-RPC `getrawmempool`.
async fn get_raw_mempool<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, _transparent_txid, _unified_txid) =
        fund_and_fill_mempool::<Service>().await;

    let json_service = test_manager.full_node_jsonrpc_connector().await;

    let mut zaino_mempool = test_manager.subscriber().get_raw_mempool().await.unwrap();
    let mut validator_mempool = json_service.get_raw_mempool().await.unwrap().transactions;

    dbg!(&zaino_mempool);
    dbg!(&validator_mempool);
    zaino_mempool.sort();
    validator_mempool.sort();
    assert_eq!(validator_mempool, zaino_mempool);

    test_manager.close().await;
}

/// Port of `fetch_service_get_mempool_tx` (zebrad): the `get_mempool_tx`
/// stream returns the two unmined transactions keyed by txid (internal byte
/// order), and the exclude-by-txid-suffix filter drops the named one.
async fn get_mempool_tx<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    use futures::StreamExt as _;
    use zaino_proto::proto::service::GetMempoolTxRequest;

    let (mut test_manager, transparent_txid, unified_txid) = fund_and_fill_mempool::<Service>().await;

    let to_bytes = |hex: &str| -> [u8; 32] {
        wallet_tests::devtool::txid_internal_bytes(hex)
            .try_into()
            .expect("txid is 32 bytes")
    };
    let mut sorted_txids = [to_bytes(&transparent_txid), to_bytes(&unified_txid)];
    sorted_txids.sort();

    let subscriber = test_manager.subscriber().clone();
    let collect = |req: GetMempoolTxRequest| {
        let subscriber = subscriber.clone();
        async move {
            let stream_items: Vec<_> = subscriber.get_mempool_tx(req).await.unwrap().collect().await;
            let mut txs: Vec<_> = stream_items.into_iter().filter_map(|r| r.ok()).collect();
            txs.sort_by_key(|tx| tx.txid.clone());
            txs
        }
    };

    // Both transactions present, no exclusions.
    let all = collect(GetMempoolTxRequest {
        exclude_txid_suffixes: Vec::new(),
        pool_types: Vec::new(),
    })
    .await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].txid, sorted_txids[0]);
    assert_eq!(all[1].txid, sorted_txids[1]);

    // Excluding the first by its txid suffix leaves only the second.
    let remaining = collect(GetMempoolTxRequest {
        exclude_txid_suffixes: vec![sorted_txids[0][8..].to_vec()],
        pool_types: Vec::new(),
    })
    .await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].txid, sorted_txids[1]);

    test_manager.close().await;
}

/// Port of `fetch_service_get_mempool_stream` (zebrad): a `get_mempool_stream`
/// subscription opened before two sends observes those unmined transactions.
/// Smoke test — the original only `dbg!`s the collected stream.
async fn get_mempool_stream<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    use futures::StreamExt as _;

    // Two orchard notes — one per unmined send.
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(2).await;

    // Subscribe before the sends so the stream observes them entering the mempool.
    let subscriber = test_manager.subscriber().clone();
    let stream_handle = tokio::spawn(async move {
        let stream = subscriber.get_mempool_stream().await.unwrap();
        let items: Vec<_> = stream.collect().await;
        items
            .into_iter()
            .filter_map(|result| result.ok())
            .collect::<Vec<_>>()
    });

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;

    let mempool_tx = stream_handle.await.unwrap();

    let mut sorted_mempool_tx = mempool_tx.clone();
    sorted_mempool_tx.sort_by_key(|tx| tx.data.clone());

    dbg!(sorted_mempool_tx);

    test_manager.close().await;
}

/// Fund the faucet, send 250_000 to the recipient's unified address, and mine
/// it in. Returns the manager and the broadcast txid in zaino's internal byte
/// order. The devtool analogue of `fund_and_send(Pool::Orchard)`; the wallet
/// clients are dropped once the transaction is mined (the queries below hit
/// zaino, not the wallet).
async fn fund_and_send_orchard<Service>() -> (TestManager<Zebrad, Service>, Vec<u8>)
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (test_manager, mut clients) = launch_and_fund_faucet::<Service>(1).await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let txid_hex = clients.send_from_faucet(&recipient_ua, 250_000).await;

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;

    (test_manager, wallet_tests::devtool::txid_internal_bytes(&txid_hex))
}

/// Port of `fetch_service_get_transaction_mined` (zebrad): the indexer serves
/// `get_transaction` for the mined orchard send, keyed by its txid. Also
/// confirms `txid_internal_bytes` yields the order the indexer matches on.
async fn get_transaction_mined<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, txid_bytes) = fund_and_send_orchard::<Service>().await;

    let tx_filter = TxFilter {
        block: None,
        index: 0,
        hash: txid_bytes,
    };
    let raw_transaction = test_manager
        .subscriber()
        .get_transaction(tx_filter)
        .await
        .unwrap();
    dbg!(raw_transaction);

    test_manager.close().await;
}

/// Port of `fetch_service_get_transaction_mempool` (zebrad): the indexer
/// serves `get_transaction` for an orchard send that is broadcast but not
/// mined, keyed by its txid — i.e. from the mempool.
async fn get_transaction_mempool<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(1).await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let txid_hex = clients.send_from_faucet(&recipient_ua, 250_000).await;
    let tx_filter = TxFilter {
        block: None,
        index: 0,
        hash: wallet_tests::devtool::txid_internal_bytes(&txid_hex),
    };

    // Let the broadcaster and the indexer observe the unmined transaction.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let raw_transaction = test_manager
        .subscriber()
        .get_transaction(tx_filter)
        .await
        .unwrap();
    dbg!(raw_transaction);

    test_manager.close().await;
}

/// Port of `state_service_get_block_range_returns_default_pools` (zebrad):
/// fund the faucet, send 250_000 to the recipient's unified address, mine it
/// in, then assert that `get_block_range` with no pools requested returns the
/// same shielded compact blocks as requesting sapling+orchard, that the
/// fetch- and state-service indexers agree, and that the tip block holds the
/// shielded coinbase and the send with no transparent data.
async fn block_range_returns_default_pools() {
    let mut svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    let mut clients = wallet_tests::devtool::build_clients(
        svc.test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    // fund_and_send(Orchard): one orchard coinbase note, then send it to the
    // recipient's unified address and mine the send in.
    svc.generate_blocks_and_wait_for_tips(1).await;
    clients.sync_faucet().await;
    let recipient_ua = clients.get_recipient_address("unified").await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;
    svc.generate_blocks_and_wait_for_tips(1).await;

    let start_height: u64 = 1;
    let end_height: u64 = svc.fetch_subscriber.tip_height().await;

    let fetch_default = zaino_testutils::collect_block_range(
        &svc.fetch_subscriber,
        start_height,
        end_height,
        vec![],
    )
    .await;
    let fetch_shielded = zaino_testutils::collect_block_range(
        &svc.fetch_subscriber,
        start_height,
        end_height,
        zaino_testutils::shielded_pools_i32(),
    )
    .await;
    assert_eq!(fetch_default, fetch_shielded);

    let state_shielded = zaino_testutils::collect_block_range(
        &svc.state_subscriber,
        start_height,
        end_height,
        zaino_testutils::shielded_pools_i32(),
    )
    .await;
    let state_default = zaino_testutils::collect_block_range(
        &svc.state_subscriber,
        start_height,
        end_height,
        vec![],
    )
    .await;
    assert_eq!(state_default, state_shielded);

    assert_eq!(fetch_default, state_default);

    let compact_block = state_default.last().unwrap();
    assert_eq!(compact_block.height, end_height);
    // The tip block holds the shielded coinbase (the miner address is a
    // shielded pool) and the send.
    assert_eq!(compact_block.vtx.len(), 2);
    assert_eq!(compact_block.vtx.last().unwrap().index, 1);
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

    svc.test_manager.close().await;
}

/// Port of `state_service_get_block_range_returns_all_pools` (zebrad): fund
/// the faucet with three orchard coinbase notes, send 250_000 to the
/// recipient's transparent, sapling, and unified addresses (one tx each),
/// mine them into one block, then assert `get_block_range` with all pools
/// requested agrees between the fetch and state indexers and that the tip
/// block carries the coinbase plus all three sends with their pool data.
async fn block_range_returns_all_pools() {
    let mut svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    let mut clients = wallet_tests::devtool::build_clients(
        svc.test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    // Three orchard coinbase notes (one per send below — devtool will not
    // chain unconfirmed change), then one send to each pool's recipient
    // address, mined into a single block.
    svc.generate_blocks_and_wait_for_tips(3).await;
    clients.sync_faucet().await;

    let recipient_t = clients.get_recipient_address("transparent").await;
    let recipient_s = clients.get_recipient_address("sapling").await;
    let recipient_u = clients.get_recipient_address("unified").await;
    let deshielding_txid = clients.send_from_faucet(&recipient_t, 250_000).await;
    let sapling_txid = clients.send_from_faucet(&recipient_s, 250_000).await;
    let orchard_txid = clients.send_from_faucet(&recipient_u, 250_000).await;
    svc.generate_blocks_and_wait_for_tips(1).await;

    let start_height: u64 = 1;
    let end_height: u64 = svc.fetch_subscriber.tip_height().await;
    let all_pools = zaino_testutils::all_pools_i32();

    let fetch_range = zaino_testutils::collect_block_range(
        &svc.fetch_subscriber,
        start_height,
        end_height,
        all_pools.clone(),
    )
    .await;
    let state_range = zaino_testutils::collect_block_range(
        &svc.state_subscriber,
        start_height,
        end_height,
        all_pools,
    )
    .await;
    assert_eq!(fetch_range, state_range);

    let compact_block = state_range.last().unwrap();
    assert_eq!(compact_block.height, end_height);
    // coinbase + the three sends
    assert_eq!(compact_block.vtx.len(), 4);

    wallet_tests::assert_pool_present(
        compact_block,
        &wallet_tests::devtool::txid_from_devtool(&deshielding_txid),
        wallet_tests::Pool::Transparent,
    );
    wallet_tests::assert_pool_present(
        compact_block,
        &wallet_tests::devtool::txid_from_devtool(&sapling_txid),
        wallet_tests::Pool::Sapling,
    );
    wallet_tests::assert_pool_present(
        compact_block,
        &wallet_tests::devtool::txid_from_devtool(&orchard_txid),
        wallet_tests::Pool::Orchard,
    );

    svc.test_manager.close().await;
}

/// Launch dual fetch+state services, fund the faucet, send 250_000 to the
/// recipient's `pool` address, and mine it in. Returns the services, the
/// broadcast txid hex (display order), and the recipient address. The devtool
/// analogue of the state_service `fund_and_send` (dual-subscriber); the wallet
/// clients are dropped once the send is mined (the queries below hit zaino).
/// Port of `fetch_service_get_address_balance` (zebrad): after a transparent
/// send of 250_000 to the recipient, `z_get_address_balance` over that taddr
/// reports exactly 250_000.
async fn get_address_balance<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let balance = test_manager
        .subscriber()
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();

    dbg!(balance);
    // The fixture sent exactly 250_000 to the recipient taddr.
    assert_eq!(balance.balance(), 250_000);

    test_manager.close().await;
}

/// Port of `fetch_service_get_taddress_balance` (zebrad): after a transparent
/// send of 250_000 to the recipient, `get_taddress_balance` over that taddr
/// reports value_zat 250_000.
async fn get_taddress_balance<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip + LightWalletIndexer,
{
    let (mut test_manager, clients, _txid_hex) =
        fund_and_send_to::<Service>(wallet_tests::Pool::Transparent).await;
    let recipient_taddr = clients.get_recipient_address("transparent").await;

    let address_list = AddressList {
        addresses: vec![recipient_taddr],
    };
    let balance = test_manager
        .subscriber()
        .get_taddress_balance(address_list)
        .await
        .unwrap();

    dbg!(&balance);
    // The fixture sent exactly 250_000 to the recipient taddr.
    assert_eq!(balance.value_zat, 250_000);

    test_manager.close().await;
}

async fn fund_and_send_dual(
    pool: wallet_tests::Pool,
) -> (zaino_testutils::StateAndFetchServices<Zebrad>, String, String) {
    let svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    let mut clients = wallet_tests::devtool::build_clients(
        svc.test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    svc.generate_blocks_and_wait_for_tips(1).await;
    clients.sync_faucet().await;
    let recipient = clients.get_recipient_address(pool.address_kind()).await;
    let txid_hex = clients.send_from_faucet(&recipient, 250_000).await;
    svc.generate_blocks_and_wait_for_tips(1).await;

    (svc, txid_hex, recipient)
}

/// Port of `state_service_z_get_treestate` (zebrad): the fetch and state
/// indexers agree on `z_get_treestate` at the tip.
async fn z_get_treestate_fetch_vs_state() {
    let (mut svc, _txid_hex, _addr) = fund_and_send_dual(wallet_tests::Pool::Orchard).await;

    let tip = svc.fetch_subscriber.tip_height().await;
    let fetch = svc
        .fetch_subscriber
        .z_get_treestate(tip.to_string())
        .await
        .unwrap();
    let state = svc
        .state_subscriber
        .z_get_treestate(tip.to_string())
        .await
        .unwrap();
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Port of `state_service_z_get_subtrees_by_index` (zebrad): the fetch and
/// state indexers agree on `z_get_subtrees_by_index` for orchard.
async fn z_get_subtrees_by_index_fetch_vs_state() {
    let (mut svc, _txid_hex, _addr) = fund_and_send_dual(wallet_tests::Pool::Orchard).await;

    let fetch = svc
        .fetch_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap();
    let state = svc
        .state_subscriber
        .z_get_subtrees_by_index("orchard".to_string(), NoteCommitmentSubtreeIndex(0), None)
        .await
        .unwrap();
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Port of `state_service_get_raw_transaction` (zebrad): the fetch and state
/// indexers agree on `get_raw_transaction` for the orchard send's txid.
async fn get_raw_transaction_fetch_vs_state() {
    let (mut svc, txid_hex, _addr) = fund_and_send_dual(wallet_tests::Pool::Orchard).await;
    let txid = txid_hex.trim().to_string();

    let fetch = svc
        .fetch_subscriber
        .get_raw_transaction(txid.clone(), Some(1))
        .await
        .unwrap();
    let state = svc
        .state_subscriber
        .get_raw_transaction(txid, Some(1))
        .await
        .unwrap();
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Port of `state_service_get_address_tx_ids` (zebrad): `get_address_tx_ids`
/// over the recipient's taddr returns the send's txid, and the fetch and state
/// indexers agree.
async fn get_address_tx_ids_fetch_vs_state() {
    let (mut svc, txid_hex, recipient_taddr) =
        fund_and_send_dual(wallet_tests::Pool::Transparent).await;

    let tip = svc.fetch_subscriber.tip_height().await;
    let start = Some((tip - 2) as u32);
    let end = Some(tip as u32);
    let fetch = svc
        .fetch_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(
            vec![recipient_taddr.clone()],
            start,
            end,
        ))
        .await
        .unwrap();
    let state = svc
        .state_subscriber
        .get_address_tx_ids(GetAddressTxIdsRequest::new(vec![recipient_taddr], start, end))
        .await
        .unwrap();
    assert_eq!(txid_hex.trim(), fetch[0]);
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Port of `state_service_get_address_utxos` (zebrad): `z_get_address_utxos`
/// over the recipient's taddr returns the send's txid, and the fetch and state
/// indexers agree on it.
async fn get_address_utxos_fetch_vs_state() {
    let (mut svc, txid_hex, recipient_taddr) =
        fund_and_send_dual(wallet_tests::Pool::Transparent).await;

    let fetch_utxos = svc
        .fetch_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let (_, fetch_txid, ..) = fetch_utxos[0].into_parts();
    let state_utxos = svc
        .state_subscriber
        .z_get_address_utxos(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();
    let (_, state_txid, ..) = state_utxos[0].into_parts();

    assert_eq!(txid_hex.trim(), fetch_txid.to_string());
    assert_eq!(fetch_txid.to_string(), state_txid.to_string());

    svc.test_manager.close().await;
}

/// Dual-backend analogue of [`fund_and_fill_mempool`]: launch state+fetch
/// services, fund the faucet with two orchard notes, then broadcast a
/// transparent and a unified send (unmined) so both indexers' mempools hold
/// them. Returns the services.
async fn fund_and_fill_mempool_dual() -> zaino_testutils::StateAndFetchServices<Zebrad> {
    let svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        zaino_testutils::SHIELDED_FUNDING_POOL,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    let mut clients = wallet_tests::devtool::build_clients(
        svc.test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    // Two orchard notes — one per unmined send.
    svc.generate_blocks_and_wait_for_tips(2).await;
    clients.sync_faucet().await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    let recipient_ua = clients.get_recipient_address("unified").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    clients.send_from_faucet(&recipient_ua, 250_000).await;

    // Allow the broadcaster and the indexers to observe the unmined transactions.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    svc
}

/// Port of `state_service_get_raw_mempool` (zebrad): the fetch and state
/// indexers agree on `get_raw_mempool` while two sends sit unmined.
async fn get_raw_mempool_fetch_vs_state() {
    let mut svc = fund_and_fill_mempool_dual().await;

    let mut fetch_service_mempool = svc.fetch_subscriber.get_raw_mempool().await.unwrap();
    let mut state_service_mempool = svc.state_subscriber.get_raw_mempool().await.unwrap();

    dbg!(&fetch_service_mempool);
    fetch_service_mempool.sort();

    dbg!(&state_service_mempool);
    state_service_mempool.sort();

    assert_eq!(fetch_service_mempool, state_service_mempool);

    svc.test_manager.close().await;
}

/// Port of `state_service_get_address_transactions_regtest` (zebrad): after a
/// transparent send to the recipient, the state indexer's
/// `get_taddress_transactions` over that taddr yields at least one transaction.
async fn get_address_transactions_regtest() {
    use futures::StreamExt as _;

    let (mut svc, _txid_hex, recipient_taddr) =
        fund_and_send_dual(wallet_tests::Pool::Transparent).await;

    let chain_height = svc.fetch_subscriber.tip_height().await;
    dbg!(&chain_height);

    let state_service_txids = svc
        .state_subscriber
        .get_taddress_transactions(TransparentAddressBlockFilter {
            address: recipient_taddr,
            range: Some(BlockRange {
                start: Some(BlockId {
                    height: chain_height - 2,
                    hash: vec![],
                }),
                end: Some(BlockId {
                    height: chain_height,
                    hash: vec![],
                }),
                pool_types: zaino_testutils::all_pools_i32(),
            }),
        })
        .await
        .unwrap();

    assert!(state_service_txids.count().await > 0);

    svc.test_manager.close().await;
}

/// Port of `state_service_…::get_transparent_data_from_compact_block_when_requested`
/// (zebrad): with transparent mining, every compact-block tx carries a
/// transparent vout (the miner's transparent coinbase is the data source), so
/// each vout's `script_pub_key` is non-empty. Needs no wallet client — a pure
/// indexer-against-a-transparent-mined-chain check, so it launches the dual
/// services directly rather than through a `DevtoolClients` fixture.
async fn transparent_data_in_compact_block() {
    let mut services = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        // The assertion below requires every tx to carry a transparent vout;
        // the miner's transparent coinbase is that data source, so coinbase
        // must land on the miner taddr.
        zaino_testutils::PoolType::Transparent,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    services.generate_blocks_and_wait_for_tips(5).await;

    let chain_height = services
        .state_subscriber
        .get_latest_block()
        .await
        .unwrap()
        .height;

    // NOTE / TODO: Zaino can not currently serve non standard script types in
    // compact blocks, because of this it does not return the script pub key for
    // the coinbase transaction of the genesis block. For this reason this test
    // currently does not fetch the genesis block (start height 1, not 0).
    // Issue: https://github.com/zingolabs/zaino/issues/818
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

    services.test_manager.close().await;
}

/// Port of `state_service_get_address_balance` (zebrad): the recipient taddr
/// reports the 250_000 send, and the fetch and state indexers agree.
async fn get_address_balance_fetch_vs_state() {
    let (mut svc, _txid_hex, recipient_taddr) =
        fund_and_send_dual(wallet_tests::Pool::Transparent).await;

    let fetch = svc
        .fetch_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr.clone()]))
        .await
        .unwrap();
    let state = svc
        .state_subscriber
        .z_get_address_balance(GetAddressBalanceRequest::new(vec![recipient_taddr]))
        .await
        .unwrap();

    // The fixture sent exactly 250_000 to the recipient taddr.
    assert_eq!(fetch.balance(), 250_000);
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Launch transparent-mining state+fetch services, build the devtool faucet,
/// mine `blocks` coinbase blocks to its transparent address, and return the
/// services and that taddr — the dual-backend, devtool analogue of
/// `launch_and_build_faucet_request`'s preamble for the faucet-taddr query
/// tests. The faucet taddr is the abandon-art transparent receiver, which is
/// also the zebrad miner address under transparent mining; the non-vacuity
/// probes in the callers verify that equality empirically (devtool round-2
/// spec P1(1)).
async fn launch_transparent_and_faucet_taddr(
    blocks: u32,
) -> (zaino_testutils::StateAndFetchServices<Zebrad>, String) {
    let svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        // These tests query the faucet taddr, which only coinbase funds —
        // mining must stay transparent or the queries compare empty against
        // empty.
        zaino_testutils::PoolType::Transparent,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    let clients = wallet_tests::devtool::build_clients(
        svc.test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    let faucet_taddr = clients.get_faucet_address("transparent").await;
    svc.generate_blocks_and_wait_for_tips(blocks).await;

    (svc, faucet_taddr)
}

/// Port of `state_service_…::get_taddress_txids` (zebrad, faucet-taddr cluster):
/// the fetch and state indexers agree on `get_address_tx_ids` over the faucet's
/// coinbase taddr. The non-vacuity probe (`!txids.is_empty()`) guards against a
/// silent empty==empty pass and confirms the devtool faucet's transparent
/// receiver equals the zebrad miner address (devtool round-2 spec P1(1)).
async fn get_taddress_txids_faucet_fetch_vs_state() {
    let (mut svc, faucet_taddr) = launch_transparent_and_faucet_taddr(100).await;

    let request = GetAddressTxIdsRequest::new(vec![faucet_taddr], Some(2), Some(5));
    let state_service_taddress_txids = svc
        .state_subscriber
        .get_address_tx_ids(request.clone())
        .await
        .unwrap();
    dbg!(&state_service_taddress_txids);
    let fetch_service_taddress_txids = svc
        .fetch_subscriber
        .get_address_tx_ids(request)
        .await
        .unwrap();
    dbg!(&fetch_service_taddress_txids);
    // Non-vacuity probe: the faucet taddr must actually hold coinbase txids in
    // range, else the fetch==state assert would pass against empty == empty.
    assert!(!fetch_service_taddress_txids.is_empty());
    assert_eq!(fetch_service_taddress_txids, state_service_taddress_txids);

    svc.test_manager.close().await;
}

/// Port of `state_service_…::get_taddress_balance` (zebrad, faucet-taddr
/// cluster): the fetch and state indexers agree on `get_taddress_balance` over
/// the faucet's coinbase taddr. The non-vacuity probe (`value_zat > 0`) guards
/// against a silent 0==0 pass and confirms the address equality of round-2
/// spec P1(1).
async fn get_taddress_balance_faucet_fetch_vs_state() {
    let (mut svc, faucet_taddr) = launch_transparent_and_faucet_taddr(5).await;

    let request = AddressList {
        addresses: vec![faucet_taddr],
    };
    let state_service_taddress_balance = svc
        .state_subscriber
        .get_taddress_balance(request.clone())
        .await
        .unwrap();
    let fetch_service_taddress_balance = svc
        .fetch_subscriber
        .get_taddress_balance(request)
        .await
        .unwrap();
    // Non-vacuity probe: the faucet taddr must actually hold coinbase value,
    // else the fetch==state assert would pass against 0 == 0.
    assert!(fetch_service_taddress_balance.value_zat > 0);
    assert_eq!(
        fetch_service_taddress_balance,
        state_service_taddress_balance
    );

    svc.test_manager.close().await;
}

/// Port of `state_service_…::get_address_utxos` (faucet cluster, zebrad): the
/// fetch and state indexers agree on `get_address_utxos` over the faucet's
/// coinbase taddr. The non-vacuity probe replaces the original's zingolib
/// `transaction_summaries` wallet cross-check (devtool has no transaction
/// listing) and confirms the faucet taddr actually holds coinbase utxos.
async fn get_address_utxos_faucet_fetch_vs_state() {
    let (mut svc, faucet_taddr) = launch_transparent_and_faucet_taddr(5).await;

    let request = GetAddressUtxosArg {
        addresses: vec![faucet_taddr],
        start_height: 2,
        max_entries: 3,
    };
    let fetch = svc
        .fetch_subscriber
        .get_address_utxos(request.clone())
        .await
        .unwrap();
    let state = svc
        .state_subscriber
        .get_address_utxos(request)
        .await
        .unwrap();

    assert!(!fetch.address_utxos.is_empty());
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Port of `state_service_…::get_address_utxos_stream` (faucet cluster, zebrad):
/// the streamed `get_address_utxos_stream` agrees between the fetch and state
/// indexers over the faucet's coinbase taddr.
async fn get_address_utxos_stream_faucet_fetch_vs_state() {
    use futures::StreamExt as _;

    let (mut svc, faucet_taddr) = launch_transparent_and_faucet_taddr(5).await;

    let request = GetAddressUtxosArg {
        addresses: vec![faucet_taddr],
        start_height: 2,
        max_entries: 3,
    };
    let fetch = svc
        .fetch_subscriber
        .get_address_utxos_stream(request.clone())
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;
    let state = svc
        .state_subscriber
        .get_address_utxos_stream(request)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;

    assert!(!fetch.is_empty());
    assert_eq!(fetch, state);

    svc.test_manager.close().await;
}

/// Launch transparent-mining state+fetch services and mine up to chain height
/// 100 — the dual-backend, devtool analogue of `launch_transparent_with_known_tip`.
/// The block-range edge tests need a known 100-block tip and no wallet client.
async fn launch_transparent_to_height_100() -> zaino_testutils::StateAndFetchServices<Zebrad> {
    let svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        zaino_testutils::PoolType::Transparent,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;

    // The launch already generates blocks; only generate up to height 100.
    let chain_height = svc
        .state_subscriber
        .get_latest_block()
        .await
        .unwrap()
        .height as u32;
    svc.generate_blocks_and_wait_for_tips(100 - chain_height).await;

    svc
}

/// Port of `state_service_get_block_range_out_of_range_test_upper_bound`
/// (zebrad): draining [1, 106] on a 100-block chain yields the 100 available
/// blocks (fetch == state) and then errors rather than ending cleanly.
async fn get_block_range_out_of_range_upper_bound() {
    let mut services = launch_transparent_to_height_100().await;

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

/// Port of `state_service_get_block_range_out_of_range_test_lower_bound`
/// (zebrad): draining the inverted range [106, 1] yields no blocks (fetch ==
/// state, both empty) and then errors rather than ending cleanly.
async fn get_block_range_out_of_range_lower_bound() {
    let mut services = launch_transparent_to_height_100().await;

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

    services.test_manager.close().await;
}

/// Port of `send_to_transparent` (wallet_to_validator, heavy/finalization,
/// zebrad): a transparent send to the recipient returns the same address txids
/// whether served from the non-finalized chain (just mined) or after the
/// 99-block advance pushes the send past the finalization boundary
/// (NON_FINALIZED_DEPTH = 100).
///
/// `#[ignore]`d (gated): the load-bearing 99-block advance mines orchard
/// coinbase here — the devtool faucet funds via orchard, since the original's
/// transparent-mine + shield path needs devtool transparent-coinbase shielding
/// (round-2 P1) — so it costs ~99 halo2 proofs, against the net-speedup
/// criterion. The miner pool is fixed at launch and `generate_blocks` has no
/// per-call override, so the advance can't be cheap transparent filler until
/// per-call filler mining (round-3 P2) lands; un-ignore and mine the advance to
/// a transparent throwaway then.
async fn send_to_transparent_finalization<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(1).await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;

    let fetch_service = test_manager.full_node_jsonrpc_connector().await;
    let height = fetch_service.get_blockchain_info().await.unwrap().blocks.0;
    let unfinalised_transactions = fetch_service
        .get_address_txids(vec![recipient_taddr.clone()], height, height)
        .await
        .unwrap();

    // The load-bearing advance: 99 blocks push the send below NON_FINALIZED_DEPTH
    // into the finalized DB. Orchard coinbase here (see #[ignore] rationale).
    test_manager
        .generate_blocks_bulk_and_wait_for_tips(
            99,
            test_manager.subscriber(),
            test_manager.subscriber(),
        )
        .await;

    let finalised_transactions = fetch_service
        .get_address_txids(vec![recipient_taddr], height, height)
        .await
        .unwrap();

    clients.sync_recipient().await;
    assert_eq!(
        wallet_tests::Pool::Transparent.spendable_balance(&clients.recipient_balance().await),
        250_000
    );
    assert_eq!(unfinalised_transactions, finalised_transactions);

    test_manager.close().await;
}

/// Port of `zebra::get::address_deltas` (zebrad): `getaddressdeltas` over a
/// transparent-mined chain where the faucet shields its matured transparent
/// coinbase, then sends transparent to the recipient.
///
/// Gated `devtool-incompatible` (the wrapper in `mod zebrad` carries the
/// `#[ignore]`): the `shield_faucet` step below is blocked by a confirmed
/// devtool bug. `shield` drains *all* the faucet's transparent funds, so it
/// always includes the just-mined `tip-99` coinbase; devtool computes coinbase
/// maturity against the target height (tip+1) while zebra's mempool enforces it
/// against the current tip, so that coinbase is immature by exactly one block
/// and zebra rejects the shield ("immature transparent coinbase spend"). Mining
/// more cannot fix it — the boundary tracks the tip. The bounded `send` tests
/// pass because a send selects only enough *oldest, mature* coinbase to cover
/// the amount, never reaching the immature tip. Heights are derived from the
/// live chain (not the original's hardcoded 102/104) so that once devtool fixes
/// the off-by-one, the only failure mode is the shield, not setup-height drift.
async fn address_deltas() {
    use zaino_fetch::jsonrpsee::response::address_deltas::{
        GetAddressDeltasParams, GetAddressDeltasResponse,
    };

    const NON_EXISTENT_ADDRESS: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";

    let mut svc = zaino_testutils::launch_state_and_fetch_services_mining_to::<Zebrad>(
        // The deltas under test are the faucet taddr's coinbase credits and the
        // shield's debit — coinbase must land on the miner taddr.
        zaino_testutils::PoolType::Transparent,
        &ValidatorKind::Zebrad,
        None,
        true,
        Some(zebra_chain::parameters::NetworkKind::Regtest),
    )
    .await;
    let mut clients = wallet_tests::devtool::build_clients(
        svc.test_manager
            .zaino_grpc_listen_address
            .expect("zaino enabled")
            .port(),
    )
    .await;

    let recipient_taddr = clients.get_recipient_address("transparent").await;
    let faucet_taddr = clients.get_faucet_address("transparent").await;

    // Mature the faucet's transparent coinbase past the 100-block maturity, then
    // shield it (the shield's debit on the faucet taddr is a delta under test,
    // and shielding transparent coinbase is the devtool capability exercised).
    // Mine generously: the faucet's earliest coinbase sits a few blocks past
    // genesis, so its 100-block maturity needs the tip well above height ~110;
    // 150 leaves comfortable margin regardless of the launch's startup height.
    svc.generate_blocks_and_wait_for_tips(150).await;
    clients.sync_faucet().await;
    clients.shield_faucet().await;
    svc.generate_blocks_and_wait_for_tips(1).await;

    clients.send_from_faucet(&recipient_taddr, 250_000).await;
    svc.generate_blocks_and_wait_for_tips(1).await;
    clients.sync_recipient().await;

    // Derived heights: the send lands in the tip block.
    let chain_tip = svc.fetch_subscriber.tip_height().await as u32;
    let tx_height = chain_tip;
    let height_beyond_tip = chain_tip + 100;

    // 1) Simple query (single address) -> Simple variant with the send delta.
    let response = svc
        .state_subscriber
        .get_address_deltas(GetAddressDeltasParams::Address(recipient_taddr.clone()))
        .await
        .unwrap();
    let GetAddressDeltasResponse::Simple(deltas) = response else {
        panic!("Expected Simple variant");
    };
    let recipient_delta = deltas
        .iter()
        .find(|d| d.height >= tx_height)
        .expect("Should find recipient transaction delta");
    assert_eq!(recipient_delta.index, 0, "Expected output index 0");

    // 2) Filtered with start=0 -> Simple variant, deltas from both addresses.
    let response = svc
        .state_subscriber
        .get_address_deltas(GetAddressDeltasParams::Filtered {
            addresses: vec![recipient_taddr.clone(), faucet_taddr.clone()],
            start: 0,
            end: chain_tip,
            chain_info: true,
        })
        .await
        .unwrap();
    let GetAddressDeltasResponse::Simple(deltas) = response else {
        panic!("Expected Simple variant for start=0");
    };
    assert!(deltas.len() >= 2, "Expected deltas from multiple addresses");

    // 3) Filtered with start>0 and chain_info -> WithChainInfo variant.
    let response = svc
        .state_subscriber
        .get_address_deltas(GetAddressDeltasParams::Filtered {
            addresses: vec![recipient_taddr.clone(), faucet_taddr.clone()],
            start: 1,
            end: chain_tip,
            chain_info: true,
        })
        .await
        .unwrap();
    let GetAddressDeltasResponse::WithChainInfo { deltas, start, end } = response else {
        panic!("Expected WithChainInfo variant");
    };
    assert!(!deltas.is_empty(), "Expected deltas with chain info");
    assert_eq!(start.height, 1, "Start block should match request");
    assert_eq!(end.height, chain_tip, "End block should match request");

    // 4) Height clamping: end beyond the tip is clamped down to the tip.
    let response = svc
        .state_subscriber
        .get_address_deltas(GetAddressDeltasParams::Filtered {
            addresses: vec![recipient_taddr, faucet_taddr],
            start: 1,
            end: height_beyond_tip,
            chain_info: true,
        })
        .await
        .unwrap();
    let GetAddressDeltasResponse::WithChainInfo { deltas, start, end } = response else {
        panic!("Expected WithChainInfo variant");
    };
    assert!(!deltas.is_empty(), "Expected deltas with clamped range");
    assert_eq!(start.height, 1, "Start should match request");
    assert!(
        end.height < height_beyond_tip,
        "End height should be clamped below the requested value"
    );

    // 5) Non-existent address -> empty deltas.
    let response = svc
        .state_subscriber
        .get_address_deltas(GetAddressDeltasParams::Filtered {
            addresses: vec![NON_EXISTENT_ADDRESS.to_string()],
            start: 1,
            end: height_beyond_tip,
            chain_info: true,
        })
        .await
        .unwrap();
    let GetAddressDeltasResponse::WithChainInfo { deltas, .. } = response else {
        panic!("Expected WithChainInfo variant");
    };
    assert!(deltas.is_empty(), "Non-existent address should have no deltas");

    svc.test_manager.close().await;
}

/// Port of `monitor_unverified_mempool` (wallet_to_validator): broadcast two
/// unmined sends, observe them in the validator mempool, then mine them in.
///
/// `#[ignore]`d, with the balance assertions commented out: the original asserts
/// `WalletBalance::{unconfirmed,confirmed}_*_balance`, fields devtool's
/// `WalletBalance` does not have (it surfaces only `*_spendable`, and devtool
/// sync is block-based so it never scans the mempool). Restore the assertions
/// (using `Pool::spendable_balance` for the confirmed ones) and un-ignore when
/// devtool surfaces unconfirmed balances.
async fn monitor_unverified_mempool<Service>()
where
    Service: TestService,
    IndexerError: From<<<Service as ZcashService>::Subscriber as ZcashIndexer>::Error>,
    <Service as ZcashService>::Subscriber: PollableTip,
{
    let (mut test_manager, mut clients) = launch_and_fund_faucet::<Service>(2).await;

    let recipient_ua = clients.get_recipient_address("unified").await;
    let txid_1 = clients.send_from_faucet(&recipient_ua, 250_000).await;
    let recipient_zaddr = clients.get_recipient_address("sapling").await;
    let txid_2 = clients.send_from_faucet(&recipient_zaddr, 250_000).await;

    clients.rescan_recipient().await;

    let fetch_service = test_manager.full_node_jsonrpc_connector().await;
    let mempool_txids = fetch_service.get_raw_mempool().await.unwrap();
    dbg!(txid_1);
    dbg!(txid_2);
    dbg!(mempool_txids.clone());

    let _transaction_1 = dbg!(
        fetch_service
            .get_raw_transaction(mempool_txids.transactions[0].clone(), Some(1))
            .await
    );
    let _transaction_2 = dbg!(
        fetch_service
            .get_raw_transaction(mempool_txids.transactions[1].clone(), Some(1))
            .await
    );

    // Unconfirmed (mempool) balances — devtool's WalletBalance has no
    // unconfirmed_* fields (block-based sync, no mempool scan):
    // assert_eq!(
    //     clients.recipient_balance().await.unconfirmed_orchard_balance.unwrap().into_u64(),
    //     250_000
    // );
    // assert_eq!(
    //     clients.recipient_balance().await.unconfirmed_sapling_balance.unwrap().into_u64(),
    //     250_000
    // );

    test_manager
        .generate_blocks_and_wait_for_tip(1, test_manager.subscriber())
        .await;

    let _transaction_1 = dbg!(
        fetch_service
            .get_raw_transaction(mempool_txids.transactions[0].clone(), Some(1))
            .await
    );
    let _transaction_2 = dbg!(
        fetch_service
            .get_raw_transaction(mempool_txids.transactions[1].clone(), Some(1))
            .await
    );

    clients.sync_recipient().await;

    // Confirmed balances — original asserts WalletBalance::confirmed_orchard_balance,
    // also absent on devtool. Restore as e.g.:
    // assert_eq!(
    //     wallet_tests::Pool::Orchard.spendable_balance(&clients.recipient_balance().await),
    //     250_000
    // );

    test_manager.close().await;
}

/// Port of `test_get_mempool_info` (fetch_service, zebrad): `get_mempool_info`
/// matches values recomputed from the fetch subscriber's mempool internals.
/// FetchService-only — the recompute reads `FetchServiceSubscriber.indexer`.
#[allow(deprecated)] // FetchService is a deprecated re-export.
async fn get_mempool_info_fetch() {
    use hex::ToHex as _;
    use zaino_state::ChainIndex as _;

    let (mut test_manager, _transparent_txid, _unified_txid) =
        fund_and_fill_mempool::<zaino_state::FetchService>().await;

    let subscriber = test_manager.subscriber();
    let info = subscriber.get_mempool_info().await.unwrap();
    let keys = subscriber.indexer.get_mempool_txids().await.unwrap();
    let values = subscriber
        .indexer
        .get_mempool_transactions(Vec::new())
        .await
        .unwrap();

    assert_eq!(info.size, values.len() as u64);
    assert!(info.size >= 1);

    let expected_bytes: u64 = values.iter().map(|entry| entry.len() as u64).sum();
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

/// Port of `state_service_…::get_mempool_info` (zebrad): `get_mempool_info`
/// matches values recomputed from the state subscriber's mempool internals.
/// StateService-only — the recompute reads `StateServiceSubscriber.mempool`.
async fn get_mempool_info_state() {
    let mut svc = fund_and_fill_mempool_dual().await;

    let info = svc.state_subscriber.get_mempool_info().await.unwrap();
    let entries = svc.state_subscriber.mempool.get_mempool().await;

    assert_eq!(entries.len() as u64, info.size);
    assert!(info.size >= 1);

    let expected_bytes: u64 = entries
        .iter()
        .map(|(_, v)| v.serialized_tx.as_ref().as_ref().len() as u64)
        .sum();
    let expected_key_heap_bytes: u64 = entries.iter().map(|(k, _)| k.txid.capacity() as u64).sum();
    let expected_usage = expected_bytes.saturating_add(expected_key_heap_bytes);

    assert!(info.bytes > 0);
    assert_eq!(info.bytes, expected_bytes);
    assert!(info.usage >= info.bytes);
    assert_eq!(info.usage, expected_usage);

    svc.test_manager.close().await;
}

mod zebrad {
    // FetchService is a deprecated re-export; the deprecation fires at the
    // turbofish use sites below, so the allow covers the whole module.
    #[allow(deprecated)]
    mod fetch_service {
        use zaino_state::FetchService;

        #[tokio::test(flavor = "multi_thread")]
        async fn receives_mining_reward() {
            crate::receives_mining_reward::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn connect_to_node_get_info() {
            crate::connect_to_node_get_info::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_orchard() {
            crate::send_to_pool::<FetchService>(wallet_tests::Pool::Orchard).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_sapling() {
            crate::send_to_pool::<FetchService>(wallet_tests::Pool::Sapling).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_transparent() {
            crate::send_to_pool::<FetchService>(wallet_tests::Pool::Transparent).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_all() {
            crate::send_to_all::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn shield_for_validator() {
            crate::shield_for_validator::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(not(feature = "devtool-incompatible"), ignore = "heavy: 99-block orchard advance (~99 halo2 proofs); un-ignore + transparent filler when round-3 P2 lands")]
        async fn send_to_transparent_finalization() {
            crate::send_to_transparent_finalization::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction_mined() {
            crate::get_transaction_mined::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_mempool() {
            crate::get_raw_mempool::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_tx() {
            crate::get_mempool_tx::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_stream() {
            crate::get_mempool_stream::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_info() {
            crate::get_mempool_info_fetch().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(not(feature = "devtool-incompatible"), ignore = "devtool WalletBalance has no unconfirmed_*/confirmed_* fields; balance asserts are commented out — restore + un-ignore when devtool surfaces unconfirmed balances")]
        async fn monitor_unverified_mempool() {
            crate::monitor_unverified_mempool::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_tx_ids() {
            crate::get_address_tx_ids::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos() {
            crate::get_address_utxos::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_treestate() {
            crate::z_get_treestate::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_subtrees_by_index() {
            crate::z_get_subtrees_by_index::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_transaction() {
            crate::get_raw_transaction::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_txids() {
            crate::get_taddress_txids::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_utxos() {
            crate::get_taddress_utxos::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_utxos_stream() {
            crate::get_taddress_utxos_stream::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction_mempool() {
            crate::get_transaction_mempool::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_balance() {
            crate::get_address_balance::<FetchService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_balance() {
            crate::get_taddress_balance::<FetchService>().await;
        }
    }

    // Span both the fetch and state indexers (compares their answers), so they
    // are not per-backend tests.
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_default_pools() {
        crate::block_range_returns_default_pools().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_all_pools() {
        crate::block_range_returns_all_pools().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate_fetch_vs_state() {
        crate::z_get_treestate_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index_fetch_vs_state() {
        crate::z_get_subtrees_by_index_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction_fetch_vs_state() {
        crate::get_raw_transaction_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids_fetch_vs_state() {
        crate::get_address_tx_ids_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_fetch_vs_state() {
        crate::get_address_utxos_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool_fetch_vs_state() {
        crate::get_raw_mempool_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_transactions_regtest() {
        crate::get_address_transactions_regtest().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_data_in_compact_block() {
        crate::transparent_data_in_compact_block().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids_faucet_fetch_vs_state() {
        crate::get_taddress_txids_faucet_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance_faucet_fetch_vs_state() {
        crate::get_taddress_balance_faucet_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_faucet_fetch_vs_state() {
        crate::get_address_utxos_faucet_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_stream_faucet_fetch_vs_state() {
        crate::get_address_utxos_stream_faucet_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(not(feature = "devtool-incompatible"), ignore = "devtool `shield` drains all transparent funds, so it always includes the freshly-mined tip-99 coinbase; devtool computes coinbase maturity against the target height (tip+1) while zebra's mempool enforces it against the current tip, so that coinbase is immature by one block and the shield is rejected — un-ignore when devtool fixes the coinbase-maturity off-by-one")]
    async fn address_deltas() {
        crate::address_deltas().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance_fetch_vs_state() {
        crate::get_address_balance_fetch_vs_state().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_upper_bound() {
        crate::get_block_range_out_of_range_upper_bound().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_lower_bound() {
        crate::get_block_range_out_of_range_lower_bound().await;
    }

    mod state_service {
        #[allow(deprecated)]
        use zaino_state::StateService;

        #[tokio::test(flavor = "multi_thread")]
        async fn receives_mining_reward() {
            crate::receives_mining_reward::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn connect_to_node_get_info() {
            crate::connect_to_node_get_info::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_orchard() {
            crate::send_to_pool::<StateService>(wallet_tests::Pool::Orchard).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_sapling() {
            crate::send_to_pool::<StateService>(wallet_tests::Pool::Sapling).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_transparent() {
            crate::send_to_pool::<StateService>(wallet_tests::Pool::Transparent).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_all() {
            crate::send_to_all::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn shield_for_validator() {
            crate::shield_for_validator::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(not(feature = "devtool-incompatible"), ignore = "heavy: 99-block orchard advance (~99 halo2 proofs); un-ignore + transparent filler when round-3 P2 lands")]
        async fn send_to_transparent_finalization() {
            crate::send_to_transparent_finalization::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction_mined() {
            crate::get_transaction_mined::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_mempool() {
            crate::get_raw_mempool::<StateService>().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_info() {
            crate::get_mempool_info_state().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(not(feature = "devtool-incompatible"), ignore = "devtool WalletBalance has no unconfirmed_*/confirmed_* fields; balance asserts are commented out — restore + un-ignore when devtool surfaces unconfirmed balances")]
        async fn monitor_unverified_mempool() {
            crate::monitor_unverified_mempool::<StateService>().await;
        }

        // No get_mempool_tx here: the state backend returns mempool-tx txids
        // in display (reversed) byte order while the fetch backend returns
        // internal order, so the cross-backend txid comparison fails on
        // state (zingolabs/zaino#1225). The original test was FetchService-
        // only; re-enable once that bug is fixed.
    }
}
