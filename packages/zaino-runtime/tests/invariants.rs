//! Cross-cutting invariants that tie the *manifest* to actual *read* behaviour.
//!
//! The serviceability tests check the manifest, and the routing tests check the
//! reads — but the load-bearing property is that they *agree*: a capability is
//! advertised as answerable (to a height) iff a read of it actually succeeds
//! (within that height). These tests assert the biconditional directly, across
//! the application-state matrix, so the two paths can't drift apart.

#[path = "support/mocks.rs"]
mod mocks;

use zaino_core::{BlockRef, Capability, Height, TransactionHash, TransparentAddress};
use zaino_service::error::{AddressReadError, BlockReadError, TxReadError};
use zaino_service::{AddressRead, CompactBlockRead, Serviceable, TransactionRead};

use mocks::{assemble_runtime, build_runtime, h, Calls, MockRuntime};

/// The height a capability is advertised answerable to now (`None` = absent /
/// not-now).
fn advertised(runtime: &MockRuntime, cap: Capability) -> Option<Height> {
    runtime
        .serviceability()
        .answerable
        .into_iter()
        .find(|(c, _)| *c == cap)
        .and_then(|(_, height)| height)
}

/// **Route invariant:** the manifest's answerable height for `Blocks` is exactly
/// the boundary between an `Ok` and a `NotServiceable` compact-block read. A read
/// at height `h` succeeds iff `h <= advertised(Blocks)` — so the number the
/// manifest publishes is the number the reads actually honour.
#[tokio::test]
async fn manifest_height_is_the_route_read_boundary() {
    for nfs_ready in [true, false] {
        let calls = Calls::default();
        let runtime = build_runtime(&calls, 100, nfs_ready, true).await;
        let ceiling = advertised(&runtime, Capability::Blocks).expect("blocks always served");
        let snap = runtime.snapshot();

        // At the advertised ceiling: answerable.
        let at = snap.compact_block(BlockRef::Height(ceiling)).await;
        assert!(
            !matches!(at, Err(BlockReadError::NotServiceable(_))),
            "ready={nfs_ready}: read at the advertised ceiling must be serviceable, got {at:?}"
        );

        // One past it: unserviceable exactly when the manifest stops advertising
        // (i.e. while syncing, where the ceiling is the watermark).
        let past = h(u32::from(ceiling) + 1);
        let past_ok = !matches!(
            snap.compact_block(BlockRef::Height(past)).await,
            Err(BlockReadError::NotServiceable(_))
        );
        assert_eq!(
            past_ok, nfs_ready,
            "ready={nfs_ready}: serviceability past the advertised ceiling must match readiness"
        );
    }
}

/// **Passthrough invariant:** `Transactions` is advertised iff a raw-tx read
/// succeeds — both gated by the same passthrough config, in lockstep.
#[tokio::test]
async fn tx_advertised_iff_answerable() {
    for passthrough in [true, false] {
        let calls = Calls::default();
        let runtime = build_runtime(&calls, 100, true, passthrough).await;
        let is_advertised = advertised(&runtime, Capability::Transactions).is_some();
        let snap = runtime.snapshot();

        let answered = !matches!(
            snap.transaction(TransactionHash::from([7u8; 32])).await,
            Err(TxReadError::NotServiceable(_))
        );
        assert_eq!(
            is_advertised, answered,
            "passthrough={passthrough}: Transactions must be advertised iff answerable"
        );
    }
}

/// **Merge invariant:** `AddressHistory` is advertised iff an address read
/// succeeds — across both the type-gated opt-in (served) and the sync state.
#[tokio::test]
async fn address_advertised_iff_answerable() {
    for serve in [true, false] {
        for nfs_ready in [true, false] {
            let calls = Calls::default();
            let runtime = assemble_runtime(&calls, 100, nfs_ready, true, serve).await;
            let is_advertised = advertised(&runtime, Capability::AddressHistory).is_some();
            let snap = runtime.snapshot();

            let addr = TransparentAddress::new("t1example".to_string());
            let answered = !matches!(
                snap.unspent_outpoints(&addr).await,
                Err(AddressReadError::NotServiceable(_))
            );
            assert_eq!(
                is_advertised, answered,
                "serve={serve} ready={nfs_ready}: AddressHistory must be advertised iff answerable"
            );
        }
    }
}
