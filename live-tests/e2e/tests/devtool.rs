//! Wallet-to-validator end-to-end tests: an in-process librustzcash wallet
//! (faucet + recipient) drives a Zaino-in-a-pod over its gRPC / JSON-RPC
//! surface.
//!
//! Covered (the full zebrad fetch + state query/send surface):
//! - sends to each pool, `send_to_all`, shielding, mining-reward receipt;
//! - `get_transaction` (mined / mempool), `get_raw_transaction`;
//! - mempool: `get_raw_mempool`, `get_mempool_tx`, `get_mempool_stream`,
//!   `get_mempool_info`;
//! - address queries: `get_address_tx_ids`, `get_address_utxos`,
//!   `get_address_balance`, the `get_taddress_*` family (recipient and
//!   faucet-coinbase variants), `get_address_transactions_regtest`;
//! - tree state: `z_get_treestate`, `z_get_subtrees_by_index`;
//! - block range: default/all pools and the out-of-range edge cases;
//! - compact-block transparent data;
//! - `getblockdeltas` spend-resolution (AP-03 / Zellic #48500) and the
//!   coinbase-only edge case;
//! - `connect_to_node_get_info` (wallet `get_info` smoke).
//!
//! Dual `*_fetch_vs_state` tests assert the fetch and state backends agree.
//!
//! Deferred (documented `#[ignore]` stubs, names preserved — see each stub for
//! the ztest gap and the re-home target):
//! - `send_to_transparent_finalization` — heavy seam-deep advance;
//! - `monitor_unverified_mempool` — no unconfirmed-balance wallet accessor;
//! - `address_deltas` — `getaddressdeltas` has no pod JSON-RPC surface;
//! - `get_outpoint_spenders_fetch_vs_state` — in-process `ChainScope` API.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

use e2e::{assert_pool_absent, assert_pool_present, Pool};

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;
/// Shielded funding pool for the faucet coinbase. Mirrors dev's
/// `SHIELDED_FUNDING_POOL = MinerPool::Orchard`: the miner coinbase pays the
/// Orchard receiver of a unified address. Under this file's NU6.3-active regtest
/// schedule (Ironwood live from height 2), that Orchard-receiver coinbase note
/// is credited to the Ironwood pool — hence `receives_mining_reward` asserts an
/// Ironwood balance.
const FUND: Pool = Pool::Orchard;
/// Blocks to mine past a transaction's block to bury it below the finalisation
/// seam (so it crosses `tip - seam` into the finalized DB). Mirrors dev's
/// `FAST_TEST_MAX_NONFINALISED_DEPTH` (100, under `fast-test-seam`) plus a small
/// margin to keep the boundary unambiguous. The e2e crate links no production
/// code, so the seam depth is inlined here rather than read from
/// `zaino_common::consensus`.
const SEAM_ADVANCE: u32 = 105;

/// Pool-type filters for `GetBlockRange`, matching zaino's `PoolType` wire enum:
/// transparent=1, sapling=2, orchard=3, ironwood=4. Ironwood must be included:
/// zaino's default (empty `poolTypes`) filter is every shielded pool
/// (`PoolTypeFilter::default` = sapling+orchard+ironwood), so the explicit
/// shielded set has to match it or the default-vs-explicit parity checks diverge.
const ALL_POOLS: [i32; 4] = [1, 2, 3, 4];
const SHIELDED_POOLS: [i32; 3] = [2, 3, 4];

/// The tip block hash (display order), via the indexer's `getbestblockhash`.
async fn best_block_hash(irpc: &JsonRpcClient) -> Result<String> {
    let v = irpc
        .call_value("getbestblockhash", serde_json::json!([]))
        .await?;
    Ok(v.as_str()
        .expect("getbestblockhash returns a hash string")
        .to_string())
}

