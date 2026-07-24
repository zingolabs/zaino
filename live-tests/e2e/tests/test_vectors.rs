//! Holds code used to build test vector data for unit tests. These tests should
//! not be run by default or in CI.
//!
//! MIGRATION NOTE (ztest): dev built these vectors by driving Zaino's
//! **in-process** state service directly — it took `TestManager`'s
//! `service_subscriber`, issued `zebra_state::ReadRequest`s against the
//! `ReadStateService` (matched via the `expected_read_response!` macro), and
//! serialized per-block data / tree roots / treestates to `tests/vectors_tmp/`.
//! Under ztest Zaino runs in a pod and is reachable only over its lightwallet
//! gRPC / JSON-RPC surface; the in-process `ReadStateService` / `state_subscriber`
//! has no pod surface, so this vector-builder cannot run here. It is preserved
//! as a documented `#[ignore]` stub (dev's name, attributes and build steps
//! recorded below) and should be re-homed to a `packages/zaino-state` unit test.

use std::time::Duration;

use anyhow::Result;
use ztest::prelude::*;

/// Indexer sync / pod-ready timeout.
const READY: Duration = Duration::from_secs(120);
/// The committed vectors encode a ~200-block regtest chain.
const CHAIN_LEN: u32 = 200;

#[ztest::qos::testnet]
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(feature = "devtool-incompatible"),
    ignore = "Not a test: builds test-vector data for zaino_state::chain_index unit tests. ztest gap: dev serialized the vectors via the in-process ReadStateService/state_subscriber, which has no pod surface, so this can only stand up and serve the chain — re-home vector generation to a packages/zaino-state unit test."
)]
async fn create_200_block_regtest_chain_vectors() -> Result<()> {
    // The reachable half of dev's vector builder: stand up a transparent-mining
    // Zebrad on a shared volume with a state-backend zainod and mine the
    // ~200-block regtest chain the vectors encode, so the state indexer serves
    // it. The committed vectors encode a transparent-mined chain, so mining stays
    // transparent to keep that shape when generation is re-homed.
    let mut env = TestEnv::builder().ready_timeout(READY);
    let vol = env.shared_volume("zebra-db");
    let validator = env.add_validator(
        Validator::zebrad("6.2.0")
            .regtest()
            .mine_to(Pool::Transparent)
            .mount(&vol),
    );
    let indexer = env.add_indexer(
        dev!(Indexer::Zainod, "../../Dockerfile")
            .regtest()
            .tuning(ZainoTuning::State)
            .mount(&vol),
    );
    env.build().await?;

    let tip = validator.generate_blocks(CHAIN_LEN).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    // TODO: dev serialized vectors via the in-process ReadStateService /
    // state_subscriber, which has no pod surface — for each block it issued
    // `zebra_state::ReadRequest`s (unwrapped via `expected_read_response!`),
    // collected per-block data, Sapling/Orchard tree roots, and treestates, and
    // wrote them (via `zaino_state::{CompactSize, ChainWork, IndexedBlock, ...}`)
    // to `tests/vectors_tmp/*.{dat,json}`, then read them back to validate the
    // round-trip. Re-home that serialization to a packages/zaino-state unit test.
    Ok(())
}
