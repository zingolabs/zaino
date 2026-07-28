//! `Serviceable` on the live `Runtime`: the manifest reflects the *current*
//! finalised watermark and NFS readiness — a control read, gathered fresh each
//! call (not pinned). Proves the wiring; the projection itself is unit-tested in
//! `serviceability.rs`.

#[path = "support/mocks.rs"]
mod mocks;

use zaino_core::{Capability, Height};
use zaino_service::Serviceable;

use mocks::{build_runtime, h, Calls};

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
