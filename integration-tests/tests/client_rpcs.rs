//! Tests Zainod release binary against the `zcash_local_net` client RPC test fixtures.
//!
//! Ensure the release binary is up-to-date with `cargo build --release` before running this test-suite.
//!
//! See `Testing` section of README.md for more details.

use zaino_testutils::{LIGHTWALLETD_BIN, ZAINOD_BIN, ZCASHD_BIN, ZCASH_CLI_BIN, ZEBRAD_BIN};
use zingo_infra_testutils::services::network::Network;

pub mod client_rpcs {
    use super::*;

    #[ignore = "not a test. generates chain cache for client_rpc tests."]
    #[tokio::test]
    async fn generate_zcashd_chain_cache() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::generate_zcashd_chain_cache(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[ignore = "not a test. generates chain cache for client_rpc tests."]
    #[tokio::test]
    async fn generate_zebrad_large_chain_cache() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::generate_zebrad_large_chain_cache(
            ZEBRAD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_lightd_info() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_lightd_info(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_latest_block() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_latest_block(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn get_block_out_of_bounds() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_out_of_bounds(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block_nullifiers() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_nullifiers(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block_range_nullifiers() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_range_nullifiers(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block_range_nullifiers_reverse() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_range_nullifiers_reverse(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block_range_lower() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_range_lower(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block_range_upper() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_range_upper(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_block_range_reverse() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_range_reverse(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn get_block_range_out_of_bounds() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_block_range_out_of_bounds(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn get_transaction() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_transaction(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn send_transaction() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::send_transaction(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_taddress_txids_all() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_taddress_txids_all(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_taddress_txids_lower() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_taddress_txids_lower(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_taddress_txids_upper() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_taddress_txids_upper(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_taddress_balance() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_taddress_balance(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_taddress_balance_stream() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_taddress_balance_stream(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn get_mempool_tx() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_mempool_tx(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn get_mempool_stream_zingolib_mempool_monitor() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_mempool_stream_zingolib_mempool_monitor(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// TODO: Fix this test in infrastructure and reactivate here.
    #[ignore = "ignored in infrastructure, fix there first."]
    #[tokio::test]
    async fn get_mempool_stream() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_mempool_stream(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_tree_state_by_height() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_tree_state_by_height(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_tree_state_by_hash() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_tree_state_by_hash(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_tree_state_out_of_bounds() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_tree_state_out_of_bounds(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_latest_tree_state() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_latest_tree_state(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    /// This test requires Zebrad testnet to be already synced to at least 2 sapling shards with the cache at
    /// `zaino/chain_cache/get_subtree_roots_sapling`
    ///
    /// See doc comments of test_fixture for more details.
    #[ignore = "flaky"]
    #[tokio::test]
    async fn get_subtree_roots_sapling() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_subtree_roots_sapling(
            ZEBRAD_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
            Network::Testnet,
        )
        .await;
    }

    /// This test requires Zebrad testnet to be already synced to at least 2 orchard shards with the cache at
    /// `zaino/chain_cache/get_subtree_roots_orchard`
    ///
    /// See doc comments of test_fixture for more details.
    #[ignore = "flaky"]
    #[tokio::test]
    async fn get_subtree_roots_orchard() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_subtree_roots_orchard(
            ZEBRAD_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
            Network::Mainnet,
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_all() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_all(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_lower() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_lower(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_upper() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_upper(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_out_of_bounds() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_out_of_bounds(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_stream_all() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_stream_all(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_stream_lower() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_stream_lower(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_stream_upper() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_stream_upper(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }

    #[tokio::test]
    async fn get_address_utxos_stream_out_of_bounds() {
        tracing_subscriber::fmt().init();

        zingo_infra_testutils::test_fixtures::get_address_utxos_stream_out_of_bounds(
            ZCASHD_BIN.clone(),
            ZCASH_CLI_BIN.clone(),
            ZAINOD_BIN.clone(),
            LIGHTWALLETD_BIN.clone(),
        )
        .await;
    }
}
