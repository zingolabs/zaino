//! The contract every chain-serving source must honour, as reusable assertions.
//!
//! One contract; each adapter proves it by running this battery against itself.
//! The assertions are transport-agnostic — they speak only the source traits —
//! so a mock, a JSON-RPC adapter, a read-state adapter, and the composite all
//! run the same checks. What a single adapter cannot show on its own is a
//! *composition* fault (a router preferring a stale sub-source); that surfaces
//! only when the battery runs against the composite over a live, advancing
//! chain, which is what [`assert_follows_to_tip`] is for.
//!
//! The battery panics on the first violation, naming it — it is meant to be
//! called from a `#[tokio::test]` (or a live-test) that treats a panic as the
//! failure, not to be handled.

use core::time::Duration;

use zaino_primitives::types::Height;

use crate::{OneShotGetBlock, OneShotGetBlockByHash, OneShotGetChainTip, QueryError};

/// The source reports a tip whose block is retrievable and self-consistent: the
/// block at the tip height exists, reports that height, and carries the hash the
/// tip named.
pub async fn assert_tip_consistent<S>(source: &S)
where
    S: OneShotGetChainTip + OneShotGetBlock,
{
    let (tip_hash, tip_height) = source
        .get_chain_tip()
        .await
        .expect("source reports a chain tip");
    let block = source
        .get_block(tip_height)
        .await
        .expect("the tip height's block is retrievable");
    assert_eq!(
        block.header.height, tip_height,
        "the block at the tip height reports that height"
    );
    assert_eq!(
        block.header.hash, tip_hash,
        "the block at the tip height carries the tip hash"
    );
}

/// Every height in `0..=tip` is present, the chain is hash-linked (each block's
/// `prev_hash` is its predecessor's hash), and every block round-trips by hash.
pub async fn assert_contiguous_and_linked<S>(source: &S)
where
    S: OneShotGetChainTip + OneShotGetBlock + OneShotGetBlockByHash,
{
    let (_, tip_height) = source
        .get_chain_tip()
        .await
        .expect("source reports a chain tip");
    let tip = u32::from(tip_height);
    let mut prev_hash = None;
    for h in 0..=tip {
        let height = Height::try_from(h).expect("h <= tip is a valid height");
        let block = source
            .get_block(height)
            .await
            .unwrap_or_else(|e| panic!("height {h} at or below the tip must be present: {e:?}"));
        assert_eq!(
            u32::from(block.header.height),
            h,
            "the block returned for height {h} reports height {h}"
        );
        if let Some(expected_prev) = prev_hash {
            assert_eq!(
                block.header.prev_hash, expected_prev,
                "block {h} links to its predecessor"
            );
        }
        let by_hash = source
            .get_block_by_hash(block.header.hash)
            .await
            .unwrap_or_else(|e| panic!("block {h} must be retrievable by its hash: {e:?}"));
        assert_eq!(
            by_hash.header.height, block.header.height,
            "by-hash lookup round-trips to the same block as by-height"
        );
        prev_hash = Some(block.header.hash);
    }
}

/// A height above the tip is a typed *domain* miss — never a panic, never a
/// silently-wrong block, and never a transport error masquerading as absence.
/// A consumer must be able to tell "not there" from "cannot reach the source".
pub async fn assert_above_tip_is_typed_miss<S>(source: &S)
where
    S: OneShotGetChainTip + OneShotGetBlock,
{
    let (_, tip_height) = source
        .get_chain_tip()
        .await
        .expect("source reports a chain tip");
    let above = Height::try_from(u32::from(tip_height) + 1).expect("tip + 1 is a valid height");
    match source.get_block(above).await {
        Err(QueryError::Domain(_)) => {}
        Err(QueryError::NonDomain(e)) => {
            panic!("a height above the tip must be a domain miss, not a transport error: {e}")
        }
        Ok(_) => panic!("a height above the tip must not return a block"),
    }
}

/// The full static battery, for a source at a fixed chain state.
pub async fn assert_chain_source_conformance<S>(source: &S)
where
    S: OneShotGetChainTip + OneShotGetBlock + OneShotGetBlockByHash,
{
    assert_tip_consistent(source).await;
    assert_contiguous_and_linked(source).await;
    assert_above_tip_is_typed_miss(source).await;
}

/// Liveness — the assertion a boot-time snapshot source fails.
///
/// After the underlying chain has advanced to `expected_tip`, the source must
/// *eventually* (within `timeout`) report a tip at or beyond it and serve the
/// contiguous range up to it. A source frozen at a stale height — a read-only
/// snapshot that never catches up, a composite routing the tip through a stale
/// sub-source — never reaches `expected_tip`, so this times out and panics.
///
/// This is the check a fixed-state test structurally cannot make: at one height
/// a frozen source looks correct; only advancing the chain reveals that it does
/// not follow.
pub async fn assert_follows_to_tip<S>(source: &S, expected_tip: Height, timeout: Duration)
where
    S: OneShotGetChainTip + OneShotGetBlock + OneShotGetBlockByHash,
{
    let poll = Duration::from_millis(200);
    let reached = tokio::time::timeout(timeout, async {
        loop {
            if let Ok((_, tip)) = source.get_chain_tip().await {
                if tip >= expected_tip {
                    return;
                }
            }
            tokio::time::sleep(poll).await;
        }
    })
    .await;
    assert!(
        reached.is_ok(),
        "source did not reach tip {expected_tip:?} within {timeout:?} — it is not following the chain"
    );
    // Reached the tip; the served range up to it must also be well-formed.
    assert_contiguous_and_linked(source).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::linked_test_chain;

    #[tokio::test]
    async fn a_well_formed_mock_chain_passes_the_battery() {
        let chain = linked_test_chain(6);
        assert_chain_source_conformance(&chain).await;
    }

    #[tokio::test]
    async fn a_source_already_at_the_tip_follows_immediately() {
        let chain = linked_test_chain(6);
        let tip = Height::try_from(5).expect("valid height");
        assert_follows_to_tip(&chain, tip, Duration::from_millis(500)).await;
    }

    #[tokio::test]
    #[should_panic(expected = "not following the chain")]
    async fn a_frozen_source_below_the_target_fails_liveness() {
        // A chain that stops at height 2 stands in for a frozen snapshot; asking
        // it to reach height 5 must time out — the exact shape of the read-state
        // freeze this battery exists to catch.
        let frozen = linked_test_chain(3);
        let target = Height::try_from(5).expect("valid height");
        assert_follows_to_tip(&frozen, target, Duration::from_millis(300)).await;
    }
}
