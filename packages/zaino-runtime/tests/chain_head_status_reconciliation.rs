//! Exploration (throwaway): can dev's legacy chain-head status be presented as a
//! `zaino-component` `ComponentStatus`?
//!
//! dev's `zaino-chain-head` is doomed — it will be replaced by an `im`-based NFS
//! that implements `zaino-component` **natively** (a de-fused `ComponentStatus`
//! and a status `watch`). So this only validates the reconciliation *map* and
//! surfaces the frictions of adapting the legacy fused status; it is not a landed
//! adapter, and no chain-head internals are touched.
//!
//! Frictions this confirms an adapter would hit (all avoided by a native impl):
//! 1. fused -> de-fused is **lossy** on the error rows — the single enum
//!    overwrote the lifecycle, so the phase can only be guessed (see below);
//! 2. chain-head's status is a poll-only atomic (`NamedAtomicStatus`), so
//!    `StatusWatch` would need a poll -> `watch` bridge task;
//! 3. chain-head's `spawn` is a constructor (`-> Arc<Self>`) with no in-place
//!    restart, so it does not fit `Managed`; supervision would be observe-only.

use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle};
use zaino_status::StatusType;

const CHAIN_HEAD: ComponentName = ComponentName("chain-head");

/// Map a legacy fused [`StatusType`] onto a de-fused [`ComponentStatus`].
///
/// Lossy by nature in this direction: the fused enum stored an error value
/// *instead of* the phase, so the error rows guess `Ready` (chain-head only
/// errors after it has become `Ready`).
fn reconcile(status: StatusType) -> ComponentStatus {
    let (lifecycle, health) = match status {
        StatusType::Spawning => (Lifecycle::Spawning, Health::Healthy),
        StatusType::Syncing => (Lifecycle::Syncing, Health::Healthy),
        StatusType::Ready => (Lifecycle::Ready, Health::Healthy),
        // Load axis deferred: Busy reads as a healthy, running component.
        StatusType::Busy => (Lifecycle::Ready, Health::Healthy),
        StatusType::Closing => (Lifecycle::Closing, Health::Healthy),
        StatusType::Offline => (Lifecycle::Offline, Health::Offline),
        // Lossy: the phase was overwritten; guess Ready.
        StatusType::RecoverableError => (Lifecycle::Ready, Health::Recoverable),
        StatusType::CriticalError => (Lifecycle::Ready, Health::Critical),
    };
    ComponentStatus::new(CHAIN_HEAD, lifecycle, health)
}

#[test]
fn clean_states_map_losslessly() {
    assert_eq!(
        reconcile(StatusType::Syncing),
        ComponentStatus::new(CHAIN_HEAD, Lifecycle::Syncing, Health::Healthy)
    );
    assert_eq!(
        reconcile(StatusType::Ready),
        ComponentStatus::new(CHAIN_HEAD, Lifecycle::Ready, Health::Healthy)
    );
    assert_eq!(
        reconcile(StatusType::Closing),
        ComponentStatus::new(CHAIN_HEAD, Lifecycle::Closing, Health::Healthy)
    );
    assert_eq!(reconcile(StatusType::Offline).health, Health::Offline);
}

#[test]
fn error_states_keep_health_but_must_guess_the_phase() {
    // The fusion loss: a validator failure after `Ready` reads as
    // `RecoverableError`, stored *instead of* `Ready`, so we can only guess the
    // phase back. A native de-fused component keeps both (Ready, Recoverable).
    let recoverable = reconcile(StatusType::RecoverableError);
    assert_eq!(recoverable.health, Health::Recoverable);
    assert_eq!(
        recoverable.lifecycle,
        Lifecycle::Ready,
        "phase is a guess — the fused enum lost it"
    );

    let critical = reconcile(StatusType::CriticalError);
    assert_eq!(critical.health, Health::Critical);
    assert_eq!(critical.lifecycle, Lifecycle::Ready);
}
