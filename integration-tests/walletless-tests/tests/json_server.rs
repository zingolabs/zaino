//! Tests that compare the output of both `zcashd` and `zainod` through `FetchService`.
//!
//! Entirely gated on `zcashd_support`: every test here launches the
//! zcashd-backed dual fetch services. See
//! docs/adr/0001-zcashd-support-feature-gate.md.
#![cfg(feature = "zcashd_support")]

#[allow(deprecated)]
use zaino_state::{FetchService, FetchServiceSubscriber, ZcashIndexer};
use zaino_testutils::TestManager;
use zcash_local_net::validator::zcashd::Zcashd;

/// Assert that `query` returns the same value from the zcashd-backed and
/// zaino-backed subscribers. `query` takes the subscriber by value (subscribers
/// are `Clone`) so its future owns it — sidestepping the borrow-across-await
/// problem. The pure-compare building block of the json_server tests.
#[allow(deprecated)]
async fn assert_subscribers_agree<Q, Fut, T>(
    zcashd_subscriber: &FetchServiceSubscriber,
    zaino_subscriber: &FetchServiceSubscriber,
    query: Q,
) where
    Q: Fn(FetchServiceSubscriber) -> Fut,
    Fut: std::future::Future<Output = T>,
    T: std::fmt::Debug + PartialEq,
{
    let from_zcashd = query(zcashd_subscriber.clone()).await;
    let from_zaino = query(zaino_subscriber.clone()).await;
    assert_eq!(from_zcashd, from_zaino);
}

/// Mine `blocks` blocks one at a time, asserting `query` agrees across both
/// subscribers before each. The looped form of [`assert_subscribers_agree`] for
/// tests that check an invariant holds across the chain.
#[allow(deprecated)]
async fn compare_over_blocks<Q, Fut, T>(
    test_manager: &TestManager<Zcashd, FetchService>,
    zcashd_subscriber: &FetchServiceSubscriber,
    zaino_subscriber: &FetchServiceSubscriber,
    blocks: u32,
    query: Q,
) where
    Q: Fn(FetchServiceSubscriber) -> Fut,
    Fut: std::future::Future<Output = T>,
    T: std::fmt::Debug + PartialEq,
{
    for _ in 0..blocks {
        assert_subscribers_agree(zcashd_subscriber, zaino_subscriber, &query).await;
        test_manager
            .generate_blocks_and_wait_for_tips(1, zaino_subscriber, zcashd_subscriber)
            .await;
    }
}

#[allow(deprecated)]
async fn launch_json_server_check_info() {
    let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;
    let zcashd_info = dbg!(services.zcashd_subscriber.get_info().await.unwrap());
    let zcashd_blockchain_info = dbg!(services
        .zcashd_subscriber
        .get_blockchain_info()
        .await
        .unwrap());
    let zaino_info = dbg!(services.zaino_subscriber.get_info().await.unwrap());
    let zaino_blockchain_info = dbg!(services
        .zaino_subscriber
        .get_blockchain_info()
        .await
        .unwrap());

    // Clean timestamp from get_info
    let cleaned_zcashd_info = zaino_testutils::get_info_with_zeroed_timestamp(zcashd_info);

    let cleaned_zaino_info = zaino_testutils::get_info_with_zeroed_timestamp(zaino_info);

    assert_eq!(cleaned_zcashd_info, cleaned_zaino_info);

    assert_eq!(
        zcashd_blockchain_info.chain(),
        zaino_blockchain_info.chain()
    );
    assert_eq!(
        zcashd_blockchain_info.blocks(),
        zaino_blockchain_info.blocks()
    );
    assert_eq!(
        zcashd_blockchain_info.best_block_hash(),
        zaino_blockchain_info.best_block_hash()
    );
    assert_eq!(
        zcashd_blockchain_info.estimated_height(),
        zaino_blockchain_info.estimated_height()
    );
    assert_eq!(
        zcashd_blockchain_info.value_pools(),
        zaino_blockchain_info.value_pools()
    );
    assert_eq!(
        zcashd_blockchain_info.upgrades(),
        zaino_blockchain_info.upgrades()
    );
    assert_eq!(
        zcashd_blockchain_info.consensus(),
        zaino_blockchain_info.consensus()
    );

    services.test_manager.close().await;
}

async fn get_best_blockhash_inner() {
    let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

    assert_subscribers_agree(
        &services.zcashd_subscriber,
        &services.zaino_subscriber,
        |s| async move { dbg!(s.get_best_blockhash().await.unwrap()) },
    )
    .await;

    services.test_manager.close().await;
}

