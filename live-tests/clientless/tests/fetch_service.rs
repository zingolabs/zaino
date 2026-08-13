//! These tests compare the output of `FetchService` with the output of `JsonRpcConnector`.

use clientless::rpc::z_validate_address::run_z_validate_for;
use futures::StreamExt as _;
use zaino_address::ValidatedAddress;
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::service::{BlockId, BlockRange, GetSubtreeRootsArg};
use zaino_serve::rpc::jsonrpc::wire::{
    block_header::GetBlockHeader, block_subsidy::GetBlockSubsidy, mining_info::GetMiningInfoWire,
    peer_info::GetPeerInfo,
};
use zaino_state::{LightWalletIndexer, NodeBackedIndexerServiceSubscriber, ZcashIndexer};
use zaino_status::{Status, StatusType};
use zaino_testutils::ValidatorOracle;
use zaino_testutils::{Rpc, TestManager, ValidatorExt, ValidatorKind};
use zebra_chain::parameters::subsidy::ParameterSubsidy as _;
use zebra_rpc::methods::{GetBlock, GetBlockHash};

async fn launch_fetch_service<V: ValidatorExt>(
    validator: &ValidatorKind,
    chain_cache: Option<std::path::PathBuf>,
) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, chain_cache).await;
    assert_eq!(fetch_service_subscriber.status(), StatusType::Ready);
    dbg!(fetch_service_subscriber.data.clone());
    dbg!(fetch_service_subscriber.get_info().await.unwrap());
    dbg!(
        fetch_service_subscriber
            .get_blockchain_info()
            .await
            .unwrap()
            .blocks
    );

    test_manager.close().await;
}

/// Fetch block 1 at the given `getblock` verbosity (0 = hex encoded data,
/// 1 = JSON object).
#[allow(deprecated)]
async fn fetch_service_get_block_at_verbosity<V: ValidatorExt>(
    validator: &ValidatorKind,
    verbosity: u8,
) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    dbg!(fetch_service_subscriber
        .z_get_block("1".to_string(), Some(verbosity))
        .await
        .unwrap());

    test_manager.close().await;
}

async fn fetch_service_get_block_raw<V: ValidatorExt>(validator: &ValidatorKind) {
    fetch_service_get_block_at_verbosity::<V>(validator, 0).await;
}

async fn fetch_service_get_block_object<V: ValidatorExt>(validator: &ValidatorKind) {
    fetch_service_get_block_at_verbosity::<V>(validator, 1).await;
}

#[allow(deprecated)]
async fn fetch_service_get_latest_block<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    test_manager
        .generate_blocks_and_wait_for_tip(1, &fetch_service_subscriber)
        .await;

    let json_service = test_manager.full_node_jsonrpc_connector().await;

    let fetch_service_get_latest_block =
        dbg!(fetch_service_subscriber.get_latest_block().await.unwrap());

    let json_service_blockchain_info = json_service.get("getblockchaininfo").await;

    // The validator writes its tip hash in display order; `BlockId.hash` is
    // internal order, so it is reversed on the way in.
    let mut tip_hash = <[u8; 32]>::try_from(
        hex::decode(
            json_service_blockchain_info["bestblockhash"]
                .as_str()
                .expect("a best block hash"),
        )
        .expect("a hex best block hash")
        .as_slice(),
    )
    .expect("a 32-byte best block hash");
    tip_hash.reverse();

    let json_service_get_latest_block = dbg!(BlockId {
        height: json_service_blockchain_info["blocks"]
            .as_u64()
            .expect("a chain height"),
        hash: tip_hash.to_vec(),
    });

    assert_eq!(fetch_service_get_latest_block.height, 3);
    assert_eq!(
        fetch_service_get_latest_block,
        json_service_get_latest_block
    );

    test_manager.close().await;
}

