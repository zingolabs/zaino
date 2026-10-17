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

/// What a run loop reports back to its component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunReport {
    /// The loop has reached its ready condition — a server bound to its socket, a
    /// writer caught up to the tip — so the component reports `Ready` only when it
    /// is genuinely useful, not optimistically at spawn.
    Ready,
    /// The loop's current position toward a target (e.g. a writer's committed
    /// height toward the chain tip); `target` is `None` when not yet known.
    Progress {
        /// How far the loop has got.
        current: u64,
        /// The target it is working toward, if known.
        target: Option<u64>,
    },
}

/// The handle a run loop uses to report readiness and progress to its component.
///
/// [`ready`](Self::ready) is idempotent (the first call transitions the component
/// to `Ready`; if the loop returns `Err` before ever calling it, the component
/// never becomes `Ready` — a boot failure, the honest outcome).
/// [`progress`](Self::progress) is repeatable. `Clone` so a loop can share it
/// with, say, a progress poller running alongside the main work.
#[derive(Clone)]
pub struct RunReporter {
    report: std::sync::Arc<dyn Fn(RunReport) + Send + Sync>,
}

impl RunReporter {
    /// A reporter that runs `report` for each [`RunReport`] the loop emits.
    pub fn new(report: impl Fn(RunReport) + Send + Sync + 'static) -> Self {
        Self {
            report: std::sync::Arc::new(report),
        }
    }

    /// Report that the loop has reached its ready condition.
    pub fn ready(&self) {
        (self.report)(RunReport::Ready)
    }

    /// Report the loop's current position toward `target` (if known).
    pub fn progress(&self, current: u64, target: Option<u64>) {
        (self.report)(RunReport::Progress { current, target })
    }
}

/// A supervised, long-running loop the runtime drives — an index writer's build
/// loop or a server's serve loop, unified.
///
/// `run` starts the loop, uses `reporter` to signal readiness once it reaches its
/// ready condition (bound / caught-up) and to report progress along the way, and
/// runs until `cancel` fires. `Ok(())` is a clean stop in response to the token;
/// `Err` is a failure — which a supervisor turns into a `Critical` component
/// (and, if `reporter.ready()` never fired, a boot failure).
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

    /// Start the loop, call `reporter.ready()` once at its ready condition (and
    /// `reporter.progress(..)` as it advances), and run until `cancel` fires.
    fn run(
        self: Arc<Self>,
        cancel: CancellationToken,
        reporter: RunReporter,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
