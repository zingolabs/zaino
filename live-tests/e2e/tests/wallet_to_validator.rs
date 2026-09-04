//! Wallet-to-validator end-to-end tests: an in-process librustzcash wallet
//! (faucet + recipient) drives a Zaino-in-a-pod over its gRPC / JSON-RPC
//! surface.
//!
//! Covered (the full query/send surface):
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
//! - `getaddressdeltas` across its five request shapes;
//! - `connect_to_node_get_info` (wallet `get_info` smoke).
//!
//! Dual `*_fetch_vs_state` tests compare zaino's two ingest paths. `backend = 'fetch'`
//! (`Rpc`) reaches the validator over JSON-RPC alone; `backend = 'state'` (`Direct`)
//! additionally runs a zebra `ReadStateService` it syncs itself, and the routing table
//! prefers it for every query it can answer. Both arms build the same chain index through
//! one `NodeBackedIndexerService`, so the axis under test is the transport, not the index.
//!
//! The ingest path and the validator backend are one flat case list, not a matrix:
//! `State` reads the validator's own on-disk zebra DB over a shared volume, which only
//! the zebra backend writes.

use std::time::Duration;

use anyhow::{Context, Result};
use rstest::rstest;
use serde_json::{json, Value};
use zaino_testutils::{assert_rpc_parity, wait_for_finalised};
use ztest::prelude::*;

use e2e::{assert_pool_absent, assert_pool_present, Pool};

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis).
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;
/// Shielded funding pool for the faucet coinbase: the miner coinbase pays the
/// Orchard receiver of a unified address. Under this file's NU6.3-active
/// regtest schedule (Ironwood live from height 2), that note is credited to the
/// Ironwood pool — hence `receives_mining_reward` asserts an Ironwood balance.
const FUND: Pool = Pool::Orchard;
/// Blocks to mine past a transaction's block to bury it below the finalisation
/// seam (so it crosses `tip - seam` into the finalized DB):
/// `FAST_TEST_MAX_NONFINALISED_DEPTH` (100, under `fast-test-seam`) plus margin.
/// Inlined because this crate links no production code.
const SEAM_ADVANCE: u32 = 105;
/// Longest the finalised writer should need to commit up to a height the
/// validator already serves. It commits per batch, so the frontier moves in
/// steps rather than per block.
const SEAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Wallet-driven flows.
mod wallet {
    use super::*;

    /// The indexer's ingest path. `Fetch` reaches the validator over JSON-RPC alone;
    /// `State` additionally runs a zebra `ReadStateService`, synced over the validator's
    /// indexer gRPC into the zebra db on the shared volume, and prefers it for every
    /// query it can answer. Only zebrad writes that db, so `State` pairs with zebrad only.
    #[derive(Copy, Clone, Debug)]
    enum Backend {
        Fetch,
        State,
    }

