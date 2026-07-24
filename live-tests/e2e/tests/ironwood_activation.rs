//! Wallet-tier predicates across the Orchard→Ironwood activation boundary.
//!
//! Every test here runs a librustzcash wallet on a mid-chain NU6.3 schedule —
//! the hermetic replay of what The Public Testnet did once at height
//! 4,134,000: heights 2 through 5 are Orchard era, [`NU6_3_TRANSITION_BOUNDARY`]
//! (6) onward is Ironwood era. The schedule is pinned in exactly one place, the
//! `TestEnv` builder's [`activation_heights`], and the validator, indexer, and
//! wallet all adopt it (zainod over `getblockchaininfo`, the wallet over the
//! validator's activation heights); height drift between them is
//! unrepresentable.
//!
//! # The predicates, and where each era's cell is covered
//!
//! | predicate (wallet-observable)                | Orchard era | Ironwood era |
//! |----------------------------------------------|-------------|--------------|
//! | unified-address receipt lands in Orchard     | here        | false — `devtool.rs` `send_to_ironwood` asserts the Orchard pool stays empty |
//! | unified-address receipt lands in Ironwood    | false (pool inactive) | `devtool.rs` `send_to_ironwood` |
//! | shielded-receiver coinbase pays the era pool | here (wallet view); wire tier in `compact_block_wire.rs` | `devtool.rs` `receives_mining_reward`; wire tier in `compact_block_wire.rs` |
//! | an Orchard note spends into an Ironwood receipt (ZIP 318 migration) | n/a (nothing to exit) | here |
//! | `shield` deposits into the era pool             | here        | `devtool.rs` `shield_for_validator` |
//! | receipt pool flips between the last Orchard block and the activation block | here (boundary − 1) | here (exactly at the boundary) |
//! | Orchard pool value grows only below the boundary; Ironwood holds value only from it | here (per-height value pools) | here |
//!
//! Era composition of the *served* chain (coinbase routing, compact-block
//! action fields) is covered clientless in
//! `clientless/tests/compact_block_consistency.rs` and over the real gRPC wire
//! in `compact_block_wire.rs`; this file owns the cells that need a wallet on
//! both sides of the boundary.
//!
//! The Public Testnet cannot host the migration cell for us: its pre-NU6.3
//! epoch closed at height 4,134,000, no new value may enter Orchard from there
//! (post-activation Orchard actions permit only same-receiver change or
//! withdrawal — the cross-address restriction,
//! <https://zcash.github.io/ironwood/design/action-circuit.html#the-cross-address-restriction>),
//! and we hold no pre-activation Orchard TAZ — so this hermetic fixture is the
//! only controlled venue for it.
//!
//! ztest port note: dev pinned the schedule in the zebrad launch config and
//! drove a `zcash-devtool` wallet; here the schedule is the `TestEnv` builder's
//! `activation_heights(orchard_then_ironwood_at(6))` (the mid-chain NU6.3
//! transition is now a real ztest capability), the wallet is
//! `Wallet::librustzcash()` (its balance summary tracks the Ironwood pool
//! separately, unlike the zingo backend), and the per-height value-pool reads
//! come off the served `getblockchaininfo` `valuePools` totals.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

use e2e::{assert_pool_absent, assert_pool_present, Pool};

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// Standard transfer amount (zatoshis), matching dev's sends.
const SEND_AMOUNT: u64 = 250_000;
/// zingolib's ZIP-317 fee for a single-note shield round under regtest.
const SHIELD_FEE: u64 = 15_000;
/// The mid-chain NU6.3 (Ironwood) activation height: heights `[2, 6)` are
/// Orchard era, height 6 onward is Ironwood era.
const NU6_3_TRANSITION_BOUNDARY: u32 = 6;

/// Mid-chain transition schedule: NU5..=NU6.2 active from height 2, NU6.3
/// (Ironwood) pinned at `boundary`, so the same orchard-receiver miner and
/// wallet flip pools at `boundary`.
fn orchard_then_ironwood_at(boundary: u32) -> ActivationHeights {
    ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(2))
        .set_nu6(Some(2))
        .set_nu6_1(Some(2))
        .set_nu6_2(Some(2))
        .set_nu6_3(Some(boundary))
        .build()
}

