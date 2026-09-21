//! Direct-mode (ReadState) compact-block serving against the new runtime stack.
//!
//! The new zainod boots the runtime Orchestra (validator → indexer → store →
//! lightserve gRPC) and serves the compact-block triad from its own index. This
//! walk proves that path end to end over the wire: a Direct/`ReadState` source
//! reads a shared regtest zebra cache, the indexer builds the current-zaino
//! index, and `GetBlockRange` serves the composed compact blocks the harness
//! streams back.
//!
//! It boots zainod through the **`ztest-fixture`** bridge: ztest 0.1.21 mounts a
//! legacy-schema `zainod.toml` the greenfield `DaemonConfig` can't parse, so the
//! image is built `--features ztest-fixture` and the env var makes zainod ignore
//! the mounted `--config` and boot an in-process regtest Direct config, pointed
//! at the shared zebra volume via `ZAINO_TEST_ZEBRA_CACHE_DIR`.
//!
//! On a short regtest chain the finalised ReadState (FS) is empty — every block
//! is still non-final — so the served compact blocks come from the NFS
//! (chain-head over the validator's JSON-RPC), which the fixture dials at the
//! validator's in-cluster address. This is the proof the NFS fills the gap an
//! FS-only setup left (that earlier e2e failed `indexer 0/0`).
//!
//! Scope is the served slice only — heights and compact-block shape. Treestate,
//! transactions, address queries and per-tx ironwood actions are out of the
//! index-only serving path and are not asserted here.

use std::time::Duration;

use anyhow::{Context, Result};
use ztest::prelude::*;

const READY: Duration = Duration::from_secs(180);

/// A Direct/`ReadState` zainod serves a contiguous compact-block range, built
/// from a shared regtest zebra cache, up to the mined tip.
///
/// multi_thread required: the harness spawns the validator and indexer services.
#[ztest::qos::integration]
#[tokio::test(flavor = "multi_thread")]
async fn direct_mode_serves_compact_block_range() -> Result<()> {
    let mut env = TestEnv::builder().ready_timeout(READY);

    // One RWO volume holds zebra's regtest state DB; both pods mount it at the
    // same path, and zaino opens it as a RocksDB secondary (Direct/ReadState).
    let zebra_vol = env.shared_volume("zebra-state");

    let validator = env.add_validator(Validator::zebrad("6.2.3").regtest().mount(&zebra_vol));
    let indexer = env.add_indexer(
        dev!(
            Indexer::Zainod,
            "../../Dockerfile",
            features = ["ztest-fixture"]
        )
        .regtest()
        .tuning(ZainoTuning::State)
        .mount(&zebra_vol)
        // Activate the in-process regtest Direct fixture and tell it where the
        // shared zebra cache is mounted (ztest picks the path).
        .env("ZAINO_TEST_REGTEST_DIRECT_FIXTURE", "1")
        .env("ZAINO_TEST_ZEBRA_CACHE_DIR", zebra_vol.mount_path())
        // The NFS dials the validator's JSON-RPC in-cluster: the validator pod's
        // DNS name (default "zebrad") on the regtest RPC port. Localhost would be
        // wrong here — the RPC lives on the validator pod, not this one.
        .env(
            "ZAINO_TEST_ZEBRA_JSONRPC",
            format!("zebrad:{}", ztest::ports::ZEBRAD_RPC),
        ),
    );
    env.build().await?;

    let tip = validator.generate_blocks(5).await?;
    indexer.wait_for_block_num(tip, READY).await?;

    // The core assertion: the store composes and serves a contiguous compact
    // block range, reaching the mined tip. On this short regtest chain the FS is
    // empty, so every served block is sourced from the NFS.
    let blocks = indexer
        .get_block_range(BlockHeight::from(1u32), tip)
        .await?;
    assert!(
        !blocks.is_empty(),
        "no compact blocks served from the index"
    );
    for (offset, block) in blocks.iter().enumerate() {
        let expected = 1 + u64::try_from(offset).expect("small range fits u64");
        assert_eq!(block.height, expected, "served heights must be contiguous");
        assert!(!block.hash.is_empty(), "served block must carry its hash");
    }
    let last = blocks.last().context("range is non-empty")?;
    assert_eq!(
        last.height,
        u64::from(tip),
        "the served range must reach the mined tip"
    );

    Ok(())
}