    /// The faucet's synced wallet holds a spendable shielded coinbase note.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn receives_mining_reward<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        // NU6.3 regtest: orchard coinbase -> Ironwood
        let credited = Pool::Ironwood;
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let balances = faucet.balances().await?;
        assert!(
            balances.get(credited.ztest()) > 0,
            "faucet must hold a spendable {credited:?} coinbase note, got {balances:?}"
        );
        Ok(())
    }

    /// Smoke: faucet and recipient wallets connect and sync without error,
    /// and the indexer reports node info.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn connect_to_node_get_info<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let _faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        recipient.sync().await?;
        assert!(!indexer.indexer_info().await?.chain_name.is_empty());
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's unified address, whose Orchard
    /// receiver credits Ironwood from NU6.3 (Orchard is spend-locked) and Orchard
    /// before it.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_unified<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let credited = Pool::Ironwood;
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
        let wallet = env.add_wallet(Wallet::librustzcash());
        env.build().await?;

        let faucet = wallet
            .funded_faucet_with_notes(&validator, &indexer, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &indexer).await?;
        let addr = recipient.address(credited.ztest()).await?;
        faucet.send(&addr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        assert_eq!(
            recipient.balances().await?.get(credited.ztest()),
            SEND_AMOUNT,
            "recipient {credited:?} balance must equal the send"
        );
        Ok(())
    }

    /// The faucet sends 250_000 to the recipient's sapling address; the
    /// recipient's synced wallet shows it.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_sapling(#[case] backend: Backend) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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

    /// The faucet sends 250_000 to the recipient's transparent address; the
    /// recipient's synced wallet shows it.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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

    /// One faucet funds a send to all three pools; each recipient pool
    /// reports 250_000.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_all(#[case] backend: Backend) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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

    /// The recipient receives a transparent 250_000, shields it, and reports
    /// 250_000 − 15_000 fee in the pool the chain's latest activation shields into.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn shield_for_validator<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let credited = Pool::Ironwood;
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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
            recipient.balances().await?.get(credited.ztest()),
            SEND_AMOUNT - SHIELD_FEE,
            "shielded balance must be the send net of the ZIP-317 fee, in {credited:?}"
        );
        Ok(())
    }

    /// A transparent send returns the same address txids from the
    /// non-finalised chain and again after a seam-deep advance lands it in
    /// the finalised DB.
    ///
    /// Runs on the `fast-test-seam` image: at the operational depth of 1001 a
    /// `SEAM_ADVANCE` of 105 buries nothing, both reads come from the
    /// non-finalised cache, and the comparison below holds trivially. The
    /// `wait_for_finalised` check makes that failure mode loud instead. The
    /// footprint override buys the validator cores for `SEAM_ADVANCE` shielded
    /// coinbase blocks (~105 halo2 proofs).
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn send_to_transparent_finalization<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = validator.regtest().mine_to(FUND.ztest());
        // `fast-test-seam` selects the 100-block seam over the shipped 1001, which no
        // regtest fixture can bury a transaction below; `prometheus` publishes the
        // gauge `wait_for_finalised` polls. A `features` list replaces the zainod
        // defaults, so the two `no_tls` ones are repeated here.
        let image = dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = [
                "no_tls_use_unencrypted_traffic",
                "allow_unencrypted_public_json_rpc_bind",
                "prometheus",
                "fast-test-seam",
            ]
        );
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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
        let unfinalised_txids = irpc
            .call_value(
                "getaddresstxids",
                serde_json::json!([{ "addresses": [&taddr], "start": height, "end": height }]),
            )
            .await?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();

        // The load-bearing advance: push the send below the seam so it crosses
        // the finalised floor (`tip - seam`) into the finalized DB.
        let tip = validator.generate_blocks(SEAM_ADVANCE).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        // Without this the test is vacuous: it would compare two reads that
        // both came from the non-finalised cache. `index_frontier` is the
        // only observable that says the finalised writer committed the
        // send's block — a served height proves nothing, because below the
        // seam zaino can answer straight from the validator it proxies.
        wait_for_finalised(&indexer, height, SEAM_TIMEOUT).await?;

        let finalised_txids = irpc
            .call_value(
                "getaddresstxids",
                serde_json::json!([{ "addresses": [&taddr], "start": height, "end": height }]),
            )
            .await?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();

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

    /// Smoke: the indexer serves `get_transaction` for the mined orchard
    /// send by txid.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mined(#[case] backend: Backend) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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

        let served = indexer.get_transaction(txid).await?;
        // A mined transaction reports its block height, not the mempool sentinel 0.
        assert_eq!(served.height, u64::from(u32::from(tip)));
        let raw = validator
            .json_rpc()
            .await?
            .call_value(
                "getrawtransaction",
                serde_json::json!([txid.to_string(), 0]),
            )
            .await?;
        assert_eq!(
            zaino_testutils::hex::encode(&served.data),
            raw.as_str()
                .context("getrawtransaction returns a hex string")?,
            "zaino must serve the bytes the validator holds"
        );
        Ok(())
    }

    /// The indexer's `getrawmempool` holds exactly the transactions the
    /// validator's does. The txid set order is not contractual, so both
    /// sides are sorted; the two broadcast txids keep `[] == []` from
    /// passing.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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
            .expect("send returns a txid");
        let u_txid = faucet
            .send(&ua, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");

        let vrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let want = [t_txid.to_string(), u_txid.to_string()];
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        assert_eq!(
            mempool.len(),
            want.len(),
            "the mirror must hold the two broadcast txs and nothing else: {mempool:?}"
        );
        Ok(())
    }

    /// `get_mempool_tx` returns the two unmined transactions, and the
    /// exclude-by-txid-suffix filter drops one.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_tx() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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

        // The validator fixes what the mempool holds before GetMempoolTx is
        // asked; without it a count assertion only says zaino agrees with itself.
        let want = [t_txid.to_string(), u_txid.to_string()];
        let vrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        assert_eq!(
            mempool.len(),
            2,
            "the validator must hold exactly the two broadcast txs: {mempool:?}"
        );

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

    /// The stream serves exactly the transaction bytes the validator holds
    /// unmined.
    ///
    /// GetMempoolStream is bound to the chain tip the request was admitted
    /// against and ends when that tip moves, so the drain can only complete
    /// once a block is mined — hence the spawn-then-mine shape. The
    /// per-transaction bytes come from the validator's `getrawtransaction`,
    /// so this catches a mirror that is missing, over-full, or serving the
    /// wrong bytes; `!is_empty()` alone caught none of those.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_stream() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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
            .expect("send returns a txid");
        let u_txid = faucet
            .send(&ua, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");

        let vrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let want = [t_txid.to_string(), u_txid.to_string()];
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        // The validator's own bytes for each mirrored txid: the independent oracle for
        // what zaino streams.
        let mut expected = Vec::with_capacity(mempool.len());
        for txid in &mempool {
            expected.push(
                vrpc.call_value("getrawtransaction", json!([txid]))
                    .await?
                    .as_str()
                    .with_context(|| format!("getrawtransaction {txid} returns hex"))?
                    .to_string(),
            );
        }
        expected.sort();

        let drain = tokio::spawn({
            let indexer = indexer.clone();
            async move { indexer.get_mempool_stream().await }
        });
        // Mining before the subscription is admitted leaves the drain waiting
        // on the *next* tip change, by which point the mempool is empty.
        // Nothing observable reports that the stream is open, so this is a
        // grace period rather than a handshake. The missing seam is a
        // stream-established signal, e.g.
        //   let (ready, drain) = indexer.get_mempool_stream_handle().await?;
        //   ready.await?; // resolves once zaino has admitted the subscription
        tokio::time::sleep(Duration::from_secs(5)).await;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let txs = drain.await.expect("mempool-stream drain task joins")?;
        let mut streamed: Vec<String> = txs
            .iter()
            .map(|tx| zaino_testutils::hex::encode(&tx.data))
            .collect();
        streamed.sort();
        assert_eq!(
            streamed, expected,
            "the mempool stream must serve the validator's unmined transactions verbatim"
        );
        assert!(
            txs.iter().all(|tx| tx.height == 0),
            "unmined transactions carry the height-0 mempool sentinel"
        );
        Ok(())
    }

    /// `getmempoolinfo` agrees with the **validator's own** `getmempoolinfo`.
    ///
    /// `size` and `bytes` are the same quantities on both sides (tx count,
    /// Σ serialized sizes), so they are compared directly — that is the
    /// cross-check that zaino's mirror holds the transactions the validator
    /// does, rather than that zaino agrees with itself. `usage` is not:
    /// zaino reports the ZIP-401 cost total, each transaction floored at
    /// `max(size, 10_000)` because that is the figure its own memory bound
    /// is enforced against, while zebra reports its own memory estimate.
    /// The floor makes `usage >= bytes` the only contractual relation.
    #[rstest]
    #[case::fetch(Validator::zebrad("6.2.3"), Backend::Fetch)]
    #[case::state(Validator::zebrad("6.2.3"), Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_mempool_info<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
        #[case] backend: Backend,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = validator.regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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
            .expect("send returns a txid");
        let u_txid = faucet
            .send(&ua, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");

        let vrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        let want = [t_txid.to_string(), u_txid.to_string()];
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };

        let validator_info = vrpc.call_value("getmempoolinfo", json!([])).await?;
        let indexer_info = irpc.call_value("getmempoolinfo", json!([])).await?;
        // No block is mined and no send is in flight here, but re-reading the
        // set proves the totals were taken over one quiescent mempool.
        anyhow::ensure!(
            {
                let mut txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                txids.sort();
                txids
            } == mempool,
            "the mempool moved across the two getmempoolinfo reads; totals are \
             not comparable"
        );

        let size = indexer_info
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| {
                format!("getmempoolinfo must report a numeric `size`: {indexer_info}")
            })?;
        let bytes = indexer_info
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| {
                format!("getmempoolinfo must report a numeric `bytes`: {indexer_info}")
            })?;
        let usage = indexer_info
            .get("usage")
            .and_then(serde_json::Value::as_u64)
            .with_context(|| {
                format!("getmempoolinfo must report a numeric `usage`: {indexer_info}")
            })?;

        assert_eq!(
            size,
            mempool.len() as u64,
            "size must equal the mirrored txid count"
        );
        assert_eq!(
            size,
            validator_info
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .with_context(|| format!(
                    "getmempoolinfo must report a numeric `size`: {validator_info}"
                ))?,
            "zaino's mempool holds a different number of transactions than the \
             validator's"
        );
        assert_eq!(
            bytes,
            validator_info
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .with_context(|| format!(
                    "getmempoolinfo must report a numeric `bytes`: {validator_info}"
                ))?,
            "zaino's serialized-byte total disagrees with the validator's"
        );
        assert!(bytes > 0, "two unmined transactions cannot weigh nothing");
        assert!(
            usage >= bytes,
            "usage must be at least bytes: {indexer_info}"
        );
        Ok(())
    }

    /// Broadcast two unmined sends, observe them in zaino's mirror of the
    /// validator's mempool, then mine them in and confirm the balances. The
    /// *unconfirmed* (mempool) pool-balance split under test cannot be
    /// asserted — ztest's librustzcash wallet exposes no pending/unconfirmed
    /// pool-balance accessor — so the confirmed balances stand in.
    #[rstest]
    #[case::fetch(Backend::Fetch)]
    #[case::state(Backend::State)]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn monitor_unverified_mempool(#[case] backend: Backend) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let base = Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest());
        let image = dev!(Indexer::Zainod, "../../Dockerfile");
        let (validator, indexer) = match backend {
            Backend::Fetch => (env.add_validator(base), env.add_indexer(image.regtest())),
            Backend::State => {
                let vol = env.shared_volume("zebra-db");
                (
                    env.add_validator(base.mount(&vol)),
                    env.add_indexer(
                        image
                            .regtest()
                            .tuning(ZainoTuning::State)
                            .mount(&vol)
                            .named("zaino-state"),
                    ),
                )
            }
        };
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

        let want = [ua_txid.to_string(), sapling_txid.to_string()];
        let vrpc = validator.json_rpc().await?;
        let irpc = indexer.json_rpc().await?;
        // Zaino's mempool is a polled mirror (500 ms cadence), so the two agree only
        // eventually; `want` is the non-vacuity probe — a send that was built but never
        // relayed leaves both sides empty and equal.
        let mempool = {
            let deadline = tokio::time::Instant::now() + READY;
            loop {
                let mut validator_txids = vrpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                validator_txids.sort();
                let mut indexer_txids = irpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                indexer_txids.sort();
                if validator_txids == indexer_txids
                    && want.iter().all(|txid| validator_txids.contains(txid))
                {
                    break validator_txids;
                }
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "indexer mempool {indexer_txids:?} never converged on validator \
                     mempool {validator_txids:?} holding {want:?}"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        assert_eq!(
            mempool.len(),
            2,
            "the mirror must hold the two broadcast txs and nothing else: {mempool:?}"
        );

        // `PoolBalances` is confirmed-only, so an unmined send must credit
        // nothing however visible it is in the mempool mirror above.
        recipient.sync().await?;
        let unconfirmed = recipient.balances().await?;
        assert_eq!(
            unconfirmed.get(Pool::Ironwood.ztest()),
            0,
            "an unmined send must not count toward a confirmed balance"
        );
        assert_eq!(unconfirmed.get(Pool::Sapling.ztest()), 0);

        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        recipient.sync().await?;
        let balances = recipient.balances().await?;
        assert_eq!(balances.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
        assert_eq!(balances.get(Pool::Sapling.ztest()), SEND_AMOUNT);
        Ok(())
    }

    /// `getaddresstxids` over the recipient's taddr returns the send's txid
    /// (asserted as the first txid returned), identically to the validator's own.
    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids<B: ValidatorConfig>(#[case] validator: Validator<B>) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest().mine_to(FUND.ztest()));
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

        let height = u32::from(indexer.latest_block_height().await?);
        let params = json!([{
            "addresses": [taddr],
            "start": height.saturating_sub(2),
            "end": height,
        }]);
        let validator_txids = validator
            .json_rpc()
            .await?
            .call_value("getaddresstxids", params.clone())
            .await?;
        let indexer_txids = indexer
            .json_rpc()
            .await?
            .call_value("getaddresstxids", params)
            .await?;
        assert_eq!(
            validator_txids
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str),
            Some(txid.to_string().as_str()),
            "getaddresstxids first txid must be the send {txid}, got {validator_txids}"
        );
        assert_eq!(validator_txids, indexer_txids);
        Ok(())
    }

    /// `getaddressutxos` over the recipient's taddr returns a utxo whose txid is
    /// the send's, over both the gRPC and the JSON-RPC surface, and the JSON-RPC
    /// one matches the validator's own.
    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos<B: ValidatorConfig>(#[case] validator: Validator<B>) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest().mine_to(FUND.ztest()));
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
            .get_address_utxos(vec![taddr.clone()], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(
            utxos[0].txid,
            txid.as_ref().to_vec(),
            "utxo[0] txid must be the send"
        );

        let params = json!([{"addresses": [taddr]}]);
        let validator_utxos = validator
            .json_rpc()
            .await?
            .call_value("getaddressutxos", params.clone())
            .await?;
        let indexer_utxos = indexer
            .json_rpc()
            .await?
            .call_value("getaddressutxos", params)
            .await?;
        let validator_txid = validator_utxos
            .as_array()
            .and_then(|a| a.first())
            .and_then(|u| u.get("txid"))
            .and_then(Value::as_str)
            .with_context(|| format!("validator utxo[0].txid: {validator_utxos}"))?;
        let indexer_txid = indexer_utxos
            .as_array()
            .and_then(|a| a.first())
            .and_then(|u| u.get("txid"))
            .and_then(Value::as_str)
            .with_context(|| format!("indexer utxo[0].txid: {indexer_utxos}"))?;
        assert_eq!(validator_txid, txid.to_string());
        assert_eq!(validator_txid, indexer_txid);
        Ok(())
    }

    /// The tree state at the tip carries the validator's best block hash and a
    /// non-empty tree per shielded protocol, and matches the validator's
    /// `z_gettreestate` field for field.
    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate<B: ValidatorConfig>(#[case] validator: Validator<B>) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest().mine_to(FUND.ztest()));
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
        let tree = indexer.get_tree_state(tip).await?;
        assert_eq!(tree.height, u64::from(u32::from(tip)));
        let best = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", json!([]))
            .await?;
        assert_eq!(
            tree.hash.as_str(),
            best.as_str()
                .context("getbestblockhash returns a hex string")?
        );
        assert!(!tree.sapling_tree.is_empty(), "sapling tree must be served");
        assert!(!tree.orchard_tree.is_empty(), "orchard tree must be served");

        assert_rpc_parity(
            "z_gettreestate",
            &format!(r#"["{}"]"#, u32::from(tip)),
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        Ok(())
    }

    /// No regtest chain completes a subtree, so the roots are empty — and empty
    /// identically to the validator's own `z_getsubtreesbyindex`.
    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest().mine_to(FUND.ztest()));
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

        // A subtree completes every 2^16 notes, which no regtest chain reaches;
        // a synthesised root would fail this.
        let roots = indexer
            .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
            .await?;
        assert!(roots.is_empty(), "regtest completes no subtree: {roots:?}");

        assert_rpc_parity(
            "z_getsubtreesbyindex",
            r#"["orchard", 0]"#,
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        Ok(())
    }

    /// `getrawtransaction` for the shielded send's txid reports the confirming
    /// height, identically to the validator's own answer.
    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest().mine_to(FUND.ztest()));
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

        let served = assert_rpc_parity(
            "getrawtransaction",
            &format!(r#"["{txid}", 1]"#),
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        assert_eq!(
            served.get("txid").and_then(Value::as_str),
            Some(txid.to_string().as_str())
        );
        assert_eq!(
            served.get("height").and_then(Value::as_u64),
            Some(u64::from(u32::from(tip))),
            "a mined transaction reports its block height"
        );
        Ok(())
    }

    /// Smoke: `get_taddress_txids` over the recipient's taddr and a range
    /// around the send succeeds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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

        let tip = indexer.latest_block_height().await?;
        let start = BlockHeight::from(u32::from(tip).saturating_sub(2));
        let txs = indexer.get_taddress_txids(taddr, start, tip).await?;
        assert_eq!(txs.len(), 1, "the span holds exactly the one send");
        assert_eq!(
            zaino_testutils::hex::encode(&txs[0].data),
            validator
                .json_rpc()
                .await?
                .call_value(
                    "getrawtransaction",
                    serde_json::json!([txid.to_string(), 0])
                )
                .await?
                .as_str()
                .context("getrawtransaction returns a hex string")?
        );
        Ok(())
    }

    /// Smoke: `get_address_utxos` over the recipient's taddr succeeds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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

        let utxos = indexer
            .get_address_utxos(vec![taddr.clone()], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(utxos.len(), 1, "the send leaves exactly one utxo");
        assert_eq!(utxos[0].address, taddr);
        assert_eq!(utxos[0].value_zat, SEND_AMOUNT as i64);
        assert_eq!(utxos[0].height, u64::from(u32::from(tip)));
        Ok(())
    }

    /// Smoke: `get_address_utxos_stream` over the recipient's taddr
    /// succeeds.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_utxos_stream() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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

        let streamed = indexer
            .get_address_utxos_stream(vec![taddr.clone()], BlockHeight::from(0u32), 0)
            .await?;
        assert_eq!(streamed.len(), 1, "the send leaves exactly one utxo");
        assert_eq!(streamed[0].address, taddr);
        assert_eq!(streamed[0].value_zat, SEND_AMOUNT as i64);
        // The stream and unary forms serve the same set.
        assert_eq!(
            streamed,
            indexer
                .get_address_utxos(vec![taddr], BlockHeight::from(0u32), 0)
                .await?
        );
        Ok(())
    }

    /// `get_transaction` over an unmined orchard send, then over the same
    /// transaction once mined — the mempool-to-mined transition, end to end.
    ///
    /// The invariants, in order:
    /// - an unmined transaction carries the mempool height sentinel (`0`),
    ///   not the current tip height. This is the whole point of the test:
    ///   returning the tip would make an unconfirmed transaction look
    ///   confirmed to every wallet on the other end of the wire.
    /// - the bytes zaino serves are the transaction that was actually
    ///   broadcast. The validator's `getrawtransaction` is the oracle —
    ///   comparing against a hash zaino also computed would only prove
    ///   zaino agrees with itself.
    /// - once mined, the same query reports the real confirmation height,
    ///   not the sentinel.
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_transaction_mempool() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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

        let unmined = indexer.get_transaction(txid).await?;
        assert_eq!(
            unmined.height, 0,
            "an unmined transaction must carry the mempool height sentinel, \
             not the tip height"
        );

        // The validator is the independent oracle for what was broadcast.
        let vrpc = validator.json_rpc().await?;
        let raw_hex = vrpc
            .call_value("getrawtransaction", json!([txid.to_string()]))
            .await?;
        let raw_hex = raw_hex
            .as_str()
            .context("getrawtransaction must return a hex string at verbosity 0")?;
        assert_eq!(
            zaino_testutils::hex::encode(&unmined.data),
            raw_hex,
            "get_transaction served different bytes than the validator holds \
             for {txid}"
        );

        // Mine it: the same query must flip off the sentinel onto the real
        // confirmation height.
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;

        let mined = indexer.get_transaction(txid).await?;
        assert_eq!(
            mined.height,
            u64::from(u32::from(tip)),
            "a mined transaction must report the height of the block that \
             confirmed it, not the mempool sentinel"
        );
        assert_eq!(
            mined.data, unmined.data,
            "mining must not change the transaction bytes zaino serves"
        );
        Ok(())
    }

    /// `getaddressbalance` over the recipient's taddr reports exactly 250_000,
    /// identically to the validator's own answer.
    #[rstest]
    #[case::zebra(Validator::zebrad("6.2.3"))]
    #[ztest::qos::wallet]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance<B: ValidatorConfig>(
        #[case] validator: Validator<B>,
    ) -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator = env.add_validator(validator.regtest().mine_to(FUND.ztest()));
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

        let balance = assert_rpc_parity(
            "getaddressbalance",
            &format!(r#"[{{"addresses": ["{taddr}"]}}]"#),
            &validator.json_rpc().await?,
            &indexer.json_rpc().await?,
            &[],
        )
        .await?;
        assert_eq!(
            balance.get("balance").and_then(Value::as_u64),
            Some(SEND_AMOUNT),
            "getaddressbalance must report the send amount: {balance}"
        );
        Ok(())
    }

    /// `GetTaddressBalance` over the recipient's taddr reports 250_000.
    #[ztest::qos::integration]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let validator =
            env.add_validator(Validator::zebrad("6.2.3").regtest().mine_to(FUND.ztest()));
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

