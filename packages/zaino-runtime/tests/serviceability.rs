//! `Serviceable` on the live `Runtime`: the manifest reflects the *current*
//! finalised watermark and NFS readiness — a control read, gathered fresh each
//! call (not pinned). Proves the wiring; the projection itself is unit-tested in
//! `serviceability.rs`.

#[path = "support/mocks.rs"]
mod mocks;

use zaino_core::{Capability, Height, TransparentAddress};
use zaino_service::error::AddressReadError;
use zaino_service::{AddressRead, Serviceable};

use mocks::{assemble_runtime, build_runtime, h, Calls};

fn answerable(runtime: &mocks::MockRuntime, cap: Capability) -> Option<Height> {
    runtime
        .serviceability()
        .answerable
        .into_iter()
        .find(|(c, _)| *c == cap)
        .unwrap_or_else(|| panic!("{cap:?} absent"))
        .1
}

#[tokio::test]
async fn ready_window_reports_the_tip_across_tiers() {
    let calls = Calls::default();
    // watermark 100, NFS ready (mock tip height 150), passthrough on.
    let runtime = build_runtime(&calls, 100, true, true).await;

    assert_eq!(answerable(&runtime, Capability::Blocks), Some(h(150))); // route → tip
    assert_eq!(answerable(&runtime, Capability::AddressHistory), Some(h(150))); // merge → tip
    assert_eq!(answerable(&runtime, Capability::Transactions), Some(h(150))); // passthrough
}

#[tokio::test]
async fn syncing_window_degrades_to_finalised_and_drops_the_merge() {
    let calls = Calls::default();
    // watermark 100, NFS still syncing, passthrough on.
    let runtime = build_runtime(&calls, 100, false, true).await;

    // Route degrades to the watermark; the merge is unanswerable until ready.
    assert_eq!(answerable(&runtime, Capability::Blocks), Some(h(100)));
    assert_eq!(answerable(&runtime, Capability::AddressHistory), None);
    assert_eq!(answerable(&runtime, Capability::Transactions), Some(h(100)));
}

/// The closed gap: a deployment that didn't opt into address history (even
/// though the mock FS *type* can back it) neither **advertises** it (absent from
/// the manifest) nor **answers** it (the read refuses) — one served set drives
/// both, so they can't disagree.
#[tokio::test]
async fn unbacked_capability_is_neither_advertised_nor_answered() {
    let calls = Calls::default();
    // Ready window, passthrough on, but `serve_address = false`.
    let runtime = assemble_runtime(&calls, 100, true, true, false).await;

    // Not advertised: absent entirely (not "present but None").
    let advertised = runtime
        .serviceability()
        .answerable
        .iter()
        .any(|(c, _)| *c == Capability::AddressHistory);
    assert!(!advertised, "unbacked capability must be absent from the manifest");

    // Not answered: the read refuses with the same verdict.
    let snap = runtime.snapshot();
    let addr = TransparentAddress::new("t1example".to_string());
    let res = snap.unspent_outpoints(&addr).await;
    assert!(matches!(
        res,
        Err(AddressReadError::NotServiceable(Capability::AddressHistory))
    ));
}
