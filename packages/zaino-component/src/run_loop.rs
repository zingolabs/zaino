//! The run seam — the one driven port a supervised, long-running component
//! brings.
//!
//! An index writer and a network server are the same shape: a long-lived,
//! fallible loop that binds/starts, signals readiness once, and runs until
//! cancelled. [`RunLoop`] is that single contract; the runtime's generic run
//! component drives it and reconciles the outcome to the component's status.

use core::future::Future;
use std::sync::Arc;

use crate::{CancellationToken, Lifecycle};

/// Fired by a run loop once it has **started serving its purpose** — a server
/// bound to its socket, a writer caught up to the tip — so the component reports
/// `Ready` only when it is genuinely useful, not optimistically at spawn. If the
/// loop returns `Err` before firing it, the component never becomes `Ready` (a
/// boot failure), which is the honest outcome.
pub struct ReadySignal {
    on_ready: Box<dyn FnOnce() + Send>,
}

impl ReadySignal {
    /// A signal that runs `on_ready` when the loop reports it is ready.
    pub fn new(on_ready: impl FnOnce() + Send + 'static) -> Self {
        Self {
            on_ready: Box::new(on_ready),
        }
    }

    /// Report that the loop has reached its ready condition.
    pub fn notify(self) {
        (self.on_ready)()
    }
}

/// A supervised, long-running loop the runtime drives — an index writer's build
/// loop or a server's serve loop, unified.
///
/// `run` starts the loop, fires `ready` once it reaches its ready condition
/// (bound / caught-up), and runs until `cancel` fires. `Ok(())` is a clean stop
/// in response to the token; `Err` is a failure — which a supervisor turns into a
/// `Critical` component (and, if `ready` never fired, a boot failure).
///
/// This is a *driven* port: the concrete writer (e.g. ChainView's sync) or server
/// (tonic / jsonrpsee) implements it, and the runtime's run component drives it.
/// It lives here, with the component vocabulary, so a domain crate can implement
/// it depending only on `zaino-component`, never on the runtime. The receiver is
/// `Arc<Self>` so the loop can be shared with the spawned task while the
/// component retains a handle.
pub trait RunLoop: Send + Sync + 'static {
    /// Why the loop could not start or keep running.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Names the loop in logs and status, e.g. `"run loop"` / `"serve loop"`.
    const LABEL: &'static str;

    /// The lifecycle phase while the loop is running-but-not-yet-`Ready`:
    /// [`Lifecycle::Syncing`] for a writer that already serves dependents while it
    /// catches up, [`Lifecycle::Spawning`] for a server that must bind before it
    /// is `Ready`.
    const RUNNING: Lifecycle;

    /// Start the loop, fire `ready` once at its ready condition, and run until
    /// `cancel` fires.
    fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        ready: ReadySignal,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