/// The served `chainValueZat` total for `pool_id` in `getblockchaininfo`'s
/// `valuePools` at the indexer's current tip (0 when the pool is absent). This
/// is the served-verbosity value-pool read dev asserted per height.
async fn served_pool_zats(rpc: &JsonRpcClient, pool_id: &str) -> Result<u64> {
    let info = rpc
        .call_value("getblockchaininfo", serde_json::json!([]))
        .await?;
    Ok(info
        .get("valuePools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|pool| pool.get("id").and_then(serde_json::Value::as_str) == Some(pool_id))
        .and_then(|pool| {
            pool.get("chainValueZat")
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0))
}

mod zebrad {
    use super::*;

    mod fetch_service {
        use super::*;

        /// Orchard-era receipt: with the tip still below the boundary, the
        /// faucet's coinbase note is an Orchard note (not Ironwood), and a
        /// unified-address send received before the boundary lands in the
        /// recipient's Orchard pool with the Ironwood pool exactly empty — the
        /// era-mirror of `devtool.rs::send_to_ironwood`.
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn unified_receipt_lands_in_orchard_before_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(Pool::Orchard.ztest()),
            );
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            // Funding warms up to NU5 then mines one Orchard-era block (tip 2),
            // well below the boundary, so the faucet holds an Orchard note.
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let faucet_balance = faucet.balances().await?;
            assert!(
                faucet_balance.get(Pool::Orchard.ztest()) > 0,
                "pre-boundary coinbase should be an orchard note, got {faucet_balance:?}"
            );
            assert_eq!(
                faucet_balance.get(Pool::Ironwood.ztest()),
                0,
                "no ironwood note can exist below the boundary, got {faucet_balance:?}"
            );

            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(balance.get(Pool::Orchard.ztest()), SEND_AMOUNT);
            assert_eq!(balance.get(Pool::Ironwood.ztest()), 0);
            Ok(())
        }

        /// The ZIP 318 migration shape: an Orchard note minted before the
        /// boundary is spent after it, to a unified address, and the receipt
        /// lands in the Ironwood pool with the recipient's Orchard pool exactly
        /// empty. The faucet's Orchard balance must shrink: from the boundary
        /// the cross-address restriction limits each Orchard action to
        /// same-receiver change or withdrawal, so a genuine Orchard spend nets
        /// sent-amount-plus-fee out of the pool. The served per-height
        /// `valuePools` totals witness the Orchard pool growing only below the
        /// boundary and the Ironwood pool taking value only from it.
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn orchard_note_spends_to_ironwood_across_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(Pool::Orchard.ztest()),
            );
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let boundary = NU6_3_TRANSITION_BOUNDARY as usize;
            let migration_height = boundary + 1;
            // Per-height served value-pool totals, indexed by height. Heights 0
            // and 1 are pre-NU5 (no shielded value) and stay zero.
            let mut orchard = vec![0u64; migration_height + 1];
            let mut ironwood = vec![0u64; migration_height + 1];

            // Fund one Orchard note below the boundary (tip 2), then snapshot the
            // funded tip's served value pools.
            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let irpc = indexer.json_rpc().await?;
            orchard[2] = served_pool_zats(&irpc, "orchard").await?;
            ironwood[2] = served_pool_zats(&irpc, "ironwood").await?;

            let pre_boundary_balance = faucet.balances().await?;
            assert!(
                pre_boundary_balance.get(Pool::Orchard.ztest()) > 0,
                "pre-boundary coinbase should be an orchard note, got {pre_boundary_balance:?}"
            );

            // Mine to the boundary one block at a time, recording each height's
            // served value pools. Each pre-boundary coinbase grows Orchard; the
            // activation block's coinbase is the chain's first Ironwood value.
            for height in 3..=boundary {
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
                orchard[height] = served_pool_zats(&irpc, "orchard").await?;
                ironwood[height] = served_pool_zats(&irpc, "ironwood").await?;
            }

            faucet.sync().await?;
            let crossed_balance = faucet.balances().await?;
            let orchard_before_send = crossed_balance.get(Pool::Orchard.ztest());
            assert!(
                crossed_balance.get(Pool::Ironwood.ztest()) > 0,
                "the boundary coinbase should be an ironwood note, got {crossed_balance:?}"
            );

            // The migration send: built at the boundary tip, it spends an
            // Orchard note into a unified-address (Ironwood) receipt.
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            orchard[migration_height] = served_pool_zats(&irpc, "orchard").await?;
            ironwood[migration_height] = served_pool_zats(&irpc, "ironwood").await?;
            faucet.sync().await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(balance.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            assert_eq!(balance.get(Pool::Orchard.ztest()), 0);
            assert!(
                faucet.balances().await?.get(Pool::Orchard.ztest()) < orchard_before_send,
                "the migration send must spend an orchard note"
            );

            // Per-height served value-pool assertions.
            for (height, &value) in ironwood.iter().enumerate().skip(1) {
                if height < boundary {
                    assert_eq!(
                        value, 0,
                        "the ironwood pool must hold no value at height {height}, below the boundary"
                    );
                }
            }
            for height in 2..boundary {
                assert!(
                    orchard[height] > orchard[height - 1],
                    "each pre-boundary coinbase must grow the orchard pool (height {height}: {} after {})",
                    orchard[height],
                    orchard[height - 1]
                );
            }
            assert_eq!(
                orchard[boundary],
                orchard[boundary - 1],
                "the orchard pool must hold exactly steady across the boundary edge"
            );
            assert!(
                ironwood[boundary] > 0,
                "the activation block's coinbase must give the ironwood pool its first value"
            );
            assert!(
                orchard[migration_height] < orchard[boundary],
                "the migration block must shrink the orchard pool ({} at the boundary, {} at the migration block)",
                orchard[boundary],
                orchard[migration_height]
            );
            Ok(())
        }

        /// The receipt pool flips between adjacent blocks: a send confirmed in
        /// the last Orchard-era block (boundary − 1) lands in Orchard, and a send
        /// built one block below the boundary but confirmed in the activation
        /// block itself lands in Ironwood. The second send is itself
        /// migration-shaped: constructed while the tip is still Orchard-era, it
        /// spends an Orchard note (the faucet's only spendable kind at that tip),
        /// so the activation block carries the Orchard spend's data alongside the
        /// Ironwood receipt.
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn receipts_flip_pools_exactly_at_the_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(Pool::Orchard.ztest()),
            );
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;

            // Position the tip at boundary − 2, so the first send (built here)
            // confirms in the last Orchard-era block (boundary − 1).
            let cur = u32::from(validator.chain_height().await?);
            let target = NU6_3_TRANSITION_BOUNDARY - 2;
            if cur < target {
                let tip = validator.generate_blocks(target - cur).await?;
                indexer.wait_for_block_num(tip, READY).await?;
                faucet.sync().await?;
            }

            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            let last_orchard_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?; // boundary − 1
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(
                balance.get(Pool::Orchard.ztest()),
                SEND_AMOUNT,
                "receipt confirmed at boundary - 1 must be orchard, got {balance:?}"
            );
            assert_eq!(
                balance.get(Pool::Ironwood.ztest()),
                0,
                "no ironwood receipt below the boundary, got {balance:?}"
            );

            // The second send is built at boundary − 1 (still Orchard era,
            // spending an Orchard note) but confirmed in the activation block.
            faucet.sync().await?;
            let first_ironwood_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?; // boundary
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(balance.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            assert_eq!(
                balance.get(Pool::Orchard.ztest()),
                SEND_AMOUNT,
                "the pre-boundary receipt must survive the flip unchanged"
            );

            // Served edge-block pool-presence: the last Orchard block and the
            // activation block.
            let blocks = indexer
                .get_block_range(BlockHeight::from(1u32), tip)
                .await?;
            let last_orchard_block = blocks
                .iter()
                .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY - 1))
                .expect("boundary-1 block served");
            let activation_block = blocks
                .iter()
                .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY))
                .expect("boundary block served");
            assert_eq!(
                last_orchard_block.height,
                u64::from(NU6_3_TRANSITION_BOUNDARY - 1)
            );
            assert_eq!(
                activation_block.height,
                u64::from(NU6_3_TRANSITION_BOUNDARY)
            );
            assert_pool_present(last_orchard_block, &last_orchard_txid, Pool::Orchard);
            assert_pool_absent(last_orchard_block, &last_orchard_txid, Pool::Ironwood);
            assert_pool_present(activation_block, &first_ironwood_txid, Pool::Ironwood);
            // The second send is migration-shaped: it spends an Orchard note, so
            // its compact form carries the Orchard spend's data too.
            assert_pool_present(activation_block, &first_ironwood_txid, Pool::Orchard);
            Ok(())
        }

        /// Below the boundary, `shield` deposits into the Orchard pool: the
        /// faucet funds the recipient's transparent address, the recipient
        /// shields, and the shielded balance (net of the ZIP-317 fee, mirroring
        /// `devtool.rs::shield_for_validator`) lands in Orchard with the Ironwood
        /// pool exactly empty — the era-mirror of the Ironwood-era shield cell.
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn shield_deposits_to_orchard_before_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
            let validator = env.add_validator(
                Validator::zebrad("6.2.0")
                    .regtest()
                    .mine_to(Pool::Orchard.ztest()),
            );
            let indexer = env.add_indexer(dev!(Indexer::Zainod, "../../Dockerfile").regtest());
            let wallet = env.add_wallet(Wallet::librustzcash());
            env.build().await?;

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;

            // Confirm the transparent receipt below the boundary (height 4).
            let cur = u32::from(validator.chain_height().await?);
            let target = NU6_3_TRANSITION_BOUNDARY - 2;
            let tip = validator
                .generate_blocks(target.saturating_sub(cur).max(1))
                .await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT
            );

            // The shield is built below the boundary, so it deposits to Orchard.
            recipient.shield().await?;
            let tip = validator.generate_blocks(1).await?; // height 5, still Orchard era
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(
                balance.get(Pool::Orchard.ztest()),
                SEND_AMOUNT - SHIELD_FEE,
                "shielded balance must be the send net of the ZIP-317 fee \
                 (below NU6.3 the shield deposits into the Orchard pool)"
            );
            assert_eq!(balance.get(Pool::Ironwood.ztest()), 0);
            Ok(())
        }
    }

    mod state_service {
        use super::*;

        /// State-backend port of
        /// [`super::fetch_service::unified_receipt_lands_in_orchard_before_boundary`].
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        // TODO: zaino StateService get_commitment_tree_roots reads the Ironwood
        // tree unconditionally once NU6.3 is active, so pre-NU6.3 blocks in a
        // mid-chain transition may stall the syncer; this test will fail until
        // zaino gates that read on height >= nu6_3. Do NOT ignore.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn unified_receipt_lands_in_orchard_before_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            let vol = env.shared_volume("zebra-db");
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
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

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let faucet_balance = faucet.balances().await?;
            assert!(
                faucet_balance.get(Pool::Orchard.ztest()) > 0,
                "pre-boundary coinbase should be an orchard note, got {faucet_balance:?}"
            );
            assert_eq!(
                faucet_balance.get(Pool::Ironwood.ztest()),
                0,
                "no ironwood note can exist below the boundary, got {faucet_balance:?}"
            );

            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(balance.get(Pool::Orchard.ztest()), SEND_AMOUNT);
            assert_eq!(balance.get(Pool::Ironwood.ztest()), 0);
            Ok(())
        }

        /// State-backend port of
        /// [`super::fetch_service::orchard_note_spends_to_ironwood_across_boundary`].
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        // TODO: zaino StateService get_commitment_tree_roots reads the Ironwood
        // tree unconditionally once NU6.3 is active, so pre-NU6.3 blocks in a
        // mid-chain transition may stall the syncer; this test will fail until
        // zaino gates that read on height >= nu6_3. Do NOT ignore.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn orchard_note_spends_to_ironwood_across_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            let vol = env.shared_volume("zebra-db");
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
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

            let boundary = NU6_3_TRANSITION_BOUNDARY as usize;
            let migration_height = boundary + 1;
            let mut orchard = vec![0u64; migration_height + 1];
            let mut ironwood = vec![0u64; migration_height + 1];

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let irpc = indexer.json_rpc().await?;
            orchard[2] = served_pool_zats(&irpc, "orchard").await?;
            ironwood[2] = served_pool_zats(&irpc, "ironwood").await?;

            let pre_boundary_balance = faucet.balances().await?;
            assert!(
                pre_boundary_balance.get(Pool::Orchard.ztest()) > 0,
                "pre-boundary coinbase should be an orchard note, got {pre_boundary_balance:?}"
            );

            for height in 3..=boundary {
                let tip = validator.generate_blocks(1).await?;
                indexer.wait_for_block_num(tip, READY).await?;
                orchard[height] = served_pool_zats(&irpc, "orchard").await?;
                ironwood[height] = served_pool_zats(&irpc, "ironwood").await?;
            }

            faucet.sync().await?;
            let crossed_balance = faucet.balances().await?;
            let orchard_before_send = crossed_balance.get(Pool::Orchard.ztest());
            assert!(
                crossed_balance.get(Pool::Ironwood.ztest()) > 0,
                "the boundary coinbase should be an ironwood note, got {crossed_balance:?}"
            );

            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            faucet.send(&ua, SEND_AMOUNT).await?;
            let tip = validator.generate_blocks(1).await?;
            indexer.wait_for_block_num(tip, READY).await?;
            orchard[migration_height] = served_pool_zats(&irpc, "orchard").await?;
            ironwood[migration_height] = served_pool_zats(&irpc, "ironwood").await?;
            faucet.sync().await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(balance.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            assert_eq!(balance.get(Pool::Orchard.ztest()), 0);
            assert!(
                faucet.balances().await?.get(Pool::Orchard.ztest()) < orchard_before_send,
                "the migration send must spend an orchard note"
            );

            for (height, &value) in ironwood.iter().enumerate().skip(1) {
                if height < boundary {
                    assert_eq!(
                        value, 0,
                        "the ironwood pool must hold no value at height {height}, below the boundary"
                    );
                }
            }
            for height in 2..boundary {
                assert!(
                    orchard[height] > orchard[height - 1],
                    "each pre-boundary coinbase must grow the orchard pool (height {height}: {} after {})",
                    orchard[height],
                    orchard[height - 1]
                );
            }
            assert_eq!(
                orchard[boundary],
                orchard[boundary - 1],
                "the orchard pool must hold exactly steady across the boundary edge"
            );
            assert!(
                ironwood[boundary] > 0,
                "the activation block's coinbase must give the ironwood pool its first value"
            );
            assert!(
                orchard[migration_height] < orchard[boundary],
                "the migration block must shrink the orchard pool ({} at the boundary, {} at the migration block)",
                orchard[boundary],
                orchard[migration_height]
            );
            Ok(())
        }

        /// State-backend port of
        /// [`super::fetch_service::receipts_flip_pools_exactly_at_the_boundary`].
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        // TODO: zaino StateService get_commitment_tree_roots reads the Ironwood
        // tree unconditionally once NU6.3 is active, so pre-NU6.3 blocks in a
        // mid-chain transition may stall the syncer; this test will fail until
        // zaino gates that read on height >= nu6_3. Do NOT ignore.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn receipts_flip_pools_exactly_at_the_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            let vol = env.shared_volume("zebra-db");
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
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

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;

            let cur = u32::from(validator.chain_height().await?);
            let target = NU6_3_TRANSITION_BOUNDARY - 2;
            if cur < target {
                let tip = validator.generate_blocks(target - cur).await?;
                indexer.wait_for_block_num(tip, READY).await?;
                faucet.sync().await?;
            }

            let recipient = wallet.recipient(&validator, &indexer).await?;
            let ua = recipient.address(Pool::Orchard.ztest()).await?;
            let last_orchard_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?; // boundary − 1
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(
                balance.get(Pool::Orchard.ztest()),
                SEND_AMOUNT,
                "receipt confirmed at boundary - 1 must be orchard, got {balance:?}"
            );
            assert_eq!(
                balance.get(Pool::Ironwood.ztest()),
                0,
                "no ironwood receipt below the boundary, got {balance:?}"
            );

            faucet.sync().await?;
            let first_ironwood_txid = faucet
                .send(&ua, SEND_AMOUNT)
                .await?
                .into_iter()
                .next()
                .expect("send returns a txid");
            let tip = validator.generate_blocks(1).await?; // boundary
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(balance.get(Pool::Ironwood.ztest()), SEND_AMOUNT);
            assert_eq!(
                balance.get(Pool::Orchard.ztest()),
                SEND_AMOUNT,
                "the pre-boundary receipt must survive the flip unchanged"
            );

            let blocks = indexer
                .get_block_range(BlockHeight::from(1u32), tip)
                .await?;
            let last_orchard_block = blocks
                .iter()
                .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY - 1))
                .expect("boundary-1 block served");
            let activation_block = blocks
                .iter()
                .find(|b| b.height == u64::from(NU6_3_TRANSITION_BOUNDARY))
                .expect("boundary block served");
            assert_eq!(
                last_orchard_block.height,
                u64::from(NU6_3_TRANSITION_BOUNDARY - 1)
            );
            assert_eq!(
                activation_block.height,
                u64::from(NU6_3_TRANSITION_BOUNDARY)
            );
            assert_pool_present(last_orchard_block, &last_orchard_txid, Pool::Orchard);
            assert_pool_absent(last_orchard_block, &last_orchard_txid, Pool::Ironwood);
            assert_pool_present(activation_block, &first_ironwood_txid, Pool::Ironwood);
            assert_pool_present(activation_block, &first_ironwood_txid, Pool::Orchard);
            Ok(())
        }

        /// State-backend port of
        /// [`super::fetch_service::shield_deposits_to_orchard_before_boundary`].
        ///
        /// multi_thread required: the test manager spawns the validator and
        /// indexer services.
        // TODO: zaino StateService get_commitment_tree_roots reads the Ironwood
        // tree unconditionally once NU6.3 is active, so pre-NU6.3 blocks in a
        // mid-chain transition may stall the syncer; this test will fail until
        // zaino gates that read on height >= nu6_3. Do NOT ignore.
        #[ztest::qos::wallet]
        #[tokio::test(flavor = "multi_thread")]
        async fn shield_deposits_to_orchard_before_boundary() -> Result<()> {
            let mut env = TestEnv::builder()
                .ready_timeout(READY)
                .activation_heights(orchard_then_ironwood_at(NU6_3_TRANSITION_BOUNDARY));
            let vol = env.shared_volume("zebra-db");
            // TODO: published zebrad("6.2.0") may lack the NU6.3 branch id to
            // mine/validate an Ironwood coinbase at the boundary; swap in a
            // NU6.3-capable dev! image if it fails on -25.
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

            let faucet = wallet
                .funded_faucet_with_notes(&validator, &indexer, 1)
                .await?;
            let recipient = wallet.recipient(&validator, &indexer).await?;
            let taddr = recipient.address(Pool::Transparent.ztest()).await?;
            faucet.send(&taddr, SEND_AMOUNT).await?;

            let cur = u32::from(validator.chain_height().await?);
            let target = NU6_3_TRANSITION_BOUNDARY - 2;
            let tip = validator
                .generate_blocks(target.saturating_sub(cur).max(1))
                .await?;
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;
            assert_eq!(
                recipient.balances().await?.get(Pool::Transparent.ztest()),
                SEND_AMOUNT
            );

            recipient.shield().await?;
            let tip = validator.generate_blocks(1).await?; // height 5, still Orchard era
            indexer.wait_for_block_num(tip, READY).await?;
            recipient.sync().await?;

            let balance = recipient.balances().await?;
            assert_eq!(
                balance.get(Pool::Orchard.ztest()),
                SEND_AMOUNT - SHIELD_FEE,
                "shielded balance must be the send net of the ZIP-317 fee \
                 (below NU6.3 the shield deposits into the Orchard pool)"
            );
            assert_eq!(balance.get(Pool::Ironwood.ztest()), 0);
            Ok(())
        }
    }
}