/// One-chain fetch-vs-state comparisons: each test stands up one zebrad on a shared
/// volume, a fetch zainod (`regtest`) and a state zainod (`tuning(ZainoTuning::State)`)
/// inline, and asserts `fetch == state` over the pods' gRPC / JSON-RPC surface. zebrad
/// only — the state pod reads the validator's own zebra db.
mod zebrad {
    use super::*;

    /// `get_block_range` with no pools == requesting the shielded pools,
    /// fetch==state, and the tip block holds the shielded coinbase + the send
    /// with no transparent data.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_default_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes. This must equal `PoolTypeFilter::default()` — every
        // shielded pool, ironwood included — or the default-vs-explicit checks below compare
        // two different filters.
        let shielded_pools = vec![2, 3, 4];
        let start = BlockHeight::from(1u32);

        let fetch_default = fetch.get_block_range(start, end).await?;
        let fetch_shielded = fetch
            .get_block_range_with_pools(start, end, shielded_pools.clone())
            .await?;
        assert_eq!(fetch_default, fetch_shielded);

        let state_shielded = state
            .get_block_range_with_pools(start, end, shielded_pools.clone())
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

    /// With all pools requested the fetch and state indexers agree, and the tip
    /// block carries the coinbase plus all three sends with their pool data.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn block_range_returns_all_pools() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let start = BlockHeight::from(1u32);

