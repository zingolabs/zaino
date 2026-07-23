//! Exercises the compose seam: `Runtime` routes reads FS (`≤ watermark`) vs NFS
//! (`> watermark`), merges where a capability spans both tiers, and passes
//! through to the validator source for data it doesn't store. The mocks record
//! which provider was asked, so routing is asserted directly — including that
//! the *wrong* provider is never consulted.

#[path = "support/mocks.rs"]
mod mocks;

use zaino_core::{BlockHash, BlockRef, TransparentAddress};
use zaino_service::error::BlockReadError;
use zaino_service::{AddressRead, BlockRead, CompactBlockRead};

use mocks::{build_runtime, h, Calls};

#[tokio::test]
async fn routes_finalised_to_fs_and_recent_to_nfs() {
    let calls = Calls::default();
    let runtime = build_runtime(&calls, 100, true, true).await;
    let snap = runtime.snapshot();

    // Finalised height (<= watermark) → FS.
    assert!(snap
        .compact_block(BlockRef::Height(h(50)))
        .await
        .expect("fs read ok")
        .is_none());
    // Recent height (> watermark) → NFS.
    assert!(snap
        .compact_block(BlockRef::Height(h(120)))
        .await
        .expect("nfs read ok")
        .is_none());

    assert_eq!(calls.log(), vec!["fs:50".to_string(), "nfs:120".to_string()]);
}

#[tokio::test]
async fn recent_reads_are_not_serviceable_while_nfs_syncs() {
    let calls = Calls::default();
    let runtime = build_runtime(&calls, 100, false, true).await;
    let snap = runtime.snapshot();

    // Recent read while the window is syncing → NotServiceable, and NFS is never
    // consulted (no false `None`).
    let res = snap.compact_block(BlockRef::Height(h(120))).await;
    assert!(matches!(res, Err(BlockReadError::NotServiceable(_))));
    assert!(
        calls.log().iter().all(|c| !c.starts_with("nfs:")),
        "NFS must not be read while syncing, got {:?}",
        calls.log()
    );
}

/// US-1.3: an address's unspent set is a merge of both tiers, not a route —
/// both FS and NFS must be consulted.
#[tokio::test]
async fn address_unspent_merges_fs_and_nfs() {
    let calls = Calls::default();
    let runtime = build_runtime(&calls, 100, true, true).await;
    let snap = runtime.snapshot();

    let addr = TransparentAddress::new("t1example".to_string());
    let utxos = snap.unspent_outpoints(&addr).await.expect("merge ok");
    assert!(utxos.is_empty());

    let log = calls.log();
    assert!(log.contains(&"addr-fs".to_string()), "FS not consulted: {log:?}");
    assert!(log.contains(&"addr-nfs".to_string()), "NFS not consulted: {log:?}");
}

// --- US-1.7: full blocks and raw txs pass through to the validator, by hash ---

/// A full `Block` is not stored — it goes to the validator source, keyed by
/// hash, so the answer is reorg-coherent. Neither cache tier is consulted.
#[tokio::test]
async fn full_block_passes_through_by_hash() {
    let calls = Calls::default();
    let runtime = build_runtime(&calls, 100, true, true).await;
    let snap = runtime.snapshot();

    let block = snap
        .block(BlockRef::Hash(BlockHash::from([0xBB; 32])))
        .await
        .expect("passthrough ok");
    assert!(block.is_none());

    assert_eq!(calls.log(), vec!["source:block".to_string()]);
}

/// With passthrough disabled by config, a full-block read is `NotServiceable`
/// and the validator source is never hit.
#[tokio::test]
async fn passthrough_disabled_is_not_serviceable() {
    let calls = Calls::default();
    let runtime = build_runtime(&calls, 100, true, false).await;
    let snap = runtime.snapshot();

    let res = snap.block(BlockRef::Hash(BlockHash::from([0xBB; 32]))).await;
    assert!(matches!(res, Err(BlockReadError::NotServiceable(_))));
    assert!(
        calls.log().is_empty(),
        "validator must not be hit when passthrough is disabled, got {:?}",
        calls.log()
    );
}