/// Launch a fetch-backend manager, run `fetch_query` against the Zaino
/// `FetchService` subscriber and `rpc_query` against the validator's JSON-RPC,
/// then assert the two results are equal. Collapses the
/// `assert_fetch_service_*_matches_rpc` helpers that differ only by the query.
#[allow(deprecated)]
async fn assert_subscriber_matches_rpc<V, T, FFut, RFut>(
    validator: &ValidatorKind,
    fetch_query: impl FnOnce(NodeBackedIndexerServiceSubscriber) -> FFut,
    rpc_query: impl FnOnce(ValidatorOracle) -> RFut,
) where
    V: ValidatorExt,
    T: std::fmt::Debug + PartialEq,
    FFut: std::future::Future<Output = T>,
    RFut: std::future::Future<Output = T>,
{
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;
    let from_fetch = fetch_query(fetch_service_subscriber).await;
    let jsonrpc_client = test_manager.full_node_jsonrpc_connector().await;
    let from_rpc = rpc_query(jsonrpc_client).await;
    assert_eq!(from_fetch, from_rpc);
}

#[allow(deprecated)]
async fn assert_fetch_service_difficulty_matches_rpc<V: ValidatorExt>(validator: &ValidatorKind) {
    assert_subscriber_matches_rpc::<V, _, _, _>(
        validator,
        |sub| async move { sub.get_difficulty().await.unwrap() },
        |client| async move {
            client
                .get("getdifficulty")
                .await
                .as_f64()
                .expect("a difficulty")
        },
    )
    .await;
}

/// Every `getmininginfo` field the validator reports must match what Zaino
/// serves.
///
/// Compared key by key over the validator's own response rather than as whole
/// objects. Zaino additionally emits the zcashd-only local-miner fields
/// (`genproclimit`, `localsolps`, `generate`, `pooledtx`, `errorstimestamp`) as
/// explicit JSON `null`, where zebrad omits the keys — long-standing behaviour
/// of this response type, and a client asking a zcashd-shaped interface gets a
/// zcashd-shaped answer. What must not differ is any value the validator
/// actually sent.
#[allow(deprecated)]
async fn assert_fetch_service_mininginfo_matches_rpc<V: ValidatorExt>(validator: &ValidatorKind) {
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let served = serde_json::to_value(GetMiningInfoWire::from_domain(
        fetch_service_subscriber.get_mining_info().await.unwrap(),
    ))
    .unwrap();

    let reported = test_manager
        .full_node_jsonrpc_connector()
        .await
        .get("getmininginfo")
        .await;

    for (field, expected) in reported
        .as_object()
        .expect("getmininginfo returns an object")
    {
        assert_eq!(
            &served[field], expected,
            "`{field}` differs from the validator's own getmininginfo"
        );
    }
}

#[cfg(feature = "zcashd_support")]
#[allow(deprecated)]
async fn assert_fetch_service_gettxoutsetinfo_matches_rpc<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let fetch_service_txoutset_info = fetch_service_subscriber
        .get_tx_out_set_info()
        .await
        .unwrap();

    let jsonrpc_client = test_manager.full_node_jsonrpc_connector().await;

    let rpc_txoutset_info = jsonrpc_client.get("gettxoutsetinfo").await;

    // Structural parity with zcashd: height, bestblock, transactions, txouts and total_amount
    // must match. `bytes_serialized` and `hash_serialized` are Zaino-defined (see the
    // `gettxoutsetinfo` spec in zaino-state) and intentionally diverge from zcashd; only
    // Zaino-internal invariants are asserted on those fields.
    let zaino = serde_json::to_value(fetch_service_txoutset_info).unwrap();
    let zcashd = rpc_txoutset_info;
    assert!(
        zaino.get("height").is_some() && zcashd.get("height").is_some(),
        "expected non-empty gettxoutsetinfo from both sides, got {zaino:?} / {zcashd:?}"
    );

    for field in ["height", "bestblock", "transactions", "txouts"] {
        assert_eq!(zaino[field], zcashd[field], "`{field}` differs from zcashd");
    }
    let amount = |value: &serde_json::Value| {
        value["total_amount"]
            .as_f64()
            .expect("gettxoutsetinfo reports total_amount as a number")
    };
    assert!(
        (amount(&zaino) - amount(&zcashd)).abs() < 1e-8,
        "`total_amount` differs from zcashd: zaino={} zcashd={}",
        amount(&zaino),
        amount(&zcashd)
    );

    // Zaino-only invariants on the redefined fields.
    assert_eq!(
        zaino["bytes_serialized"].as_u64().expect("a byte count"),
        zaino["txouts"].as_u64().expect("a txout count") * 65,
        "`bytes_serialized` must equal `txouts * 65` under Zaino's UTXO entry encoding"
    );
    let hash_serialized = zaino["hash_serialized"]
        .as_str()
        .expect("`hash_serialized` is a string");
    assert_eq!(
        hash_serialized.len(),
        64,
        "`hash_serialized` must be 64 lowercase hex chars"
    );
    assert!(
        hash_serialized.chars().all(|c| c.is_ascii_hexdigit()),
        "`hash_serialized` must be hex: got {hash_serialized}"
    );
}