async fn get_block_count_inner() {
    let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

    assert_subscribers_agree(
        &services.zcashd_subscriber,
        &services.zaino_subscriber,
        |s| async move { dbg!(s.get_block_count().await.unwrap()) },
    )
    .await;

    services.test_manager.close().await;
}

async fn validate_address_inner() {
    let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

    // Using a testnet transparent address
    let address_string = "tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb";

    let address_with_script = "t3TAfQ9eYmXWGe3oPae1XKhdTxm8JvsnFRL";

    let zcashd_valid = services
        .zcashd_subscriber
        .validate_address(address_string.to_string())
        .await
        .unwrap();

    let zaino_valid = services
        .zaino_subscriber
        .validate_address(address_string.to_string())
        .await
        .unwrap();

    assert_eq!(zcashd_valid, zaino_valid, "Address should be valid");

    let zcashd_valid_script = services
        .zcashd_subscriber
        .validate_address(address_with_script.to_string())
        .await
        .unwrap();

    let zaino_valid_script = services
        .zaino_subscriber
        .validate_address(address_with_script.to_string())
        .await
        .unwrap();

    assert_eq!(
        zcashd_valid_script, zaino_valid_script,
        "Address should be valid"
    );

    services.test_manager.close().await;
}

async fn z_get_block_inner() {
    let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

    let zcashd_block_raw = dbg!(services
        .zcashd_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    let zaino_block_raw = dbg!(services
        .zaino_subscriber
        .z_get_block("1".to_string(), Some(0))
        .await
        .unwrap());

    assert_eq!(zcashd_block_raw, zaino_block_raw);

    let zcashd_block = dbg!(services
        .zcashd_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    let zaino_block = dbg!(services
        .zaino_subscriber
        .z_get_block("1".to_string(), Some(1))
        .await
        .unwrap());

    assert_eq!(zcashd_block, zaino_block);

    let hash = match zcashd_block {
        zebra_rpc::methods::GetBlock::Raw(_) => panic!("expected object"),
        zebra_rpc::methods::GetBlock::Object(obj) => obj.hash().to_string(),
    };
    let zaino_get_block_by_hash = services
        .zaino_subscriber
        .z_get_block(hash.clone(), Some(1))
        .await
        .unwrap();
    assert_eq!(zaino_get_block_by_hash, zaino_block);

    services.test_manager.close().await;
}

async fn get_tx_out_set_info_inner() {
    let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

    services.generate_blocks_and_wait_for_tips(1).await;

    let zcashd_txoutset_info = services
        .zcashd_subscriber
        .get_tx_out_set_info()
        .await
        .unwrap();
    let zaino_txoutset_info = services
        .zaino_subscriber
        .get_tx_out_set_info()
        .await
        .unwrap();

    // Structural parity with zcashd: height, bestblock, transactions, txouts and total_amount
    // must match. `bytes_serialized` and `hash_serialized` are Zaino-defined and intentionally
    // diverge from zcashd; only Zaino-internal invariants are asserted on those fields.
    use zaino_fetch::jsonrpsee::response::GetTxOutSetInfoResponse;
    let (zaino, zcashd) = match (zaino_txoutset_info, zcashd_txoutset_info) {
        (GetTxOutSetInfoResponse::Info(z), GetTxOutSetInfoResponse::Info(r)) => (z, r),
        other => panic!("expected non-empty gettxoutsetinfo from both sides, got {other:?}"),
    };

    assert_eq!(zaino.height, zcashd.height, "`height` differs from zcashd");
    assert_eq!(
        zaino.best_block, zcashd.best_block,
        "`bestblock` differs from zcashd"
    );
    assert_eq!(
        zaino.transactions, zcashd.transactions,
        "`transactions` count differs from zcashd"
    );
    assert_eq!(zaino.txouts, zcashd.txouts, "`txouts` differs from zcashd");
    assert!(
        (zaino.total_amount - zcashd.total_amount).abs() < 1e-8,
        "`total_amount` differs from zcashd: zaino={} zcashd={}",
        zaino.total_amount,
        zcashd.total_amount
    );

    assert_eq!(
        zaino.bytes_serialized,
        zaino.txouts * 65,
        "`bytes_serialized` must equal txouts * 65 under Zaino's UTXO entry encoding"
    );
    assert_eq!(
        zaino.hash_serialized.len(),
        64,
        "`hash_serialized` must be 64 lowercase hex chars"
    );
    assert!(
        zaino.hash_serialized.chars().all(|c| c.is_ascii_hexdigit()),
        "`hash_serialized` must be hex: got {}",
        zaino.hash_serialized
    );

    services.test_manager.close().await;
}

// TODO: This module should not be called `zcashd`
mod zcashd {
    use super::*;

    pub(crate) mod zcash_indexer {
        use zaino_state::LightWalletIndexer;
        use zebra_rpc::methods::GetBlock;

        use super::*;

        #[tokio::test(flavor = "multi_thread")]
        async fn check_info_no_cookie() {
            launch_json_server_check_info().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn check_info_with_cookie() {
            launch_json_server_check_info().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_best_blockhash() {
            get_best_blockhash_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_count() {
            get_block_count_inner().await;
        }

        /// Checks that the difficulty is the same between zcashd and zaino.
        ///
        /// This tests generates blocks and checks that the difficulty is the same between zcashd and zaino
        /// after each block is generated.
        #[tokio::test(flavor = "multi_thread")]
        async fn get_difficulty() {
            let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            compare_over_blocks(
                &services.test_manager,
                &services.zcashd_subscriber,
                &services.zaino_subscriber,
                10,
                |s| async move { s.get_difficulty().await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_deltas() {
            let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            const BLOCK_LIMIT: i32 = 10;

            for _ in 0..BLOCK_LIMIT {
                let current_block = services.zcashd_subscriber.get_latest_block().await.unwrap();

                let block_hash_bytes: [u8; 32] = current_block.hash.as_slice().try_into().unwrap();

                let block_hash = zebra_chain::block::Hash::from(block_hash_bytes);

                let zcashd_deltas = services
                    .zcashd_subscriber
                    .get_block_deltas(block_hash.to_string())
                    .await
                    .unwrap();
                let zaino_deltas = services
                    .zaino_subscriber
                    .get_block_deltas(block_hash.to_string())
                    .await
                    .unwrap();

                assert_eq!(zcashd_deltas, zaino_deltas);

                services.generate_blocks_and_wait_for_tips(1).await;
            }

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_mining_info() {
            let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            compare_over_blocks(
                &services.test_manager,
                &services.zcashd_subscriber,
                &services.zaino_subscriber,
                10,
                |s| async move { s.get_mining_info().await.unwrap() },
            )
            .await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_tx_out_set_info() {
            get_tx_out_set_info_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_peer_info() {
            let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            let zcashd_peer_info = services.zcashd_subscriber.get_peer_info().await.unwrap();
            let zaino_peer_info = services.zaino_subscriber.get_peer_info().await.unwrap();

            assert_eq!(zcashd_peer_info, zaino_peer_info);

            services.generate_blocks_and_wait_for_tips(1).await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_subsidy() {
            let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            services.generate_blocks_and_wait_for_tips(1).await;

            let zcashd_block_subsidy = services
                .zcashd_subscriber
                .get_block_subsidy(1)
                .await
                .unwrap();
            let zaino_block_subsidy = services
                .zaino_subscriber
                .get_block_subsidy(1)
                .await
                .unwrap();

            assert_eq!(zcashd_block_subsidy, zaino_block_subsidy);

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn validate_address() {
            validate_address_inner().await;
        }

        #[allow(deprecated)]
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn z_validate_address() {
            let mut services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            walletless_tests::rpc::z_validate_address::run_z_validate_for(
                &services.zcashd_subscriber,
                walletless_tests::rpc::z_validate_address::SaplingSuite::Standard,
            )
            .await;

            services.test_manager.close().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_block() {
            z_get_block_inner().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn get_block_header() {
            let services = zaino_testutils::launch_zcashd_dual_fetch_services().await;

            const BLOCK_LIMIT: u32 = 10;

            services
                .test_manager
                .generate_blocks_and_check_each(
                    BLOCK_LIMIT,
                    &services.zaino_subscriber,
                    &services.zcashd_subscriber,
                    async |i| {
                        let block = services
                            .zcashd_subscriber
                            .z_get_block(i.to_string(), Some(1))
                            .await
                            .unwrap();

                        let block_hash = match block {
                            GetBlock::Object(block) => block.hash(),
                            GetBlock::Raw(_) => panic!("Expected block object"),
                        };

                        let zcashd_get_block_header = services
                            .zcashd_subscriber
                            .get_block_header(block_hash.to_string(), false)
                            .await
                            .unwrap();

                        let zainod_block_header_response = services
                            .zaino_subscriber
                            .get_block_header(block_hash.to_string(), false)
                            .await
                            .unwrap();
                        assert_eq!(zcashd_get_block_header, zainod_block_header_response);
                    },
                )
                .await;
        }
    }
}
