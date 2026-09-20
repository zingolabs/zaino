//! Logging a component's own status transitions.
//!
//! A component's core state (lifecycle / health / reason) is *inherent to the
//! component*, so an **owned** component reports its own — one INFO line per
//! transition, rendered by [`ComponentStatus`]'s `Display` — rather than relying
//! on a supervisor to do it. That keeps a single component observable **on its
//! own**, with no orchestrator running (see the design note
//! `design/topics/self-reporting-components.md`).
//!
//! `RunComponent` starts this over its own status watch. The orchestra's
//! supervisor no longer logs owned components (it only escalates); it still logs
//! the *observed* validator, whose state the runtime reports because it cannot
//! run it.

use tokio::sync::watch;
use zaino_component::{CancellationToken, ComponentStatus};

/// Log every transition of `status` at INFO until `cancel` fires or the sender is
/// dropped. The first read logs the current state, then each change.
pub(crate) async fn log_status_transitions(
    mut status: watch::Receiver<ComponentStatus>,
    cancel: CancellationToken,
) {
    loop {
        // Clone out so the watch borrow is dropped before the await.
        let current = status.borrow_and_update().clone();
        tracing::info!(status = %current, "component status");
        tokio::select! {
            _ = cancel.cancelled() => break,
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
}