#[allow(deprecated)]
async fn assert_fetch_service_peerinfo_matches_rpc<V: ValidatorExt>(validator: &ValidatorKind) {
    assert_subscriber_matches_rpc::<V, _, _, _>(
        validator,
        |sub| async move {
            serde_json::to_value(GetPeerInfo::from_domain(sub.get_peer_info().await.unwrap()))
                .unwrap()
        },
        |client| async move { client.get("getpeerinfo").await },
    )
    .await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_subsidy<V: ValidatorExt>(validator: &ValidatorKind) {
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    // getblocksubsidy is a pure function of (height, network schedule): the
    // validator never reads the chain for an explicit height, so no mining is
    // needed — heights far past the tip answer identically. Sweep through the
    // first halving boundary plus a margin on both validators.
    let height_limit = fetch_service_subscriber
        .network()
        .height_for_first_halving()
        .0
        + 10;

    let jsonrpc_client = test_manager.full_node_jsonrpc_connector().await;

    for height in 0..height_limit {
        let fetch_service_get_block_subsidy = fetch_service_subscriber
            .get_block_subsidy(height)
            .await
            .unwrap();

        let rpc_block_subsidy_response = jsonrpc_client
            .call("getblocksubsidy", vec![serde_json::json!(height)])
            .await;
        assert_eq!(
            serde_json::to_value(GetBlockSubsidy::from_domain(
                fetch_service_get_block_subsidy
            ))
            .unwrap(),
            rpc_block_subsidy_response
        );
    }
}

#[allow(deprecated)]
async fn fetch_service_get_block<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let block_id = BlockId {
        height: 1,
        hash: Vec::new(),
    };

    let fetch_service_get_block = dbg!(fetch_service_subscriber
        .get_block(block_id.clone())
        .await
        .unwrap());

    assert_eq!(fetch_service_get_block.height, block_id.height);
    let block_id_by_hash = BlockId {
        height: 0,
        hash: fetch_service_get_block.hash.clone(),
    };
    let fetch_service_get_block_by_hash = fetch_service_subscriber
        .get_block(block_id_by_hash.clone())
        .await
        .unwrap();
    assert_eq!(fetch_service_get_block_by_hash.hash, block_id_by_hash.hash);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_header<V: ValidatorExt>(validator: &ValidatorKind) {
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    const BLOCK_LIMIT: u32 = 10;

    let jsonrpc_client = test_manager.full_node_jsonrpc_connector().await;

    test_manager
        .generate_blocks_and_check_each(
            BLOCK_LIMIT,
            &fetch_service_subscriber,
            &fetch_service_subscriber,
            async |i| {
                let block = fetch_service_subscriber
                    .z_get_block(i.to_string(), Some(1))
                    .await
                    .unwrap();

                let block_hash = match block {
                    GetBlock::Object(block) => block.hash(),
                    GetBlock::Raw(_) => panic!("Expected block object"),
                };

                let fetch_service_get_block_header = GetBlockHeader::compact_from_domain(
                    fetch_service_subscriber
                        .get_raw_block_header(block_hash.to_string())
                        .await
                        .unwrap(),
                );

                let rpc_block_header_response = jsonrpc_client
                    .call(
                        "getblockheader",
                        vec![
                            serde_json::json!(block_hash.to_string()),
                            serde_json::json!(false),
                        ],
                    )
                    .await;

                let fetch_service_get_block_header_verbose = GetBlockHeader::verbose_from_domain(
                    fetch_service_subscriber
                        .get_block_header(block_hash.to_string())
                        .await
                        .unwrap(),
                );

                let rpc_block_header_response_verbose = jsonrpc_client
                    .call(
                        "getblockheader",
                        vec![
                            serde_json::json!(block_hash.to_string()),
                            serde_json::json!(true),
                        ],
                    )
                    .await;

                assert_eq!(
                    serde_json::to_value(fetch_service_get_block_header).unwrap(),
                    rpc_block_header_response
                );
                assert_eq!(
                    serde_json::to_value(fetch_service_get_block_header_verbose).unwrap(),
                    rpc_block_header_response_verbose
                );
            },
        )
        .await;
}

/// Launch a fetch-backend manager and mine 5 blocks, leaving the chain tip at
/// height 7 (a fresh regtest launch starts at height 2).
#[allow(deprecated)]
async fn launch_and_mine_five_blocks<V: ValidatorExt>(
    validator: &ValidatorKind,
) -> (TestManager<V, Rpc>, NodeBackedIndexerServiceSubscriber) {
    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    test_manager
        .generate_blocks_and_wait_for_tip(5, &fetch_service_subscriber)
        .await;

    (test_manager, fetch_service_subscriber)
}

#[allow(deprecated)]
async fn fetch_service_get_best_blockhash<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        launch_and_mine_five_blocks::<V>(validator).await;

    let inspected_block: GetBlock = fetch_service_subscriber
        // Some(verbosity) : 1 for JSON Object, 2 for tx data as JSON instead of hex
        .z_get_block("7".to_string(), Some(1))
        .await
        .unwrap();

    let ret = match inspected_block {
        GetBlock::Object(obj) => Some(obj.hash()),
        _ => None,
    };

    let fetch_service_get_best_blockhash: GetBlockHash =
        dbg!(fetch_service_subscriber.get_best_blockhash().await.unwrap());

    assert_eq!(
        fetch_service_get_best_blockhash.hash(),
        ret.expect("ret to be Some(GetBlockHash) not None")
    );

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_count<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        launch_and_mine_five_blocks::<V>(validator).await;

    let block_id = BlockId {
        height: 7,
        hash: Vec::new(),
    };

    let fetch_service_get_block_count =
        dbg!(fetch_service_subscriber.get_block_count().await.unwrap());

    assert_eq!(fetch_service_get_block_count.0 as u64, block_id.height);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_validate_address<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    // scriptpubkey: "76a914000000000000000000000000000000000000000088ac"
    let expected_validation = ValidatedAddress::Transparent {
        address: "tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_string(),
        is_script: false,
    };
    let fetch_service_validate_address = fetch_service_subscriber
        .validate_address("tm9iMLAuYMzJ6jtFLcA7rzUmfreGuKvr7Ma".to_string())
        .await
        .unwrap();

    assert_eq!(fetch_service_validate_address, expected_validation);

    // scriptpubkey: "a914000000000000000000000000000000000000000087"
    let expected_validation_script = ValidatedAddress::Transparent {
        address: "t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2".to_string(),
        is_script: true,
    };

    let fetch_service_validate_address_script = fetch_service_subscriber
        .validate_address("t26YoyZ1iPgiMEWL4zGUm74eVWfhyDMXzY2".to_string())
        .await
        .unwrap();

    assert_eq!(
        fetch_service_validate_address_script,
        expected_validation_script
    );

    test_manager.close().await;
}

