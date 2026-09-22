//! Conformance kit for the light-wallet serving surface.
//!
//! The executable acceptance target for the light-serve read-set:
//! [`assert_light_wallet_reads`] drives every read the light-wallet demand
//! pulls through a pinned view and asserts each is *serviceable* — a real
//! answer, a domain miss (`Ok(None)`), or a real backend failure, but never the
//! `NotServiceable` stub. [`assert_light_serve_conformance`] wraps it at the
//! service boundary: pin a snapshot, run the read-set through it, then exercise
//! the controls.
//!
//! Bound to the [`LightWalletReads`] read-set and the [`LightServeService`]
//! profile, so any engine claiming that profile is checked against the same
//! battery. It greens exactly when the full read-set is wired; until then it is
//! the red target a work-in-progress engine fails.

use zaino_core::{BlockRef, Height, HeightRange, ShieldedPool, TransactionId, TransparentAddress};

use crate::{LightServeService, LightWalletReads};

/// Panic if `$result` is the `NotServiceable` stub; any other outcome — an
/// answer, a domain miss, or a real `Transient` / `Fatal` failure — passes. A
/// macro, not a `fn`: the per-capability read errors are distinct types sharing
/// only the inherent `is_not_serviceable` predicate, not a trait a bound could
/// name.
macro_rules! assert_serviceable {
    ($result:expr, $label:literal) => {
        if let Err(ref e) = $result {
            assert!(
                !e.is_not_serviceable(),
                concat!($label, " is not serviceable: {:?}"),
                e
            );
        }
    };
}

/// Assert every read in the [`LightWalletReads`] set is serviceable on `snap`.
///
/// Each read is driven with a benign probe argument; the assertion is only that
/// the answer is not the `NotServiceable` stub. The probe values need not exist
/// in the view — a miss (`Ok(None)`) or a definitive `Fatal` are both serviceable
/// outcomes; the sole failure this catches is a capability the engine has not
/// yet wired.
pub async fn assert_light_wallet_reads<Snap: LightWalletReads>(snap: &Snap) {
    let height = Height::GENESIS;
    let block_ref = BlockRef::Height(height);
    let range = HeightRange {
        start: height,
        end: height,
    };
    let txid = TransactionId::from([0u8; 32]);
    let addr = TransparentAddress::new("t1ConformanceProbeAddressXXXXXXXXXXX".to_string());

    // Compact-block serving — the light path's block read.
    assert_serviceable!(snap.compact_block(block_ref).await, "compact_block");

    // Commitment treestate at a height, and per-pool subtree roots from an index.
    assert_serviceable!(snap.treestate(height).await, "treestate");
    assert_serviceable!(
        snap.subtree_roots(ShieldedPool::Sapling, 0, None).await,
        "subtree_roots"
    );

    // Raw transaction fetch — the wallet parses the bytes locally.
    assert_serviceable!(snap.raw_transaction(txid).await, "raw_transaction");

    // Transparent address history — the subset a light wallet pulls.
    assert_serviceable!(snap.balance(&addr, range).await, "balance");
    assert_serviceable!(snap.unspent_outpoints(&addr).await, "unspent_outpoints");
    assert_serviceable!(snap.deltas(&addr, range).await, "deltas");
    assert_serviceable!(snap.tx_ids(&addr, range).await, "tx_ids");

    // Compact block with spend nullifiers — the light-serve delta.
    assert_serviceable!(
        snap.compact_block_nullifiers(block_ref).await,
        "compact_block_nullifiers"
    );
}

/// Assert `service` conforms to the [`LightServeService`] profile at runtime:
/// its pinned snapshot serves the full light-wallet read-set, and its controls
/// are exercisable.
///
/// The read-set is the substantive half (see [`assert_light_wallet_reads`]). The
/// controls are checked for wiring only: `broadcast` has no unserviceable
/// variant — it either relays (`Ok`) or the validator rejects (`Err`), both a
/// live answer — and the subscription streams contract to *yield a stream*, an
/// empty one being a valid steady state.
pub async fn assert_light_serve_conformance<S: LightServeService>(service: &S) {
    let snapshot = service.snapshot().await.expect("snapshot acquired");
    assert_light_wallet_reads(&snapshot).await;

    // Broadcast: any outcome is a live answer; we assert only that the path runs.
    let _ = service.broadcast(vec![0u8; 32]).await;

    // Streaming controls: obtaining the stream is the wiring contract.
    let _tip = service.subscribe_tip();
    let _mempool = service.subscribe_mempool();
}
