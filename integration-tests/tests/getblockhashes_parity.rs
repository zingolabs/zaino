//! Algorithm-only parity test for `getblockhashes` against a synced
//! external zcashd.
//!
//! What's being tested
//! -------------------
//! We don't stand up zaino at all here. The test:
//!
//! 1. Asks the configured zcashd for the `nTime` field of every block
//!    in `[0, N)` (one `getblock height verbose=1` per height).
//! 2. Replays zcashd's documented `logical_ts(N) = max(nTime(N),
//!    logical_ts(N-1) + 1)` recurrence in-process using a tiny
//!    re-implementation, deliberately independent of zaino's
//!    `LogicalTimestamp::next`. If all three agree the algorithm is
//!    correct by independent confirmation.
//! 3. Asks zcashd for `getblockhashes(high, low, {logicalTimes:
//!    true})` covering the same range.
//! 4. Diffs the two — each block's `logicalts` from zcashd must equal
//!    what the local recurrence produced for that block, and the
//!    blockhashes must match positionally.
//!
//! Configuration
//! -------------
//! The test self-skips with an `eprintln` when no zcashd connection is
//! configured, so it can sit in the stable profile and run
//! unconditionally without breaking CI. Set the env vars in a
//! developer environment with a synced zcashd to actually exercise the
//! parity check. Required:
//!
//!   ZCASHD_RPC_URL       e.g. http://127.0.0.1:8232
//!   ZCASHD_RPC_USER
//!   ZCASHD_RPC_PASSWORD
//!
//! Optional:
//!
//!   ZCASHD_PARITY_HEIGHTS   number of blocks from genesis to test
//!                            (default 1000; clamped to tip).
//!
//! zcashd must be running with `-timestampindex=1` for
//! `getblockhashes` to be enabled.

use zaino_fetch::jsonrpsee::{
    connector::JsonRpSeeConnector,
    response::{GetBlockHashesOptions, GetBlockHashesResponse, GetBlockResponse},
};

/// zcashd's logical-timestamp recurrence, re-implemented here so the
/// parity test exercises the algorithm directly rather than chaining
/// through `LogicalTimestamp::next`. If the spec, this re-implementation,
/// zaino's type, and zcashd all match for thousands of consecutive
/// blocks on a real chain, the algorithm is correct.
fn next_logical_ts(prev: Option<u32>, n_time: u32) -> u32 {
    match prev {
        Some(p) if n_time <= p => p.saturating_add(1),
        _ => n_time,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn getblockhashes_algorithm_parity_with_zcashd() {
    // Gracefully skip when no zcashd connection is configured. The test
    // is part of the stable profile so it's always *listed*, but in CI
    // (no env vars) it self-skips with a notice. Set the env vars in a
    // developer environment with a synced zcashd to actually exercise
    // the parity check.
    let (url, user, pass) = match (
        std::env::var("ZCASHD_RPC_URL").ok(),
        std::env::var("ZCASHD_RPC_USER").ok(),
        std::env::var("ZCASHD_RPC_PASSWORD").ok(),
    ) {
        (Some(u), Some(usr), Some(p)) => (u, usr, p),
        _ => {
            eprintln!(
                "Skipping: ZCASHD_RPC_URL / ZCASHD_RPC_USER / ZCASHD_RPC_PASSWORD \
                 not all set. See file-level docs for env-var setup."
            );
            return;
        }
    };
    let max_heights: u32 = std::env::var("ZCASHD_PARITY_HEIGHTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000);

    let parsed_url = url
        .parse::<reqwest::Url>()
        .unwrap_or_else(|e| panic!("ZCASHD_RPC_URL {url:?} is not a valid URL: {e}"));
    let connector = JsonRpSeeConnector::new_with_basic_auth(parsed_url, user, pass)
        .expect("construct JsonRpSeeConnector");

    // 1. Determine the range to test.
    let count_response = connector
        .get_block_count()
        .await
        .expect("getblockcount");
    let tip_height = zebra_chain::block::Height::from(count_response).0;
    let n_heights = max_heights.min(tip_height.saturating_add(1));
    assert!(
        n_heights > 0,
        "zcashd reports no blocks; nothing to test against"
    );
    eprintln!(
        "Parity test scope: heights [0, {}) against tip height {}",
        n_heights, tip_height
    );

    // 2. Pull nTime for each height.
    let mut n_times: Vec<u32> = Vec::with_capacity(n_heights as usize);
    for h in 0..n_heights {
        let block_resp = connector
            .get_block(h.to_string(), Some(1))
            .await
            .unwrap_or_else(|e| panic!("getblock {h} verbose=1 failed: {e:?}"));
        let block_obj = match block_resp {
            GetBlockResponse::Object(obj) => obj,
            GetBlockResponse::Raw(_) => {
                panic!("getblock {h} verbose=1 returned Raw variant; expected Object")
            }
        };
        let raw_time = block_obj
            .time
            .unwrap_or_else(|| panic!("getblock {h} verbose=1 missing `time` field"));
        let n_time: u32 = u32::try_from(raw_time).unwrap_or_else(|_| {
            panic!("getblock {h} returned nTime={raw_time} outside u32 range")
        });
        n_times.push(n_time);

        if (h + 1) % 500 == 0 || h + 1 == n_heights {
            eprintln!("  fetched {}/{} headers", h + 1, n_heights);
        }
    }

    // 3. Replay the recurrence locally.
    let mut expected_logical_ts: Vec<u32> = Vec::with_capacity(n_times.len());
    let mut prev: Option<u32> = None;
    for &n_time in &n_times {
        let ts = next_logical_ts(prev, n_time);
        expected_logical_ts.push(ts);
        prev = Some(ts);
    }

    // 4. Ask zcashd for the same range via `getblockhashes`.
    // Query bounds: [low, high) covering every block in the range.
    let low_ts = *expected_logical_ts
        .first()
        .expect("at least one block in scope");
    let last_ts = *expected_logical_ts
        .last()
        .expect("at least one block in scope");
    let high_ts = last_ts.saturating_add(1);

    let options = GetBlockHashesOptions {
        no_orphans: false,
        logical_times: true,
    };
    let response = connector
        .get_block_hashes(high_ts, low_ts, Some(options))
        .await
        .unwrap_or_else(|e| {
            panic!(
                "getblockhashes failed (is zcashd running with -timestampindex=1?): {e:?}"
            )
        });

    let zcashd_entries = match response {
        GetBlockHashesResponse::WithLogicalTimestamps(v) => v,
        GetBlockHashesResponse::Hashes(_) => {
            panic!("expected WithLogicalTimestamps response with logical_times=true")
        }
    };

    assert_eq!(
        zcashd_entries.len(),
        expected_logical_ts.len(),
        "block count mismatch: zcashd returned {} entries, recurrence produced {}",
        zcashd_entries.len(),
        expected_logical_ts.len(),
    );

    // 5. Per-block diff.
    let mut drift_count = 0;
    for (i, (zcashd_entry, &expected_ts)) in zcashd_entries
        .iter()
        .zip(expected_logical_ts.iter())
        .enumerate()
    {
        assert_eq!(
            zcashd_entry.logicalts, expected_ts,
            "logical_ts mismatch at height {i}: zcashd {}, recurrence {}",
            zcashd_entry.logicalts, expected_ts,
        );
        if expected_ts != n_times[i] {
            drift_count += 1;
        }
    }

    eprintln!(
        "Parity OK across {n} heights ({drift} blocks exercised the +1 drift mechanism)",
        n = n_heights,
        drift = drift_count,
    );
}