/// Launch a fetch-backend manager and run the shared `z_validate_address`
/// suite against its subscriber.
async fn z_validate<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    run_z_validate_for(&fetch_service_subscriber).await;

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_nullifiers<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let block_id = BlockId {
        height: 1,
        hash: Vec::new(),
    };

    let fetch_service_get_block_nullifiers = dbg!(fetch_service_subscriber
        .get_block_nullifiers(block_id.clone())
        .await
        .unwrap());

    assert_eq!(fetch_service_get_block_nullifiers.height, block_id.height);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_block_range<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    test_manager
        .generate_blocks_and_wait_for_tip(10, &fetch_service_subscriber)
        .await;

    let fetch_blocks =
        zaino_testutils::collect_block_range(&fetch_service_subscriber, 1, 10, vec![]).await;

    dbg!(fetch_blocks);

    test_manager.close().await;
}

// TODO(#1088): replace deprecated nullifier-range client usage.
#[allow(deprecated)]
async fn fetch_service_get_block_range_nullifiers<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    test_manager
        .generate_blocks_and_wait_for_tip(10, &fetch_service_subscriber)
        .await;

    let block_range = BlockRange {
        start: Some(BlockId {
            height: 1,
            hash: Vec::new(),
        }),
        end: Some(BlockId {
            height: 10,
            hash: Vec::new(),
        }),
        pool_types: zaino_testutils::all_pools_i32(),
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_block_range_nullifiers(block_range.clone())
        .await
        .unwrap();
    let fetch_service_compact_blocks: Vec<_> = fetch_service_stream.collect().await;

    let fetch_nullifiers: Vec<CompactBlock> = fetch_service_compact_blocks
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(fetch_nullifiers);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_tree_state<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let chain_height = dbg!(fetch_service_subscriber.chain_height().await.unwrap()).0;

    let block_id = BlockId {
        height: chain_height as u64,
        hash: Vec::new(),
    };

    dbg!(fetch_service_subscriber
        .get_tree_state(block_id)
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_latest_tree_state<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    dbg!(fetch_service_subscriber
        .get_latest_tree_state()
        .await
        .unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_subtree_roots<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let subtree_roots_arg = GetSubtreeRootsArg {
        start_index: 0,
        shielded_protocol: 1,
        max_entries: 0,
    };

    let fetch_service_stream = fetch_service_subscriber
        .get_subtree_roots(subtree_roots_arg)
        .await
        .unwrap();
    let fetch_service_roots: Vec<_> = fetch_service_stream.collect().await;

    let fetch_roots: Vec<_> = fetch_service_roots
        .into_iter()
        .filter_map(|result| result.ok())
        .collect();

    dbg!(fetch_roots);

    test_manager.close().await;
}

#[allow(deprecated)]
async fn fetch_service_get_lightd_info<V: ValidatorExt>(validator: &ValidatorKind) {
    let (mut test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    dbg!(fetch_service_subscriber.get_lightd_info().await.unwrap());

    test_manager.close().await;
}

#[allow(deprecated)]
async fn assert_fetch_service_getnetworksols_matches_rpc<V: ValidatorExt>(
    validator: &ValidatorKind,
) {
    assert_subscriber_matches_rpc::<V, _, _, _>(
        validator,
        |sub| async move { sub.get_network_sol_ps(None, None).await.unwrap() },
        |client| async move {
            client
                .get("getnetworksolps")
                .await
                .as_u64()
                .expect("a solution rate")
        },
    )
    .await;
}

#[cfg(feature = "zcashd_support")]
#[allow(deprecated)]
async fn fetch_service_get_block_deltas<V: ValidatorExt>(validator: &ValidatorKind) {
    use zaino_serve::rpc::jsonrpc::wire::block_deltas::BlockDeltas;

    let (test_manager, fetch_service_subscriber) =
        zaino_testutils::launch_with_fetch_subscriber::<V>(validator, None).await;

    let current_block = fetch_service_subscriber.get_latest_block().await.unwrap();

    let block_hash_bytes: [u8; 32] = current_block.hash.as_slice().try_into().unwrap();

    let block_hash = zebra_chain::block::Hash::from(block_hash_bytes);

    // Note: we need an 'expected' block hash in order to query its deltas.
    // Having a predictable or test vector chain is the way to go here.
    let fetch_service_block_deltas = fetch_service_subscriber
        .get_block_deltas(block_hash.to_string())
        .await
        .unwrap();

    let jsonrpc_client = test_manager.full_node_jsonrpc_connector().await;

    let rpc_block_deltas = jsonrpc_client
        .get_block_deltas(block_hash.to_string())
        .await
        .unwrap();

    assert_eq!(
        serde_json::to_value(BlockDeltas::from_domain(fetch_service_block_deltas).unwrap())
            .unwrap(),
        serde_json::to_value(rpc_block_deltas).unwrap()
    );
}

#[cfg(feature = "zcashd_support")]
mod zcashd {

    use super::*;
    use zcash_local_net::validator::zcashd::Zcashd;

    mod launch {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn regtest_no_cache() {
            launch_fetch_service::<Zcashd>(&ValidatorKind::Zcashd, None).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zcashd::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() {
            launch_fetch_service::<Zcashd>(
                &ValidatorKind::Zcashd,
                zaino_testutils::ZCASHD_CHAIN_CACHE_DIR.clone(),
            )
            .await;
        }
    }

    mod validation {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn validate_address() {
            fetch_service_validate_address::<Zcashd>(&ValidatorKind::Zcashd).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        pub(crate) async fn z_validate_address() {
            z_validate::<Zcashd>(&ValidatorKind::Zcashd).await;
        }
    }

    mod get {
        use super::*;

        zaino_testutils::validator_tests!(
            Zcashd,
            ValidatorKind::Zcashd,
            block_raw => fetch_service_get_block_raw,
            block_object => fetch_service_get_block_object,
            latest_block => fetch_service_get_latest_block,
            block => fetch_service_get_block,
            block_header => fetch_service_get_block_header,
            difficulty => assert_fetch_service_difficulty_matches_rpc,
            block_deltas => fetch_service_get_block_deltas,
            mining_info => assert_fetch_service_mininginfo_matches_rpc,
            peer_info => assert_fetch_service_peerinfo_matches_rpc,
            block_subsidy => fetch_service_get_block_subsidy,
            best_blockhash => fetch_service_get_best_blockhash,
            block_count => fetch_service_get_block_count,
            block_nullifiers => fetch_service_get_block_nullifiers,
            block_range => fetch_service_get_block_range,
            block_range_nullifiers => fetch_service_get_block_range_nullifiers,
            tree_state => fetch_service_get_tree_state,
            latest_tree_state => fetch_service_get_latest_tree_state,
            subtree_roots => fetch_service_get_subtree_roots,
            lightd_info => fetch_service_get_lightd_info,
            get_network_sol_ps => assert_fetch_service_getnetworksols_matches_rpc,
            get_tx_out_set_info => assert_fetch_service_gettxoutsetinfo_matches_rpc,
        );
    }
}

mod zebrad {

    use super::*;
    use zcash_local_net::validator::zebrad::Zebrad;

    mod launch {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn regtest_no_cache() {
            launch_fetch_service::<Zebrad>(&ValidatorKind::Zebrad, None).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[ignore = "We no longer use chain caches. See zebrad::launch::regtest_no_cache."]
        pub(crate) async fn regtest_with_cache() {
            launch_fetch_service::<Zebrad>(
                &ValidatorKind::Zebrad,
                zaino_testutils::ZEBRAD_CHAIN_CACHE_DIR.clone(),
            )
            .await;
        }
    }

    mod validation {

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        pub(crate) async fn validate_address() {
            fetch_service_validate_address::<Zebrad>(&ValidatorKind::Zebrad).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        pub(crate) async fn z_validate_address() {
            z_validate::<Zebrad>(&ValidatorKind::Zebrad).await;
        }
    }

    mod get {
        use super::*;

        zaino_testutils::validator_tests!(
            Zebrad,
            ValidatorKind::Zebrad,
            block_raw => fetch_service_get_block_raw,
            block_object => fetch_service_get_block_object,
            latest_block => fetch_service_get_latest_block,
            block => fetch_service_get_block,
            block_header => fetch_service_get_block_header,
            difficulty => assert_fetch_service_difficulty_matches_rpc,
            mining_info => assert_fetch_service_mininginfo_matches_rpc,
            peer_info => assert_fetch_service_peerinfo_matches_rpc,
            block_subsidy => fetch_service_get_block_subsidy,
            best_blockhash => fetch_service_get_best_blockhash,
            block_count => fetch_service_get_block_count,
            block_nullifiers => fetch_service_get_block_nullifiers,
            block_range => fetch_service_get_block_range,
            block_range_nullifiers => fetch_service_get_block_range_nullifiers,
            tree_state => fetch_service_get_tree_state,
            latest_tree_state => fetch_service_get_latest_tree_state,
            subtree_roots => fetch_service_get_subtree_roots,
            lightd_info => fetch_service_get_lightd_info,
            get_network_sol_ps => assert_fetch_service_getnetworksols_matches_rpc,
        );
    }
}
