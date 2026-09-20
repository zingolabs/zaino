//! The one place a supervised component's run loop is driven to its end.
//!
//! Both the indexer's build loop and a server's serve loop are fallible,
//! long-lived futures whose *terminal outcome* is handled identically: a clean
//! stop goes `Offline`, a returned error or a panic goes `Critical` (with the
//! cause on the status) and is logged. Factored here so neither component
//! re-implements that match — nor has to *remember* to log its own failure.

use std::future::Future;

use tokio::sync::watch;
use zaino_async::catch_panic;
use zaino_component::{error_chain, ComponentName, ComponentStatus, Health, Lifecycle};

/// Run `fut` (a component's run/serve loop) to completion under supervision, then
/// reconcile the terminal status and log any failure:
///
/// * `Ok(())` — a clean stop → `Offline`, no reason.
/// * `Err(e)` — the loop failed → `Critical`, `reason` = the full cause chain
///   ([`error_chain`]), logged at ERROR with `error`/`cause`.
/// * a **panic** — caught by [`catch_panic`] (the run loop is fire-and-forget, so
///   nothing else would observe it) → `Critical`, logged at ERROR; the panic
///   hook already recorded the origin.
///
/// `what` names the loop for the log message (e.g. `"run loop"`, `"serve loop"`).
/// The status *transition* itself is logged generically by the babysitter; this
/// adds only the failure cause a transition line cannot carry.
pub(crate) async fn run_and_reconcile<E, F>(
    name: ComponentName,
    status: &watch::Sender<ComponentStatus>,
    what: &str,
    fut: F,
) where
    E: std::error::Error,
    F: Future<Output = Result<(), E>>,
{
    match catch_panic(fut).await {
        Ok(Ok(())) => status.send_modify(|s| {
            s.lifecycle = Lifecycle::Offline;
            s.health = Health::Offline;
            s.reason = None;
        }),
        Ok(Err(e)) => {
            let chain = error_chain(&e);
            tracing::error!(component = %name, error = %e, cause = %chain, "{what} failed");
            status.send_modify(|s| {
                s.health = Health::Critical;
                s.reason = Some(chain);
            });
        }
        Err(message) => {
            tracing::error!(component = %name, %message, "{what} panicked");
            status.send_modify(|s| {
                s.health = Health::Critical;
                s.reason = Some(format!("{what} panicked: {message}"));
            });
        }
    }
}