/// The string elements of a JSON array response (e.g. the `getrawmempool`
/// txid list); non-strings and non-arrays yield an empty vec.
fn json_string_array(v: serde_json::Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A `getblockdeltas` delta's `outputs` array (empty if absent).
fn delta_outputs(delta: &serde_json::Value) -> Vec<serde_json::Value> {
    delta
        .get("outputs")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A `getblockdeltas` delta's `inputs` array (empty if absent).
fn delta_inputs(delta: &serde_json::Value) -> Vec<serde_json::Value> {
    delta
        .get("inputs")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The `satoshis` (zatoshis) of a `getblockdeltas` output entry.
fn output_satoshis(output: &serde_json::Value) -> Option<i64> {
    output.get("satoshis").and_then(serde_json::Value::as_i64)
}

/// getrawmempool as a sorted `Vec<String>` for order-independent parity.
async fn sorted_raw_mempool(irpc: &JsonRpcClient) -> Result<Vec<String>> {
    let mut txids = json_string_array(
        irpc.call_value("getrawmempool", serde_json::json!([]))
            .await?,
    );
    txids.sort();
    Ok(txids)
}

/// `getaddresstxids` over `taddr` in `[start, end]` as a `Vec<String>`.
async fn address_tx_ids(
    irpc: &JsonRpcClient,
    taddr: &str,
    start: u32,
    end: u32,
) -> Result<Vec<String>> {
    let v = irpc
        .call_value(
            "getaddresstxids",
            serde_json::json!([{ "addresses": [taddr], "start": start, "end": end }]),
        )
        .await?;
    Ok(json_string_array(v))
}

mod zebrad {
    use super::*;

    /// Wallet-driven flows. Tests with a StateService twin are split into
    /// `*_fetch` / `*_state` variants; the rest are FetchService-only.
    mod wallet {
        use super::*;

        /// Port of `receives_mining_reward` (FetchService): the faucet's synced
        /// wallet holds a spendable shielded coinbase note.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn receives_mining_reward_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let balances = faucet.balances().await?;
            assert!(
                balances.get(Pool::Ironwood.ztest()) > 0,
                "faucet must hold a spendable Ironwood coinbase note, got {balances:?}"
            );
            Ok(())
        }

        /// Port of `receives_mining_reward` (StateService): the faucet's synced
        /// wallet holds a spendable shielded coinbase note.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn receives_mining_reward_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let balances = faucet.balances().await?;
            assert!(
                balances.get(Pool::Ironwood.ztest()) > 0,
                "faucet must hold a spendable Ironwood coinbase note, got {balances:?}"
            );
            Ok(())
        }

        /// Port of `connect_to_node_get_info` (FetchService): faucet and recipient
        /// wallets connect and sync without error, and the indexer reports node
        /// info (smoke).
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn connect_to_node_get_info_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let _faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            recipient.sync().await?;
            indexer.indexer_info().await?;
            Ok(())
        }

        /// Port of `connect_to_node_get_info` (StateService): faucet and recipient
        /// wallets connect and sync without error, and the indexer reports node
        /// info (smoke).
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn connect_to_node_get_info_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let _faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            recipient.sync().await?;
            indexer.indexer_info().await?;
            Ok(())
        }

        /// Port of the `send_to_pool` family: the faucet sends 250_000 to the
        /// recipient's unified address. The recipient's unified address exposes
        /// an Orchard receiver, but from NU6.3 librustzcash routes the output
        /// value to the Ironwood pool (Orchard is spend-locked), so the receipt
        /// lands in — and is asserted against — the Ironwood balance.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_ironwood_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Ironwood.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Ironwood.ztest()),
                SEND_AMOUNT,
                "recipient Ironwood balance must equal the send"
            );
            Ok(())
        }

        /// Port of the `send_to_pool` family: the faucet sends 250_000 to the
        /// recipient's unified address. The recipient's unified address exposes
        /// an Orchard receiver, but from NU6.3 librustzcash routes the output
        /// value to the Ironwood pool (Orchard is spend-locked), so the receipt
        /// lands in — and is asserted against — the Ironwood balance.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_ironwood_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Ironwood.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Ironwood.ztest()),
                SEND_AMOUNT,
                "recipient Ironwood balance must equal the send"
            );
            Ok(())
        }

        /// Port of the `send_to_pool` family: the faucet sends 250_000 to the
        /// recipient's sapling address; the recipient's synced wallet shows it.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_sapling_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Sapling.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Sapling.ztest()),
                SEND_AMOUNT,
                "recipient Sapling balance must equal the send"
            );
            Ok(())
        }

        /// Port of the `send_to_pool` family: the faucet sends 250_000 to the
        /// recipient's sapling address; the recipient's synced wallet shows it.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_sapling_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Sapling.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Sapling.ztest()),
                SEND_AMOUNT,
                "recipient Sapling balance must equal the send"
            );
            Ok(())
        }

        /// Port of the `send_to_pool` family: the faucet sends 250_000 to the
        /// recipient's transparent address; the recipient's synced wallet shows
        /// it.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_transparent_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT,
                "recipient Transparent balance must equal the send"
            );
            Ok(())
        }

        /// Port of the `send_to_pool` family: the faucet sends 250_000 to the
        /// recipient's transparent address; the recipient's synced wallet shows
        /// it.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_transparent_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT,
                "recipient Transparent balance must equal the send"
            );
            Ok(())
        }

        /// Port of `send_to_all` (FetchService): one faucet funds a send to all
        /// three pools; each recipient pool reports 250_000.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_all_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            // Three notes — one per send (no chaining of unconfirmed change).
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 3)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            // NU6.3: the unified-address (Orchard-receiver) output routes to Ironwood.
            for pool in [Pool::Ironwood, Pool::Sapling, Pool::Transparent] {
                let addr = recipient.address(pool.ztest()).await?;
                faucet.send(&addr, SEND_AMOUNT).await?;
            }
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balances = recipient.balances().await?;
            assert_eq!(balances.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            // From NU6.3 the unified-address output routes to Ironwood; the
            // orchard pool must stay empty (a nonzero orchard here means the
            // receipt was mislabelled, not merely misrouted).
            assert_eq!(balances.get(Pool::Orchard.ztest()), 0);
            assert_eq!(balances.get(Pool::Sapling.ztest()), SEND_AMOUNT);
            assert_eq!(balances.get(Pool::Transparent.ztest()), SEND_AMOUNT);
            Ok(())
        }

        /// Port of `send_to_all` (StateService): one faucet funds a send to all
        /// three pools; each recipient pool reports 250_000.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn send_to_all_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            // Three notes — one per send (no chaining of unconfirmed change).
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 3)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            // NU6.3: the unified-address (Orchard-receiver) output routes to Ironwood.
            for pool in [Pool::Ironwood, Pool::Sapling, Pool::Transparent] {
                let addr = recipient.address(pool.ztest()).await?;
                faucet.send(&addr, SEND_AMOUNT).await?;
            }
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balances = recipient.balances().await?;
            assert_eq!(balances.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            // From NU6.3 the unified-address output routes to Ironwood; the
            // orchard pool must stay empty (a nonzero orchard here means the
            // receipt was mislabelled, not merely misrouted).
            assert_eq!(balances.get(Pool::Orchard.ztest()), 0);
            assert_eq!(balances.get(Pool::Sapling.ztest()), SEND_AMOUNT);
            assert_eq!(balances.get(Pool::Transparent.ztest()), SEND_AMOUNT);
            Ok(())
        }

        /// Port of `shield_for_validator` (FetchService): the recipient receives a
        /// transparent 250_000, shields it (to Ironwood from NU6.3), and reports
        /// 250_000 − 15_000 fee.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn shield_for_validator_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT
            );

            recipient.shield().await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Ironwood.ztest()),
                SEND_AMOUNT - SHIELD_FEE,
                "shielded balance must be the send net of the ZIP-317 fee \
                 (NU6.3 shields transparent funds into the Ironwood pool)"
            );
            Ok(())
        }

        /// Port of `shield_for_validator` (StateService): the recipient receives a
        /// transparent 250_000, shields it (to Ironwood from NU6.3), and reports
        /// 250_000 − 15_000 fee.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn shield_for_validator_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT
            );

            recipient.shield().await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Ironwood.ztest()),
                SEND_AMOUNT - SHIELD_FEE,
                "shielded balance must be the send net of the ZIP-317 fee \
                 (NU6.3 shields transparent funds into the Ironwood pool)"
            );
            Ok(())
        }

        /// Port of `send_to_transparent_finalization` (FetchService): a transparent
        /// send returns the same address txids from the non-finalised chain and
        /// again after a seam-deep advance lands it in the finalised DB. Heavy: the
        /// advance mines `SEAM_ADVANCE` (~105) shielded coinbase blocks to cross
        /// the seam. Gated + ignored-by-default; un-ignore for manual /
        /// dedicated CI, or once cheap transparent filler mining lands.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(
            not(feature = "devtool-incompatible"),
            ignore = "heavy: mines ~105 shielded-coinbase blocks (~105 groth16 proofs) to bury the send below the finalisation seam; un-ignore for manual / dedicated CI or when cheap transparent filler mining lands"
        )]
        async fn send_to_transparent_finalization_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            // The send's block, queried while it is still in the non-finalised window.
            let irpc = indexer.json_rpc().await?;
            let height = u32::from(indexer.latest_block_height().await?);
            let unfinalised_txids = address_tx_ids(&irpc, &taddr, height, height).await?;

            // The load-bearing advance: push the send below the seam so it crosses
            // the finalised floor (`tip - seam`) into the finalized DB.
            let tip = validator.generate_blocks(SEAM_ADVANCE).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let finalised_txids = address_tx_ids(&irpc, &taddr, height, height).await?;

            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT,
                "the transparent send must still be served after it finalizes"
            );
            assert_eq!(
                unfinalised_txids, finalised_txids,
                "the address txids must be identical across the finalisation seam"
            );
            Ok(())
        }

        /// Port of `send_to_transparent_finalization` (StateService): a transparent
        /// send returns the same address txids from the non-finalised chain and
        /// again after a seam-deep advance lands it in the finalised DB. Heavy: the
        /// advance mines `SEAM_ADVANCE` (~105) shielded coinbase blocks to cross
        /// the seam. Gated + ignored-by-default; un-ignore for manual /
        /// dedicated CI, or once cheap transparent filler mining lands.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(
            not(feature = "devtool-incompatible"),
            ignore = "heavy: mines ~105 shielded-coinbase blocks (~105 groth16 proofs) to bury the send below the finalisation seam; un-ignore for manual / dedicated CI or when cheap transparent filler mining lands"
        )]
        async fn send_to_transparent_finalization_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            // The send's block, queried while it is still in the non-finalised window.
            let irpc = indexer.json_rpc().await?;
            let height = u32::from(indexer.latest_block_height().await?);
            let unfinalised_txids = address_tx_ids(&irpc, &taddr, height, height).await?;

            // The load-bearing advance: push the send below the seam so it crosses
            // the finalised floor (`tip - seam`) into the finalized DB.
            let tip = validator.generate_blocks(SEAM_ADVANCE).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let finalised_txids = address_tx_ids(&irpc, &taddr, height, height).await?;

            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT,
                "the transparent send must still be served after it finalizes"
            );
            assert_eq!(
                unfinalised_txids, finalised_txids,
                "the address txids must be identical across the finalisation seam"
            );
            Ok(())
        }

        /// Port of `get_transaction_mined` (smoke, FetchService): the indexer
        /// serves `get_transaction` for the mined orchard send by txid.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction_mined_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Orchard.ztest()).await?;
            let txid = faucet
                .send(&addr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let _ = indexer.get_transaction(txid).await?;
            Ok(())
        }

        /// Port of `get_transaction_mined` (smoke, StateService): the indexer
        /// serves `get_transaction` for the mined orchard send by txid.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction_mined_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Orchard.ztest()).await?;
            let txid = faucet
                .send(&addr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let _ = indexer.get_transaction(txid).await?;
            Ok(())
        }

        /// Port of `get_raw_mempool` (FetchService): the indexer's `getrawmempool`
        /// matches the validator's, with two unmined transactions. `getrawmempool`
        /// returns the mempool txid set in unspecified order; sort both txid lists
        /// and compare as sets.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_mempool_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut validator_txids = json_string_array(
                validator
                    .json_rpc()
                    .await?
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?,
            );
            let mut indexer_txids = json_string_array(
                indexer
                    .json_rpc()
                    .await?
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?,
            );
            validator_txids.sort();
            indexer_txids.sort();
            assert_eq!(
                validator_txids, indexer_txids,
                "getrawmempool txid sets must agree (order-insensitive)"
            );
            Ok(())
        }

        /// Port of `get_raw_mempool` (StateService): the indexer's `getrawmempool`
        /// matches the validator's, with two unmined transactions. `getrawmempool`
        /// returns the mempool txid set in unspecified order; sort both txid lists
        /// and compare as sets.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_mempool_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut validator_txids = json_string_array(
                validator
                    .json_rpc()
                    .await?
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?,
            );
            let mut indexer_txids = json_string_array(
                indexer
                    .json_rpc()
                    .await?
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?,
            );
            validator_txids.sort();
            indexer_txids.sort();
            assert_eq!(
                validator_txids, indexer_txids,
                "getrawmempool txid sets must agree (order-insensitive)"
            );
            Ok(())
        }

        /// Port of `get_mempool_tx`: `get_mempool_tx` returns the two unmined
        /// transactions, and the exclude-by-txid-suffix filter drops one.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_tx() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            let t_txid = faucet
                .send(&taddr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("txid");
            let u_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("txid");
            tokio::time::sleep(Duration::from_secs(1)).await;

            let mut want = [t_txid.as_ref().to_vec(), u_txid.as_ref().to_vec()];
            want.sort();

            let mut all = indexer.get_mempool_tx(Vec::new()).await?;
            all.sort_by_key(|tx| tx.txid.clone());
            assert_eq!(all.len(), 2, "both unmined txs must be present");
            assert_eq!(all[0].txid, want[0]);
            assert_eq!(all[1].txid, want[1]);

            // Excluding the first by its txid suffix leaves only the second.
            let remaining = indexer.get_mempool_tx(vec![want[0][8..].to_vec()]).await?;
            assert_eq!(remaining.len(), 1, "excluding one leaves the other");
            assert_eq!(remaining[0].txid, want[1]);
            Ok(())
        }

        /// Port of `get_mempool_stream` (smoke): a mempool subscription observes
        /// unmined transactions. zaino's GetMempoolStream snapshots the current
        /// mempool then stays open until a block is mined, so the drain is spawned
        /// and a block mined concurrently (draining before mining hangs):
        /// subscribe, mine to close, then collect.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_stream() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            tokio::time::sleep(Duration::from_secs(1)).await;

            let drain = tokio::spawn({
                let indexer = indexer.clone();
                async move { indexer.get_mempool_stream().await }
            });
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let txs = drain.await.expect("mempool-stream drain task joins")?;
            assert!(
                !txs.is_empty(),
                "mempool stream must observe the unmined txs"
            );
            Ok(())
        }

        /// Port of `get_mempool_info` (FetchService): `getmempoolinfo` matches
        /// values recomputed from the mempool's own contents (`size` and `bytes`
        /// from the mempool-stream's serialized transactions). `usage == bytes + Σ
        /// txid-key heap-capacity` is an in-process detail with no pod surface, so
        /// we assert `usage >= bytes` instead.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_info_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Query getmempoolinfo while the two txs are still unmined (mining
            // below clears the mempool, so this must come first).
            let info = indexer
                .json_rpc()
                .await?
                .call_value("getmempoolinfo", serde_json::json!([]))
                .await?;

            // The mempool-stream carries each unmined tx's serialized bytes;
            // recompute the expected byte total from them. As above, spawn the
            // drain and mine concurrently; draining before mining hangs.
            let drain = tokio::spawn({
                let indexer = indexer.clone();
                async move { indexer.get_mempool_stream().await }
            });
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let txs = drain.await.expect("mempool-stream drain task joins")?;
            let expected_bytes: u64 = txs.iter().map(|tx| tx.data.len() as u64).sum();

            let size = info.get("size").and_then(serde_json::Value::as_u64);
            let bytes = info.get("bytes").and_then(serde_json::Value::as_u64);
            let usage = info.get("usage").and_then(serde_json::Value::as_u64);

            assert_eq!(size, Some(txs.len() as u64), "size must equal the tx count");
            assert!(size.is_some_and(|s| s >= 1), "size must be at least one");
            assert!(bytes.is_some_and(|b| b > 0), "bytes must be positive");
            assert_eq!(
                bytes,
                Some(expected_bytes),
                "bytes must equal Σ serialized-tx lengths"
            );
            assert!(
                matches!((usage, bytes), (Some(u), Some(b)) if u >= b),
                "usage must be at least bytes: {info:?}"
            );
            Ok(())
        }

        /// Port of `get_mempool_info` (StateService): `getmempoolinfo` matches
        /// values recomputed from the mempool's own contents (`size` and `bytes`
        /// from the mempool-stream's serialized transactions). `usage == bytes + Σ
        /// txid-key heap-capacity` is an in-process detail with no pod surface, so
        /// we assert `usage >= bytes` instead.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_mempool_info_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Query getmempoolinfo while the two txs are still unmined (mining
            // below clears the mempool, so this must come first).
            let info = indexer
                .json_rpc()
                .await?
                .call_value("getmempoolinfo", serde_json::json!([]))
                .await?;

            // The mempool-stream carries each unmined tx's serialized bytes;
            // recompute the expected byte total from them. As above, spawn the
            // drain and mine concurrently; draining before mining hangs.
            let drain = tokio::spawn({
                let indexer = indexer.clone();
                async move { indexer.get_mempool_stream().await }
            });
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            let txs = drain.await.expect("mempool-stream drain task joins")?;
            let expected_bytes: u64 = txs.iter().map(|tx| tx.data.len() as u64).sum();

            let size = info.get("size").and_then(serde_json::Value::as_u64);
            let bytes = info.get("bytes").and_then(serde_json::Value::as_u64);
            let usage = info.get("usage").and_then(serde_json::Value::as_u64);

            assert_eq!(size, Some(txs.len() as u64), "size must equal the tx count");
            assert!(size.is_some_and(|s| s >= 1), "size must be at least one");
            assert!(bytes.is_some_and(|b| b > 0), "bytes must be positive");
            assert_eq!(
                bytes,
                Some(expected_bytes),
                "bytes must equal Σ serialized-tx lengths"
            );
            assert!(
                matches!((usage, bytes), (Some(u), Some(b)) if u >= b),
                "usage must be at least bytes: {info:?}"
            );
            Ok(())
        }

        /// Port of `monitor_unverified_mempool` (FetchService): broadcast two
        /// unmined sends, observe them in the mempool, then mine them in and
        /// confirm the balances. The *unconfirmed* (mempool) pool-balance split
        /// under test cannot be asserted — ztest's librustzcash wallet exposes no
        /// pending/unconfirmed pool-balance accessor — so the confirmed balances
        /// stand in. Ignored-by-default.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(
            not(feature = "devtool-incompatible"),
            ignore = "ztest's Wallet::librustzcash exposes no unconfirmed/pending pool-balance accessor, so the unconfirmed-vs-confirmed balance split under test cannot be asserted yet — un-ignore when ztest surfaces unconfirmed balances"
        )]
        async fn monitor_unverified_mempool_fetch() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            // Two shielded notes — one per unmined send.
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Ironwood.ztest()).await?;
            let zaddr = recipient.address(Pool::Sapling.ztest()).await?;
            let ua_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let sapling_txid = faucet
                .send(&zaddr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Both unmined sends must be observable in the mempool.
            let irpc = indexer.json_rpc().await?;
            let mempool = sorted_raw_mempool(&irpc).await?;
            assert!(
                mempool.contains(&ua_txid.to_string())
                    && mempool.contains(&sapling_txid.to_string()),
                "both unmined sends must be visible in the mempool: {mempool:?}"
            );

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            let balances = recipient.balances().await?;
            assert_eq!(balances.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            assert_eq!(balances.get(Pool::Sapling.ztest()), SEND_AMOUNT);
            Ok(())
        }

        /// Port of `monitor_unverified_mempool` (StateService): broadcast two
        /// unmined sends, observe them in the mempool, then mine them in and
        /// confirm the balances. The *unconfirmed* (mempool) pool-balance split
        /// under test cannot be asserted — ztest's librustzcash wallet exposes no
        /// pending/unconfirmed pool-balance accessor — so the confirmed balances
        /// stand in. Ignored-by-default.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        #[cfg_attr(
            not(feature = "devtool-incompatible"),
            ignore = "ztest's Wallet::librustzcash exposes no unconfirmed/pending pool-balance accessor, so the unconfirmed-vs-confirmed balance split under test cannot be asserted yet — un-ignore when ztest surfaces unconfirmed balances"
        )]
        async fn monitor_unverified_mempool_state() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let vol = env.shared_volume("zebra-db");
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(FUND.ztest())
                    .mount(&vol),
            );
            let indexer = env.add_indexer(
                dev!(Indexer::Zainod, "../../Dockerfile")
                    .regtest()
                    .tuning(ZainoTuning::State)
                    .mount(&vol)
                    .named("zaino-state"),
            );
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            // Two shielded notes — one per unmined send.
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 2)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Ironwood.ztest()).await?;
            let zaddr = recipient.address(Pool::Sapling.ztest()).await?;
            let ua_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let sapling_txid = faucet
                .send(&zaddr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            tokio::time::sleep(Duration::from_secs(1)).await;

            // Both unmined sends must be observable in the mempool.
            let irpc = indexer.json_rpc().await?;
            let mempool = sorted_raw_mempool(&irpc).await?;
            assert!(
                mempool.contains(&ua_txid.to_string())
                    && mempool.contains(&sapling_txid.to_string()),
                "both unmined sends must be visible in the mempool: {mempool:?}"
            );

            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            let balances = recipient.balances().await?;
            assert_eq!(balances.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            assert_eq!(balances.get(Pool::Sapling.ztest()), SEND_AMOUNT);
            Ok(())
        }

        /// Port of `get_address_tx_ids`: `getaddresstxids` over the recipient's
        /// taddr returns the send's txid (asserted as the first txid returned).
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_tx_ids() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let txid = faucet
                .send(&taddr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let start = u32::from(indexer.latest_block_height().await?).saturating_sub(2);
            let res = indexer
                .json_rpc()
                .await?
                .call_value(
                    "getaddresstxids",
                    serde_json::json!([{ "addresses": [taddr], "start": start }]),
                )
                .await?;
            let txids = json_string_array(res);
            assert_eq!(
                txids[0],
                txid.to_string(),
                "getaddresstxids first txid must be the send {txid}, got {txids:?}"
            );
            Ok(())
        }

        /// Port of `get_address_utxos`: `z_getaddressutxos` over the recipient's
        /// taddr returns a utxo whose txid is the send's.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_utxos() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            let txid = faucet
                .send(&taddr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let utxos = indexer
                .get_address_utxos(vec![taddr], BlockHeight::from(0u32), 0)
                .await?;
            assert_eq!(
                utxos[0].txid,
                txid.as_ref().to_vec(),
                "utxo[0] txid must be the send"
            );
            Ok(())
        }

        /// Port of `z_get_treestate` (smoke): tree state at the tip succeeds.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_treestate() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let tip = indexer.latest_block_height().await?;
            let _ = indexer.get_tree_state(tip).await?;
            Ok(())
        }

        /// Port of `z_get_subtrees_by_index` (smoke): orchard subtree roots
        /// succeed.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn z_get_subtrees_by_index() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&addr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let _ = indexer
                .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                .await?;
            Ok(())
        }

        /// Port of `get_raw_transaction` (smoke): `getrawtransaction` for the
        /// orchard send's txid succeeds.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_raw_transaction() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Orchard.ztest()).await?;
            let txid = faucet
                .send(&addr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let _ = indexer
                .json_rpc()
                .await?
                .call_value(
                    "getrawtransaction",
                    serde_json::json!([txid.to_string(), 1]),
                )
                .await?;
            Ok(())
        }

        /// Port of `get_taddress_txids` (smoke): `get_taddress_txids` over the
        /// recipient's taddr and a range around the send succeeds.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_txids() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let tip = indexer.latest_block_height().await?;
            let start = BlockHeight::from(u32::from(tip).saturating_sub(2));
            let _ = indexer.get_taddress_txids(taddr, start, tip).await?;
            Ok(())
        }

        /// Port of `get_taddress_utxos` (smoke): `get_address_utxos` over the
        /// recipient's taddr succeeds.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_utxos() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let _ = indexer
                .get_address_utxos(vec![taddr], BlockHeight::from(0u32), 0)
                .await?;
            Ok(())
        }

        /// Port of `get_taddress_utxos_stream` (smoke): `get_address_utxos_stream`
        /// over the recipient's taddr succeeds.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_utxos_stream() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let _ = indexer
                .get_address_utxos_stream(vec![taddr], BlockHeight::from(0u32), 0)
                .await?;
            Ok(())
        }

        /// Port of `get_transaction_mempool` (smoke): the indexer serves
        /// `get_transaction` for an unmined orchard send from the mempool.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_transaction_mempool() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let addr = recipient.address(Pool::Orchard.ztest()).await?;
            let txid = faucet
                .send(&addr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("txid");
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = indexer.get_transaction(txid).await?;
            Ok(())
        }

        /// Port of `get_address_balance`: `getaddressbalance` over the
        /// recipient's taddr reports exactly 250_000.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_address_balance() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let res = indexer
                .json_rpc()
                .await?
                .call_value(
                    "getaddressbalance",
                    serde_json::json!([{ "addresses": [taddr] }]),
                )
                .await?;
            assert_eq!(
                res.get("balance").and_then(serde_json::Value::as_u64),
                Some(SEND_AMOUNT),
                "getaddressbalance must report the send amount: {res:?}"
            );
            Ok(())
        }

        /// Port of `get_taddress_balance`: `GetTaddressBalance` over the
        /// recipient's taddr reports 250_000.
        #[ztest::qos::integration]
        #[tokio::test(flavor = "multi_thread")]
        async fn get_taddress_balance() -> Result<()> {
            let mut env = TestEnv::builder().ready_timeout(READY);
            let validator =
                env.add_validator(Validator::zebrad("6.2.0").regtest().mine_to(FUND.ztest()));
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;

            let bal = indexer.get_taddress_balance(vec![taddr]).await?;
            assert_eq!(
                u64::try_from(i64::from(bal)).unwrap_or(0),
                SEND_AMOUNT,
                "get_taddress_balance must report the send amount"
            );
            Ok(())
        }
    }

    // These compare a fetch-backend zainod pod against a state-backend zainod
    // pod, both reading one shared zebrad regtest chain. Each test stands up one
    // zebrad on a shared volume, a fetch zainod (`regtest`) and a state zainod
    // (`tuning(ZainoTuning::State)`) inline, and reproduces the exact
    // `assert_eq!(fetch, state)` comparison over the pods' gRPC / JSON-RPC surface.

    /// Port of `block_range_returns_default_pools`: `get_block_range` with no
    /// pools == requesting the shielded pools, fetch==state, and the tip block
    /// holds the shielded coinbase + the send with no transparent data.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_default_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // fund_and_send(Orchard): one shielded coinbase note, then send it to the
        // recipient's unified address and mine the send in.
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let ua = recipient.address(Pool::Orchard.ztest()).await?;
        faucet.send(&ua, SEND_AMOUNT).await?;
        let end = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(end, READY).await?;
        state.wait_for_block_num(end, READY).await?;
        let start = BlockHeight::from(1u32);

        let fetch_default = fetch.get_block_range(start, end).await?;
        let fetch_shielded = fetch
            .get_block_range_with_pools(start, end, SHIELDED_POOLS.to_vec())
            .await?;
        assert_eq!(fetch_default, fetch_shielded);

        let state_shielded = state
            .get_block_range_with_pools(start, end, SHIELDED_POOLS.to_vec())
            .await?;
        let state_default = state.get_block_range(start, end).await?;
        assert_eq!(state_default, state_shielded);

        assert_eq!(fetch_default, state_default);

        let compact_block = state_default.last().expect("non-empty range");
        assert_eq!(BlockHeight::from(compact_block.height as u32), end);
        // The tip block holds the shielded coinbase and the send.
        assert_eq!(compact_block.vtx.len(), 2);
        assert_eq!(compact_block.vtx.last().expect("send tx").index, 1);
        for tx in &compact_block.vtx {
            assert!(
                tx.vin.is_empty(),
                "transparent data should be absent when no pool types are requested"
            );
            assert!(
                tx.vout.is_empty(),
                "transparent data should be absent when no pool types are requested"
            );
        }
        Ok(())
    }

    /// Port of `block_range_returns_all_pools`: with all pools requested the
    /// fetch and state indexers agree, and the tip block carries the coinbase
    /// plus all three sends with their pool data.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_all_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // Three shielded coinbase notes (one per send below), then one send to
        // each pool's recipient address, mined into a single block.
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 3)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let mut txids = Vec::new();
        // NU6.3: the unified-address (Orchard-receiver) send emits Ironwood
        // actions, so the compact block carries them under `ironwood_actions`.
        for pool in [Pool::Transparent, Pool::Sapling, Pool::Ironwood] {
            let addr = recipient.address(pool.ztest()).await?;
            let txid = faucet
                .send(&addr, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("txid");
            txids.push(txid);
        }
        let end = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(end, READY).await?;
        state.wait_for_block_num(end, READY).await?;
        let start = BlockHeight::from(1u32);

        let fetch_range = fetch
            .get_block_range_with_pools(start, end, ALL_POOLS.to_vec())
            .await?;
        let state_range = state
            .get_block_range_with_pools(start, end, ALL_POOLS.to_vec())
            .await?;
        assert_eq!(fetch_range, state_range);

        let compact_block = state_range.last().expect("non-empty range");
        assert_eq!(BlockHeight::from(compact_block.height as u32), end);
        // coinbase + the three sends
        assert_eq!(compact_block.vtx.len(), 4);

        assert_pool_present(compact_block, &txids[0], Pool::Transparent);
        assert_pool_present(compact_block, &txids[1], Pool::Sapling);
        assert_pool_present(compact_block, &txids[2], Pool::Ironwood);
        // The unified-address send must carry no Orchard actions from NU6.3.
        assert_pool_absent(compact_block, &txids[2], Pool::Orchard);
        Ok(())
    }

    /// Port of `z_get_treestate_fetch_vs_state`: the fetch and state indexers
    /// agree on the tree state at the tip.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let tip = fetch.latest_block_height().await?;
        assert_eq!(
            fetch.get_tree_state(tip).await?,
            state.get_tree_state(tip).await?
        );
        Ok(())
    }

    /// Port of `z_get_subtrees_by_index_fetch_vs_state`: the fetch and state
    /// indexers agree on orchard subtree roots.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        assert_eq!(
            fetch
                .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                .await?,
            state
                .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                .await?,
        );
        Ok(())
    }

    /// Port of `get_raw_transaction_fetch_vs_state`: the fetch and state indexers
    /// agree on `getrawtransaction` for the send.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let addr = recipient.address(Pool::Orchard.ztest()).await?;
        let txid = faucet
            .send(&addr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let params = serde_json::json!([txid.to_string(), 1]);
        assert_eq!(
            fetch
                .json_rpc()
                .await?
                .call_value("getrawtransaction", params.clone())
                .await?,
            state
                .json_rpc()
                .await?
                .call_value("getrawtransaction", params)
                .await?,
        );
        Ok(())
    }

    /// Port of `get_address_tx_ids_fetch_vs_state`: `getaddresstxids` over the
    /// recipient's taddr returns the send txid, and the fetch and state indexers
    /// agree.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let tip = u32::from(fetch.latest_block_height().await?);
        let (start, end) = (tip.saturating_sub(2), tip);
        let fetch_txids = address_tx_ids(&fetch.json_rpc().await?, &taddr, start, end).await?;
        let state_txids = address_tx_ids(&state.json_rpc().await?, &taddr, start, end).await?;
        assert_eq!(fetch_txids[0], txid.to_string());
        assert_eq!(fetch_txids, state_txids);
        Ok(())
    }

    /// Port of `get_address_utxos_fetch_vs_state`: `z_getaddressutxos` over the
    /// recipient's taddr returns the send txid, and the fetch and state indexers
    /// agree.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let z = BlockHeight::from(0u32);
        let fetch_utxos = fetch.get_address_utxos(vec![taddr.clone()], z, 0).await?;
        let state_utxos = state.get_address_utxos(vec![taddr], z, 0).await?;
        assert_eq!(fetch_utxos[0].txid, txid.as_ref().to_vec());
        assert_eq!(fetch_utxos[0].txid, state_utxos[0].txid);
        Ok(())
    }

    /// Port of `get_raw_mempool_fetch_vs_state`: the fetch and state indexers
    /// agree on `getrawmempool` while two sends sit unmined.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 2)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let ua = recipient.address(Pool::Orchard.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        faucet.send(&ua, SEND_AMOUNT).await?;
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(
            sorted_raw_mempool(&fetch.json_rpc().await?).await?,
            sorted_raw_mempool(&state.json_rpc().await?).await?,
        );
        Ok(())
    }

    /// Port of `get_address_transactions_regtest`: after a transparent send, the
    /// state indexer's transparent-address txid query over that taddr yields at
    /// least one transaction.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_transactions_regtest() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let chain_height = fetch.latest_block_height().await?;
        let start = BlockHeight::from(u32::from(chain_height).saturating_sub(2));
        let txids = state.get_taddress_txids(taddr, start, chain_height).await?;
        assert!(
            !txids.is_empty(),
            "at least one tx must touch the recipient taddr"
        );
        Ok(())
    }

    /// Port of `transparent_data_in_compact_block`: with transparent mining,
    /// every compact-block tx carries a transparent vout (the miner's transparent
    /// coinbase is the data source), so each vout's `script_pub_key` is non-empty.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_data_in_compact_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        env.build().await?;

        let chain_height = validator.generate_blocks(5).await?;
        fetch.wait_for_block_num(chain_height, READY).await?;
        state.wait_for_block_num(chain_height, READY).await?;

        // NOTE: Zaino cannot serve the non-standard genesis coinbase script in
        // compact blocks, so this starts at height 1, not 0
        // (zingolabs/zaino#818).
        let range = state
            .get_block_range_with_pools(BlockHeight::from(1u32), chain_height, ALL_POOLS.to_vec())
            .await?;
        for cb in range {
            for tx in cb.vtx {
                assert!(
                    !tx.vout
                        .first()
                        .expect("transparent vout present")
                        .script_pub_key
                        .is_empty(),
                    "each tx's transparent output must carry a script pub key"
                );
            }
        }
        Ok(())
    }

    /// Port of `get_taddress_txids_faucet_fetch_vs_state`: the fetch and state
    /// indexers agree on `getaddresstxids` over the faucet's coinbase taddr. The
    /// non-vacuity probe guards against a silent empty==empty pass. Under
    /// `mine_to(Transparent)` the faucet account is the miner address, so its
    /// taddr holds the coinbase.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &fetch).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
        let tip = validator.generate_blocks(100).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let fetch_txids = address_tx_ids(&fetch.json_rpc().await?, &faucet_taddr, 2, 5).await?;
        let state_txids = address_tx_ids(&state.json_rpc().await?, &faucet_taddr, 2, 5).await?;
        assert!(
            !fetch_txids.is_empty(),
            "faucet taddr must hold coinbase txids in range"
        );
        assert_eq!(fetch_txids, state_txids);
        Ok(())
    }

    /// Port of `get_taddress_balance_faucet_fetch_vs_state`: the fetch and state
    /// indexers agree on the transparent balance of the faucet's coinbase taddr.
    /// The non-vacuity probe guards against 0==0.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &fetch).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
        let tip = validator.generate_blocks(5).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let fetch_bal = fetch
            .get_taddress_balance(vec![faucet_taddr.clone()])
            .await?;
        let state_bal = state.get_taddress_balance(vec![faucet_taddr]).await?;
        assert!(
            i64::from(fetch_bal) > 0,
            "faucet taddr must hold coinbase value"
        );
        assert_eq!(i64::from(fetch_bal), i64::from(state_bal));
        Ok(())
    }

    /// Port of `get_address_utxos_faucet_fetch_vs_state`: the fetch and state
    /// indexers agree on `get_address_utxos` over the faucet's coinbase taddr.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &fetch).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
        let tip = validator.generate_blocks(5).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let start = BlockHeight::from(2u32);
        let fetch_utxos = fetch
            .get_address_utxos(vec![faucet_taddr.clone()], start, 3)
            .await?;
        let state_utxos = state
            .get_address_utxos(vec![faucet_taddr], start, 3)
            .await?;
        assert!(
            !fetch_utxos.is_empty(),
            "faucet taddr must hold coinbase utxos"
        );
        assert_eq!(fetch_utxos, state_utxos);
        Ok(())
    }

    /// Port of `get_address_utxos_stream_faucet_fetch_vs_state`: the streamed
    /// utxos agree between the fetch and state indexers over the faucet's
    /// coinbase taddr.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_stream_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet.faucet(&validator, &fetch).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
        let tip = validator.generate_blocks(5).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let start = BlockHeight::from(2u32);
        let fetch_utxos = fetch
            .get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3)
            .await?;
        let state_utxos = state
            .get_address_utxos_stream(vec![faucet_taddr], start, 3)
            .await?;
        assert!(
            !fetch_utxos.is_empty(),
            "faucet taddr must hold coinbase utxos"
        );
        assert_eq!(fetch_utxos, state_utxos);
        Ok(())
    }

    /// Port of `zebra::get::address_deltas` (`getaddressdeltas`). BLOCKED (ztest
    /// gap): the zaino pod's JSON-RPC serves `getblockdeltas` but NOT
    /// `getaddressdeltas` (only `getblockdeltas`, `getaddressbalance`,
    /// `getaddresstxids`, `getaddressutxos` are `#[method(...)]`-registered in
    /// `zaino-serve`), and dev drove the in-process
    /// `state_subscriber.get_address_deltas(...)` API, which has no gRPC/JSON-RPC
    /// pod surface. This builds the reachable chain the query would run against —
    /// a transparent-mined chain with a transparent send to the recipient, over
    /// the state backend that synthesizes deltas — but stops at the missing RPC.
    /// Ignored-by-default; re-home to a `packages/zaino-state` unit test, or
    /// un-ignore once zaino serves `getaddressdeltas` over the pod.
    ///
    /// Intended assertions: Simple/Filtered/WithChainInfo variants, the recipient
    /// send delta at output index 0, multi-address deltas, start/end clamping to
    /// the tip, and empty deltas for a non-existent address.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "`getaddressdeltas` has no pod JSON-RPC surface in zaino/zainod (zaino-serve registers only getblockdeltas / getaddressbalance / getaddresstxids / getaddressutxos), and dev drove the in-process state_subscriber.get_address_deltas API — un-ignore once zaino serves getaddressdeltas over the pod"
    )]
    async fn address_deltas() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let indexer = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // A transparent-mined chain with a transparent send to the recipient:
        // the deltas under test are the send's credit at the recipient taddr and
        // the faucet coinbase's transparent credits.
        let faucet = wallet.faucet(&validator, &indexer).await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let recipient_taddr = recipient.address(Pool::Transparent.ztest()).await?;
        let tip = validator.generate_blocks(5).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        faucet.sync().await?;
        faucet.send(&recipient_taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        Ok(())
    }

    /// Regression coverage for AP-03 / Zellic #48500
    /// (`get_block_deltas_resolves_transparent_spend`): the State backend used to
    /// silently drop every *non-coinbase transparent spend input* from
    /// `getblockdeltas`, because Zebra's stateless verbosity-2 transaction object
    /// leaves an input's value/address unset. The fix resolves each spend's
    /// prevout and reads the spent output's value/address.
    ///
    /// The faucet pays the recipient a transparent output; the recipient then
    /// shields it, producing a non-coinbase transparent input that references the
    /// funding output. The spend block's `InputDelta` must be present and resolve
    /// to the funding output's address and full (negated) value — pre-fix this
    /// input was dropped and `inputs` was empty, so balances overstated funds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_deltas_resolves_transparent_spend() -> Result<()> {
        // The only transparent output in the funding block: coinbase pays the
        // shielded pool and the faucet's change returns shielded, so the funding
        // output is uniquely identifiable by its amount.
        const FUNDING_AMOUNT: i64 = 250_000;

        // State-backend only: zebra serves no `getblockdeltas` RPC, so only the
        // state backend — which synthesizes the deltas from a verbosity-2 block
        // and resolves each spend's prevout via its `ReadStateService` — can
        // answer it. There is no fetch path and no fetch-vs-state cross-check. The
        // state zainod opens the validator's zebra-state DB as a RocksDB secondary
        // over the shared volume, so the validator mounts the same `vol`
        // (`.mount(&vol)`).
        //
        // Coinbase mines to Orchard. Orchard is invalid before NU5 (height 2) and
        // the miner address pins the pool, so `funded_faucet_with_notes` warms the
        // chain past NU5 before mining the faucet's notes. The coinbase is
        // shielded, so the 250_000 transparent send is the funding block's only
        // transparent output.
        //
        // zebra must link the same orchard 0.15 / zcash_protocol 0.10 as the
        // wallet and miner to verify their proofs; an older 5.2.0 (orchard ~0.13)
        // rejects them with "could not validate orchard proof".
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Orchard.ztest())
                .mount(&vol),
        );
        let indexer = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;
        let irpc = indexer.json_rpc().await?;

        // One shielded coinbase note, then fund the recipient's transparent
        // address with a non-coinbase transparent output.
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let recipient_taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&recipient_taddr, FUNDING_AMOUNT as u64).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let funding_block_hash = best_block_hash(&irpc).await?;

        // The recipient confirms the received output and shields it; the
        // shielding tx spends that output, producing the non-coinbase transparent
        // input under test.
        recipient.sync().await?;
        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let spend_block_hash = best_block_hash(&irpc).await?;

        // Locate the funding output (unique by amount) and capture its
        // txid / index / address.
        let funding = irpc
            .call_value("getblockdeltas", serde_json::json!([funding_block_hash]))
            .await?;
        let funding_delta = funding
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .find(|d| {
                delta_outputs(d)
                    .iter()
                    .any(|o| output_satoshis(o) == Some(FUNDING_AMOUNT))
            })
            .cloned()
            .expect("funding tx paying the recipient should be in its block");
        let funding_output = delta_outputs(&funding_delta)
            .into_iter()
            .find(|o| output_satoshis(o) == Some(FUNDING_AMOUNT))
            .expect("funding output paying the recipient should be present");
        let funding_txid = funding_delta
            .get("txid")
            .and_then(serde_json::Value::as_str)
            .expect("delta txid")
            .to_string();
        let funding_vout = funding_output
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .expect("output index");
        let funding_address = funding_output
            .get("address")
            .and_then(serde_json::Value::as_str)
            .expect("output address")
            .to_string();

        // The spend's input must be present and resolved to the funding output's
        // address and full value (negative — it is a debit). Pre-fix `inputs` was
        // empty and this lookup would fail.
        let spend = irpc
            .call_value("getblockdeltas", serde_json::json!([spend_block_hash]))
            .await?;
        let input = spend
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .flat_map(delta_inputs)
            .find(|i| {
                i.get("prevtxid").and_then(serde_json::Value::as_str) == Some(&funding_txid)
                    && i.get("prevout").and_then(serde_json::Value::as_u64) == Some(funding_vout)
            })
            .expect("spend input referencing the funding output should be present");

        assert_eq!(
            input.get("address").and_then(serde_json::Value::as_str),
            Some(funding_address.as_str()),
            "input must resolve to the prevout's address"
        );
        assert_eq!(
            input.get("satoshis").and_then(serde_json::Value::as_i64),
            Some(-FUNDING_AMOUNT),
            "input must resolve to the prevout's full value, negated"
        );
        Ok(())
    }

    /// Port of `get_block_deltas_coinbase_only_block_has_no_inputs`: a freshly
    /// mined block carries only its (shielded) coinbase transaction — the
    /// coinbase input is skipped and `getblockdeltas` fabricates no transparent
    /// input deltas.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_deltas_coinbase_only_block_has_no_inputs() -> Result<()> {
        // State-backend only (cf. `get_block_deltas_resolves_transparent_spend`):
        // zebra serves no `getblockdeltas` RPC, so only the synthesizing state
        // backend can answer it. Coinbase mines to Orchard; see the sibling test
        // for why the orchard version must match the wallet. This test has no
        // faucet, so it mines the NU5 warmup block itself (height 1, pre-NU5
        // fallback coinbase) and inspects the height-2 block — the first true
        // Orchard coinbase — which carries only its coinbase tx, so
        // `getblockdeltas` fabricates no transparent inputs.
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Orchard.ztest())
                .mount(&vol),
        );
        let indexer = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol),
        );
        env.build().await?;
        let irpc = indexer.json_rpc().await?;

        // Height 1 warms past NU5; height 2 is the Orchard coinbase-only block
        // under test.
        let tip = validator.generate_blocks(2).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let block_hash = best_block_hash(&irpc).await?;

        let deltas = irpc
            .call_value("getblockdeltas", serde_json::json!([block_hash]))
            .await?;
        let all_empty = deltas
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .all(|d| delta_inputs(d).is_empty());
        assert!(all_empty, "a coinbase-only block must have no input deltas");
        Ok(())
    }

    /// Rewrite of `zebra::get::chain_cache::get_outpoint_spenders`
    /// (`get_outpoint_spenders_fetch_vs_state`). BLOCKED (ztest gap): this drives
    /// the IN-PROCESS `zaino_state::ChainIndex` API (`snapshot_nonfinalized_state`
    /// + `get_outpoint_spenders` with `ChainScope::{FullChain, Finalised}`), which
    /// has no gRPC/JSON-RPC surface — not reachable from a ztest pod test.
    /// Re-home to a `packages/zaino-state` unit test.
    ///
    /// Intended assertions: build three transparent outpoints at the recipient
    /// taddr — one spent and buried past the finalised seam (a small margin below
    /// `tip - seam`; ~105 blocks on the operational chain), one spent but left in
    /// the non-finalised window, one left unspent — then for BOTH backends:
    ///   - `ChainScope::FullChain`  => vec![Some(spender_finalised), Some(spender_nonfinalised), None]
    ///   - `ChainScope::Finalised`  => vec![Some(spender_finalised), None, None]
    /// i.e. FullChain resolves both spends, Finalised only the buried one.
    /// Preserved 1:1 by name; do not delete.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    #[cfg_attr(
        not(feature = "devtool-incompatible"),
        ignore = "heavy: mines ~105 shielded-coinbase blocks to bury a finalised spend below the seam AND the outpoint-spender lookup (ChainScope FullChain/Finalised) has no pod gRPC/JSON-RPC surface — un-ignore once zaino exposes get_outpoint_spenders over the pod, for manual / dedicated CI"
    )]
    async fn get_outpoint_spenders_fetch_vs_state() -> Result<()> {
        // The transparent output funded per phase, uniquely identifiable by amount.
        const FUNDING_AMOUNT: u64 = 250_000;

        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        // Three notes — one funds each phase's transparent outpoint.
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 3)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;

        // ---- Phase 1: an outpoint that is SPENT and FINALISED ----
        // Fund the recipient taddr, then shield it (the shield drains all
        // transparent funds, so the recipient holds one UTXO per phase and its
        // spender is unambiguous).
        faucet.send(&taddr, FUNDING_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;

        // Bury the spend below the finalised floor (`tip - seam`).
        let tip = validator.generate_blocks(SEAM_ADVANCE).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        // ---- Phase 2: an outpoint that is SPENT but stays NON-FINALISED ----
        faucet.send(&taddr, FUNDING_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        // ---- Phase 3: an outpoint that is created but left UNSPENT ----
        faucet.send(&taddr, FUNDING_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        Ok(())
    }

    /// Port of `get_address_balance_fetch_vs_state`: the recipient taddr reports
    /// the 250_000 send, and the fetch and state indexers agree.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(FUND.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let params = serde_json::json!([{ "addresses": [taddr] }]);
        let fetch_bal = fetch
            .json_rpc()
            .await?
            .call_value("getaddressbalance", params.clone())
            .await?;
        let state_bal = state
            .json_rpc()
            .await?
            .call_value("getaddressbalance", params)
            .await?;
        assert_eq!(
            fetch_bal.get("balance").and_then(serde_json::Value::as_u64),
            Some(SEND_AMOUNT)
        );
        assert_eq!(fetch_bal, state_bal);
        Ok(())
    }

    /// Port of `get_block_range_out_of_range_upper_bound`: draining [1, 106] on a
    /// 100-block chain yields the 100 available blocks (fetch == state) and then
    /// errors rather than ending cleanly.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_upper_bound() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        env.build().await?;

        let height = u32::from(fetch.latest_block_height().await?);
        let tip = validator
            .generate_blocks(100u32.saturating_sub(height))
            .await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let (start, end) = (BlockHeight::from(1u32), BlockHeight::from(106u32));
        let (fetch_blocks, fetch_errored) = fetch
            .drain_block_range(start, end, ALL_POOLS.to_vec())
            .await?;
        let (state_blocks, state_errored) = state
            .drain_block_range(start, end, ALL_POOLS.to_vec())
            .await?;

        assert_eq!(fetch_blocks, state_blocks);
        let compact_block = state_blocks.last().expect("non-empty range");
        assert!(compact_block.height < 106);
        assert_eq!(fetch_blocks.len(), 100);
        assert!(state_errored, "state stream should terminate with an error");
        assert!(fetch_errored, "fetch stream should terminate with an error");
        Ok(())
    }

    /// Port of `get_block_range_out_of_range_lower_bound`: draining the inverted
    /// range [106, 1] yields no blocks (fetch == state, both empty) and then
    /// errors rather than ending cleanly.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_lower_bound() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.0")
                .regtest()
                .mine_to(Pool::Transparent.ztest())
                .mount(&vol),
        );
        let fetch = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::Fetch)
                .named("zaino-fetch"),
        );
        let state = env.add_indexer(
            dev!(Indexer::Zainod, "../../Dockerfile")
                .regtest()
                .tuning(ZainoTuning::State)
                .mount(&vol)
                .named("zaino-state"),
        );
        env.build().await?;

        let height = u32::from(fetch.latest_block_height().await?);
        let tip = validator
            .generate_blocks(100u32.saturating_sub(height))
            .await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        let (start, end) = (BlockHeight::from(106u32), BlockHeight::from(1u32));
        let (fetch_blocks, fetch_errored) = fetch
            .drain_block_range(start, end, ALL_POOLS.to_vec())
            .await?;
        let (state_blocks, state_errored) = state
            .drain_block_range(start, end, ALL_POOLS.to_vec())
            .await?;

        assert_eq!(fetch_blocks, state_blocks);
        assert!(fetch_blocks.is_empty());
        assert!(state_errored, "state stream should terminate with an error");
        assert!(fetch_errored, "fetch stream should terminate with an error");
        Ok(())
    }
}
