//! Anti-misrouting matrix.
//!
//! Sweeps the application-state axes a read resolves against — NFS window
//! `ready`/`syncing`, passthrough `enabled`/`disabled`, and (for routed reads)
//! the height tier — and, for every cell, asserts *both*:
//!
//! 1. the **outcome** (serviceable vs `NotServiceable`), and
//! 2. exactly **which providers were consulted** — so a degraded state can
//!    never silently misroute to the wrong tier (a recent read falling through
//!    to FS, a merge returning an FS-only partial, a synthetic capability
//!    getting a validator fallback it must not have).
//!
//! Each capability class also pins the axes it must be *independent* of: a
//! routed cache read ignores passthrough config; a synthetic merge ignores it
//! too (no validator port exists for it); a passthrough read ignores NFS sync.

#[path = "support/mocks.rs"]
mod mocks;

use zaino_core::{BlockHash, BlockRef, TransparentAddress};
use zaino_service::error::{AddressReadError, BlockReadError};
use zaino_service::{AddressRead, BlockRead, CompactBlockRead};

use mocks::{build_runtime, h, Calls};

const WATERMARK: u32 = 100;

/// Assert a slice of recorded provider calls equals the expected set, in order.
fn assert_consulted(actual: Vec<String>, expected: &[&str], case: &str) {
    let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    assert_eq!(actual, expected, "wrong providers consulted for {case}");
}

// --- Route: `compact_block(height)` — one tier, by height at the watermark ---

/// A routed read depends on (height vs watermark) and, for a recent height,
/// NFS-readiness. It is *independent of passthrough config* — a compact block
/// is a cache read, never a passthrough. The syncing+recent cell is the crux:
/// it must be `NotServiceable` with **neither** tier consulted (no fall-through
/// to FS for a height FS doesn't own).
#[tokio::test]
async fn route_matrix() {
    struct Case {
        nfs_ready: bool,
        passthrough: bool,
        height: u32,
        serviceable: bool,
        consulted: &'static [&'static str],
    }
    let cases = [
        // Finalised height → FS, regardless of NFS sync or passthrough config.
        Case { nfs_ready: true, passthrough: true, height: 50, serviceable: true, consulted: &["fs:50"] },
        Case { nfs_ready: true, passthrough: false, height: 50, serviceable: true, consulted: &["fs:50"] },
        Case { nfs_ready: false, passthrough: true, height: 50, serviceable: true, consulted: &["fs:50"] },
        // Recent height, window ready → NFS (passthrough config irrelevant).
        Case { nfs_ready: true, passthrough: true, height: 120, serviceable: true, consulted: &["nfs:120"] },
        Case { nfs_ready: true, passthrough: false, height: 120, serviceable: true, consulted: &["nfs:120"] },
        // Recent height, window syncing → NotServiceable, nothing consulted.
        Case { nfs_ready: false, passthrough: true, height: 120, serviceable: false, consulted: &[] },
        Case { nfs_ready: false, passthrough: false, height: 120, serviceable: false, consulted: &[] },
    ];

    for c in cases {
        let label = format!(
            "route(ready={}, passthrough={}, height={})",
            c.nfs_ready, c.passthrough, c.height
        );
        let calls = Calls::default();
        let runtime = build_runtime(&calls, WATERMARK, c.nfs_ready, c.passthrough).await;
        let snap = runtime.snapshot();

        let res = snap.compact_block(BlockRef::Height(h(c.height))).await;
        if c.serviceable {
            assert!(res.expect(&label).is_none(), "{label}: expected empty-but-served");
        } else {
            assert!(
                matches!(res, Err(BlockReadError::NotServiceable(_))),
                "{label}: expected NotServiceable, got {res:?}"
            );
        }
        assert_consulted(calls.log(), c.consulted, &label);
    }
}

// --- Merge: `unspent_outpoints(addr)` — both tiers, or nothing ---

/// A merge needs both tiers to be coherent, so a syncing window makes it
/// `NotServiceable` — it must **not** return an FS-only partial that looks
/// complete. Address history is *synthetic* (no `PassthroughSource` port), so
/// passthrough config can never rescue it: syncing+passthrough-enabled is still
/// `NotServiceable`, and the validator is never consulted.
#[tokio::test]
async fn merge_matrix() {
    struct Case {
        nfs_ready: bool,
        passthrough: bool,
        serviceable: bool,
        consulted: &'static [&'static str],
    }
    let cases = [
        // Ready → both tiers, in order (FS then NFS). Passthrough irrelevant.
        Case { nfs_ready: true, passthrough: true, serviceable: true, consulted: &["addr-fs", "addr-nfs"] },
        Case { nfs_ready: true, passthrough: false, serviceable: true, consulted: &["addr-fs", "addr-nfs"] },
        // Syncing → NotServiceable, nothing consulted — even with passthrough
        // enabled, because a synthetic capability has no validator fallback.
        Case { nfs_ready: false, passthrough: true, serviceable: false, consulted: &[] },
        Case { nfs_ready: false, passthrough: false, serviceable: false, consulted: &[] },
    ];

    for c in cases {
        let label = format!("merge(ready={}, passthrough={})", c.nfs_ready, c.passthrough);
        let calls = Calls::default();
        let runtime = build_runtime(&calls, WATERMARK, c.nfs_ready, c.passthrough).await;
        let snap = runtime.snapshot();

        let addr = TransparentAddress::new("t1example".to_string());
        let res = snap.unspent_outpoints(&addr).await;
        if c.serviceable {
            assert!(res.expect(&label).is_empty(), "{label}: expected empty-but-served");
        } else {
            assert!(
                matches!(res, Err(AddressReadError::NotServiceable(_))),
                "{label}: expected NotServiceable, got {res:?}"
            );
        }
        assert_consulted(calls.log(), c.consulted, &label);
    }
}

// --- Passthrough: `block(hash)` — the validator source, gated by config ---

/// A passthrough read is keyed by hash (reorg-coherent), so it is *independent
/// of NFS sync state* — it serves while the window is still syncing. Its only
/// gate is passthrough config: disabled → `NotServiceable` with the validator
/// never hit.
#[tokio::test]
async fn passthrough_matrix() {
    struct Case {
        nfs_ready: bool,
        passthrough: bool,
        serviceable: bool,
        consulted: &'static [&'static str],
    }
    let cases = [
        // Enabled → validator, whether the window is ready or still syncing.
        Case { nfs_ready: true, passthrough: true, serviceable: true, consulted: &["source:block"] },
        Case { nfs_ready: false, passthrough: true, serviceable: true, consulted: &["source:block"] },
        // Disabled → NotServiceable, validator never consulted, any NFS state.
        Case { nfs_ready: true, passthrough: false, serviceable: false, consulted: &[] },
        Case { nfs_ready: false, passthrough: false, serviceable: false, consulted: &[] },
    ];

    for c in cases {
        let label = format!(
            "passthrough(ready={}, passthrough={})",
            c.nfs_ready, c.passthrough
        );
        let calls = Calls::default();
        let runtime = build_runtime(&calls, WATERMARK, c.nfs_ready, c.passthrough).await;
        let snap = runtime.snapshot();

        let res = snap.block(BlockRef::Hash(BlockHash::from([0xBB; 32]))).await;
        if c.serviceable {
            assert!(res.expect(&label).is_none(), "{label}: expected empty-but-served");
        } else {
            assert!(
                matches!(res, Err(BlockReadError::NotServiceable(_))),
                "{label}: expected NotServiceable, got {res:?}"
            );
        }
        assert_consulted(calls.log(), c.consulted, &label);
    }
}