        let fetch_range = fetch
            .get_block_range_with_pools(start, end, all_pools.clone())
            .await?;
        let state_range = state
            .get_block_range_with_pools(start, end, all_pools.clone())
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

    /// The fetch and state indexers agree on the tree state at the tip, and it is the tip's:
    /// height and block hash pin the reply to the block the validator calls the tip, which a
    /// default-valued or stale `TreeState` cannot satisfy.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_treestate_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state pod must answer and the fetch pod must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let fetch_tree = fetch.get_tree_state(tip).await?;
        assert_eq!(
            fetch_tree.height,
            u64::from(u32::from(tip)),
            "the tree state must be the tip's"
        );
        assert_eq!(
            fetch_tree.hash, tip_hash,
            "the tree state must name the block the validator calls the tip"
        );
        assert_eq!(fetch_tree, state.get_tree_state(tip).await?);
        Ok(())
    }

    /// Both indexers report no orchard subtree root. Zebra tracks roots at
    /// `TRACKED_SUBTREE_HEIGHT = 16`, so the first one needs 2^16 note commitments and no
    /// regtest fixture reaches a boundary — the emptiness is the assertion, and a synthesised
    /// root would fail it. Coverage of a real boundary belongs in a `packages/zaino-state`
    /// unit test over the mock chain source.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn z_get_subtrees_by_index_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state pod must answer and the fetch pod must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let fetch_roots = fetch
            .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
            .await?;
        assert!(
            fetch_roots.is_empty(),
            "a regtest chain completes no 2^16-note subtree, so no root can exist: \
             {fetch_roots:?}"
        );
        assert_eq!(
            fetch_roots,
            state
                .get_subtree_roots(0, ShieldedProtocol::Orchard, 0)
                .await?,
        );
        Ok(())
    }

    /// The fetch and state indexers serve the same whole `getrawtransaction` reply for the
    /// send, and its `hex` is the transaction the validator holds — the validator is the
    /// oracle, so a reply both indexers derived the same wrong way still fails.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_transaction_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state pod must answer and the fetch pod must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let vrpc = validator.json_rpc().await?;
        let raw_hex = vrpc
            .call_value("getrawtransaction", json!([txid.to_string()]))
            .await?;
        let raw_hex = raw_hex
            .as_str()
            .context("getrawtransaction must return a hex string at verbosity 0")?;

        let params = serde_json::json!([txid.to_string(), 1]);
        let fetch_tx = fetch
            .json_rpc()
            .await?
            .call_value("getrawtransaction", params.clone())
            .await?;
        assert_eq!(
            fetch_tx.get("hex").and_then(serde_json::Value::as_str),
            Some(raw_hex),
            "zaino served different bytes than the validator holds for {txid}"
        );
        assert_eq!(
            fetch_tx,
            state
                .json_rpc()
                .await?
                .call_value("getrawtransaction", params)
                .await?,
        );
        Ok(())
    }

    /// `getaddresstxids` over the recipient's taddr returns exactly the send, on both ingest
    /// paths. The expected list is the txid the wallet reported before either indexer was
    /// asked, so an empty or over-full answer cannot pass.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_tx_ids_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state pod must answer and the fetch pod must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let height = u32::from(tip);
        let fetch_txids = fetch
            .json_rpc()
            .await?
            .call_value(
                "getaddresstxids",
                serde_json::json!([{ "addresses": [&taddr], "start": height, "end": height }]),
            )
            .await?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        assert_eq!(
            fetch_txids,
            vec![txid.to_string()],
            "the send is the only transaction touching the recipient taddr at {height}"
        );
        assert_eq!(
            fetch_txids,
            state
                .json_rpc()
                .await?
                .call_value(
                    "getaddresstxids",
                    serde_json::json!([{ "addresses": [&taddr], "start": height, "end": height }]),
                )
                .await?
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        );
        Ok(())
    }

    /// `getaddressutxos` over the recipient's taddr returns exactly the send's output, on
    /// both ingest paths, over both the JSON-RPC and gRPC surfaces. Whole replies are
    /// compared: `txid` alone would pass on a wrong value, height or script.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state pod must answer and the fetch pod must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let params = serde_json::json!([{ "addresses": [&taddr] }]);
        let fetch_json = fetch
            .json_rpc()
            .await?
            .call_value("getaddressutxos", params.clone())
            .await?;
        assert_eq!(
            fetch_json.as_array().map(Vec::len),
            Some(1),
            "the send leaves exactly one utxo at the recipient taddr: {fetch_json}"
        );
        assert_eq!(
            fetch_json
                .pointer("/0/satoshis")
                .and_then(serde_json::Value::as_u64),
            Some(SEND_AMOUNT),
            "the utxo must carry the send amount: {fetch_json}"
        );
        assert_eq!(
            fetch_json,
            state
                .json_rpc()
                .await?
                .call_value("getaddressutxos", params)
                .await?,
        );

        let zero = BlockHeight::from(0u32);
        let fetch_utxos = fetch
            .get_address_utxos(vec![taddr.clone()], zero, 0)
            .await?;
        assert_eq!(fetch_utxos.len(), 1);
        assert_eq!(
            fetch_utxos[0].txid,
            txid.as_ref().to_vec(),
            "the only utxo must be the send's output"
        );
        assert_eq!(
            fetch_utxos,
            state.get_address_utxos(vec![taddr], zero, 0).await?
        );
        Ok(())
    }

    /// Both indexers mirror the *validator's* mempool while two sends sit unmined.
    ///
    /// Not an ingest-path comparison: the mempool has no read-state implementation at all
    /// (`GetMempoolTxids` is JSON-RPC-only on both backends), so the two pods would agree by
    /// construction. The validator is the oracle instead, and the two broadcast txids keep
    /// `[] == []` from passing.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_raw_mempool_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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
        let t_txid = faucet
            .send(&taddr, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");
        let u_txid = faucet
            .send(&ua, SEND_AMOUNT)
            .await?
            .into_iter()
            .next()
            .expect("send returns a txid");

        let vrpc = validator.json_rpc().await?;
        let want = [t_txid.to_string(), u_txid.to_string()];
        let frpc = fetch.json_rpc().await?;
        let srpc = state.json_rpc().await?;
        // Zaino's mempool is a polled mirror (500 ms cadence), so each pod agrees with the
        // validator only eventually; `want` is the non-vacuity probe — a send that was built
        // but never relayed leaves every side empty and equal.
        let deadline = tokio::time::Instant::now() + READY;
        let (fetch_mempool, state_mempool) = loop {
            let mut sets = Vec::new();
            for rpc in [&vrpc, &frpc, &srpc] {
                let mut txids = rpc
                    .call_value("getrawmempool", serde_json::json!([]))
                    .await?
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                    .unwrap_or_default();
                txids.sort();
                sets.push(txids);
            }
            if sets[0] == sets[1] && sets[0] == sets[2] && want.iter().all(|t| sets[0].contains(t))
            {
                break (sets[1].clone(), sets[2].clone());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the indexer mempools never converged on the validator's {sets:?} holding \
                 {want:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        assert_eq!(
            fetch_mempool.len(),
            want.len(),
            "the mirror must hold the two broadcast txs and nothing else: {fetch_mempool:?}"
        );
        assert_eq!(fetch_mempool, state_mempool);
        Ok(())
    }

    /// `GetTaddressTxids` over the recipient's taddr returns exactly the send's transaction,
    /// on both ingest paths. The reply carries whole raw transactions, so comparing it
    /// covers the bytes and not merely the count.
    #[ztest::qos::wallet(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_transactions_regtest() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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
            .expect("send returns a txid");
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one.
        // The state pod must answer and the fetch pod must not, or both are reaching the
        // one validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let raw_hex = validator
            .json_rpc()
            .await?
            .call_value("getrawtransaction", json!([txid.to_string()]))
            .await?;
        let raw_hex = raw_hex
            .as_str()
            .context("getrawtransaction must return a hex string at verbosity 0")?;

        let fetch_txs = fetch.get_taddress_txids(taddr.clone(), tip, tip).await?;
        assert_eq!(
            fetch_txs.len(),
            1,
            "only the send pays the recipient taddr in block {tip:?}"
        );
        assert_eq!(
            zaino_testutils::hex::encode(&fetch_txs[0].data),
            raw_hex,
            "the served transaction must be the one the validator holds"
        );
        assert_eq!(fetch_txs, state.get_taddress_txids(taddr, tip, tip).await?);
        Ok(())
    }

    /// With transparent mining every compact-block tx carries a transparent vout, so each
    /// vout's `script_pub_key` is non-empty — and both ingest paths build the same range.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn transparent_data_in_compact_block() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        // Zaino cannot serve the non-standard genesis coinbase script in compact blocks, so
        // this starts at height 1, not 0 (zingolabs/zaino#818).
        let start = BlockHeight::from(1u32);
        let state_range = state
            .get_block_range_with_pools(start, chain_height, all_pools.clone())
            .await?;
        assert_eq!(
            state_range.len(),
            u32::from(chain_height) as usize,
            "the range must serve every height in [1, {chain_height:?}]"
        );
        assert_eq!(
            state_range,
            fetch
                .get_block_range_with_pools(start, chain_height, all_pools)
                .await?
        );
        for cb in state_range {
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

    /// The fetch and state indexers agree on `getaddresstxids` over the faucet's coinbase
    /// taddr. Under `mine_to(Transparent)` the faucet account is the miner address, so its
    /// taddr holds one coinbase per block: the txid count must equal the height span, which
    /// an empty or truncated answer cannot satisfy.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_txids_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let fetch_txids = fetch
            .json_rpc()
            .await?
            .call_value(
                "getaddresstxids",
                serde_json::json!([{ "addresses": [&faucet_taddr], "start": 2, "end": 5 }]),
            )
            .await?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        let state_txids = state
            .json_rpc()
            .await?
            .call_value(
                "getaddresstxids",
                serde_json::json!([{ "addresses": [&faucet_taddr], "start": 2, "end": 5 }]),
            )
            .await?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        assert_eq!(
            fetch_txids.len(),
            4,
            "every block in [2, 5] pays one coinbase to the miner taddr: {fetch_txids:?}"
        );
        assert_eq!(fetch_txids, state_txids);
        Ok(())
    }

    /// The fetch and state indexers agree on the transparent balance of the faucet's
    /// coinbase taddr, and that balance is the sum of the utxos each reports for the same
    /// address — a cross-check no zero-valued or truncated answer survives.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_taddress_balance_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let utxos = fetch
            .json_rpc()
            .await?
            .call_value(
                "getaddressutxos",
                serde_json::json!([{ "addresses": [&faucet_taddr] }]),
            )
            .await?;
        let utxo_total: i64 = utxos
            .as_array()
            .context("getaddressutxos must return an array")?
            .iter()
            .filter_map(|u| u.get("satoshis").and_then(serde_json::Value::as_i64))
            .sum();
        assert!(utxo_total > 0, "faucet taddr must hold coinbase value");

        let fetch_bal = fetch
            .get_taddress_balance(vec![faucet_taddr.clone()])
            .await?;
        let state_bal = state.get_taddress_balance(vec![faucet_taddr]).await?;
        assert_eq!(
            i64::from(fetch_bal),
            utxo_total,
            "the balance must be the sum of the utxos reported for the same address"
        );
        assert_eq!(i64::from(fetch_bal), i64::from(state_bal));
        Ok(())
    }

    /// The fetch and state indexers agree on `GetAddressUtxos` over the faucet's coinbase
    /// taddr. `max_entries = 3` over a chain that pays the miner in every block must return
    /// exactly three utxos, so a truncated or empty reply cannot pass.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let start = BlockHeight::from(2u32);
        let fetch_utxos = fetch
            .get_address_utxos(vec![faucet_taddr.clone()], start, 3)
            .await?;
        assert_eq!(
            fetch_utxos.len(),
            3,
            "every block from {start:?} pays the miner taddr, so `max_entries` binds"
        );
        assert_eq!(
            fetch_utxos,
            state
                .get_address_utxos(vec![faucet_taddr], start, 3)
                .await?
        );
        Ok(())
    }

    /// The streamed utxos agree between the fetch and state indexers over the faucet's
    /// coinbase taddr, and with the unary reply over the same arguments.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_utxos_stream_faucet_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        let start = BlockHeight::from(2u32);
        let fetch_utxos = fetch
            .get_address_utxos_stream(vec![faucet_taddr.clone()], start, 3)
            .await?;
        assert_eq!(
            fetch_utxos.len(),
            3,
            "every block from {start:?} pays the miner taddr, so `max_entries` binds"
        );
        assert_eq!(
            fetch_utxos,
            fetch
                .get_address_utxos(vec![faucet_taddr.clone()], start, 3)
                .await?,
            "the streamed and unary replies must agree over the same arguments"
        );
        assert_eq!(
            fetch_utxos,
            state
                .get_address_utxos_stream(vec![faucet_taddr], start, 3)
                .await?
        );
        Ok(())
    }

    /// The five `getaddressdeltas` request shapes: the bare-address form, the
    /// multi-address union, the `chainInfo` form's named endpoints, an `end` past the
    /// tip clamping down to it, and an address with no history answering empty.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn address_deltas() -> Result<()> {
        // Valid-format regtest taddr that nothing on this chain ever pays.
        const UNKNOWN_TADDR: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";

        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // Transparent coinbase: `funded_faucet_with_notes` matures it and shields it into
        // Orchard, so the faucet taddr carries both the coinbase credits and the shield's
        // debits by the time the send credits the recipient taddr.
        let faucet = wallet
            .funded_faucet_with_notes(&validator, &fetch, 1)
            .await?;
        let recipient = wallet.recipient(&validator, &fetch).await?;
        let faucet_taddr = faucet.address(Pool::Transparent.ztest()).await?;
        let recipient_taddr = recipient.address(Pool::Transparent.ztest()).await?;
        faucet.send(&recipient_taddr, SEND_AMOUNT).await?;
        let tip = validator.generate_blocks(1).await?;
        fetch.wait_for_block_num(tip, READY).await?;
        state.wait_for_block_num(tip, READY).await?;
        let tip = u32::from(tip);

        // zebrad implements no `getaddressdeltas`, so only a read-state can answer it. The
        // state pod must answer and the fetch pod must not, or the assertions below are
        // reading the validator over the one transport rather than zaino's read-state.
        let srpc = state.json_rpc().await?;
        let bare = srpc
            .call_value("getaddressdeltas", serde_json::json!([&recipient_taddr]))
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getaddressdeltas", serde_json::json!([&recipient_taddr]))
                .await
                .is_err(),
            "the fetch indexer answered getaddressdeltas, which only a read-state can serve"
        );

        // The bare-address form answers the rangeless whole chain as a plain array.
        let bare = bare
            .as_array()
            .context("the bare-address form answers a delta array")?;
        assert_eq!(
            bare.len(),
            1,
            "coinbase pays the faucet taddr, so the send is the recipient taddr's only delta"
        );
        assert_eq!(bare[0]["height"].as_u64(), Some(u64::from(tip)));
        assert_eq!(
            bare[0]["index"].as_u64(),
            Some(0),
            "the send's sole transparent output"
        );
        assert_eq!(bare[0]["satoshis"].as_u64(), Some(SEND_AMOUNT));
        assert_eq!(bare[0]["address"].as_str(), Some(recipient_taddr.as_str()));

        let both = srpc
            .call_value(
                "getaddressdeltas",
                serde_json::json!([{
                    "addresses": [&faucet_taddr, &recipient_taddr],
                    "start": 0,
                    "end": tip,
                }]),
            )
            .await?;
        let both = both
            .as_array()
            .context("a request without chainInfo answers a delta array")?;
        assert!(
            both.iter()
                .any(|delta| delta["address"].as_str() == Some(faucet_taddr.as_str()))
                && both
                    .iter()
                    .any(|delta| delta["address"].as_str() == Some(recipient_taddr.as_str())),
            "a multi-address request unions every requested address's deltas"
        );

        let named = srpc
            .call_value(
                "getaddressdeltas",
                serde_json::json!([{
                    "addresses": [&faucet_taddr, &recipient_taddr],
                    "start": 1,
                    "end": tip,
                    "chainInfo": true,
                }]),
            )
            .await?;
        assert_eq!(named["start"]["height"].as_u64(), Some(1));
        assert_eq!(named["end"]["height"].as_u64(), Some(u64::from(tip)));
        assert!(!named["deltas"]
            .as_array()
            .context("the chainInfo form wraps its deltas")?
            .is_empty());

        let clamped = srpc
            .call_value(
                "getaddressdeltas",
                serde_json::json!([{
                    "addresses": [&faucet_taddr],
                    "start": 1,
                    "end": tip + 100,
                    "chainInfo": true,
                }]),
            )
            .await?;
        assert_eq!(
            clamped["end"]["height"].as_u64(),
            Some(u64::from(tip)),
            "an end past the tip is clamped to the tip, not rejected"
        );

        let unknown = srpc
            .call_value(
                "getaddressdeltas",
                serde_json::json!([{
                    "addresses": [UNKNOWN_TADDR],
                    "start": 1,
                    "end": tip,
                    "chainInfo": true,
                }]),
            )
            .await?;
        assert_eq!(
            unknown["deltas"],
            serde_json::json!([]),
            "an address with no history answers the chainInfo shape with no deltas"
        );
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
            Validator::zebrad("6.2.3")
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
        let funding_block_hash = irpc
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();

        // The recipient confirms the received output and shields it; the
        // shielding tx spends that output, producing the non-coinbase transparent
        // input under test.
        recipient.sync().await?;
        recipient.shield().await?;
        let tip = validator.generate_blocks(1).await?;
        indexer.wait_for_block_num(tip, READY).await?;
        let spend_block_hash = irpc
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();

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
                d.get("outputs")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|outputs| {
                        outputs.iter().any(|o| {
                            o.get("satoshis").and_then(serde_json::Value::as_i64)
                                == Some(FUNDING_AMOUNT)
                        })
                    })
            })
            .cloned()
            .expect("funding tx paying the recipient should be in its block");
        let funding_output = funding_delta
            .get("outputs")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .find(|o| o.get("satoshis").and_then(serde_json::Value::as_i64) == Some(FUNDING_AMOUNT))
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
            .flat_map(|d| {
                d.get("inputs")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            })
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

    /// A freshly mined block carries only its (shielded) coinbase transaction —
    /// the coinbase input is skipped and `getblockdeltas` fabricates no
    /// transparent input deltas.
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
            Validator::zebrad("6.2.3")
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
        let block_hash = irpc
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();

        let deltas = irpc
            .call_value("getblockdeltas", serde_json::json!([block_hash]))
            .await?;
        let all_empty = deltas
            .get("deltas")
            .and_then(serde_json::Value::as_array)
            .unwrap_or(&Vec::new())
            .iter()
            .all(|d| {
                d.get("inputs")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|inputs| inputs.is_empty())
            });
        assert!(all_empty, "a coinbase-only block must have no input deltas");
        Ok(())
    }

    /// The recipient taddr reports exactly the send on both ingest paths. Whole replies are
    /// compared, so a divergent `received` fails alongside a divergent `balance`.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_address_balance_fetch_vs_state() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

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
            Some(SEND_AMOUNT),
            "getaddressbalance must report the send: {fetch_bal}"
        );
        assert_eq!(fetch_bal, state_bal);
        Ok(())
    }

    /// Draining [1, 106] on a 100-block chain yields the 100 available blocks
    /// (fetch == state) and then errors rather than ending cleanly.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_upper_bound() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let (start, end) = (BlockHeight::from(1u32), BlockHeight::from(106u32));
        let (fetch_blocks, fetch_errored) = fetch
            .drain_block_range(start, end, all_pools.clone())
            .await?;
        let (state_blocks, state_errored) = state
            .drain_block_range(start, end, all_pools.clone())
            .await?;

        assert_eq!(fetch_blocks, state_blocks);
        let compact_block = state_blocks.last().expect("non-empty range");
        assert_eq!(
            compact_block.height, 100,
            "the drain must stop at the tip, not at the requested end"
        );
        assert_eq!(fetch_blocks.len(), 100);
        assert!(state_errored, "state stream should terminate with an error");
        assert!(fetch_errored, "fetch stream should terminate with an error");
        Ok(())
    }

    /// Draining the inverted range [106, 1] yields no blocks (fetch == state,
    /// both empty) and then errors rather than ending cleanly.
    #[ztest::qos::integration(footprint = "3c/6Gi")]
    #[tokio::test(flavor = "multi_thread")]
    async fn get_block_range_out_of_range_lower_bound() -> Result<()> {
        let mut env = TestEnv::builder().ready_timeout(READY);
        let vol = env.shared_volume("zebra-db");
        let validator = env.add_validator(
            Validator::zebrad("6.2.3")
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

        // zebrad implements no `getblockdeltas`, so only a read-state can synthesise one. The
        // state pod must answer and the fetch pod must not, or both are reaching the one
        // validator over the one transport and every comparison below is an identity.
        let tip_hash = validator
            .json_rpc()
            .await?
            .call_value("getbestblockhash", serde_json::json!([]))
            .await?
            .as_str()
            .context("getbestblockhash returns a hash string")?
            .to_string();
        let deltas = serde_json::json!([&tip_hash]);
        state
            .json_rpc()
            .await?
            .call_value("getblockdeltas", deltas.clone())
            .await?;
        anyhow::ensure!(
            fetch
                .json_rpc()
                .await?
                .call_value("getblockdeltas", deltas)
                .await
                .is_err(),
            "the fetch indexer answered getblockdeltas, which only a read-state can \
             synthesise; this pod pair is not comparing two ingest paths"
        );

        // `PoolType` wire codes: transparent=1, sapling=2, orchard=3, ironwood=4.
        let all_pools = vec![1, 2, 3, 4];
        let (start, end) = (BlockHeight::from(106u32), BlockHeight::from(1u32));
        let (fetch_blocks, fetch_errored) = fetch
            .drain_block_range(start, end, all_pools.clone())
            .await?;
        let (state_blocks, state_errored) = state
            .drain_block_range(start, end, all_pools.clone())
            .await?;

        assert_eq!(fetch_blocks, state_blocks);
        assert!(
            fetch_blocks.is_empty(),
            "a descending range starting past the tip serves nothing: {fetch_blocks:?}"
        );
        assert!(state_errored, "state stream should terminate with an error");
        assert!(fetch_errored, "fetch stream should terminate with an error");
        Ok(())
    }
}
