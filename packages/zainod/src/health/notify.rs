//! systemd start notifications (`Type=notify`)
//!
//! - No-ops unless systemd set `NOTIFY_SOCKET`
//! - `READY=1` the first time `/readyz` passes (or the node is held): the start job lasts until
//!   serving at the tip, then nothing more is sent
//! - not ready yet: `EXTEND_TIMEOUT_USEC` per tick that saw progress
//! - past `TimeoutStartSec`: [`STALL_WINDOW`] without progress = failed start (slow but moving
//!   start never fails)

use std::{sync::Arc, time::Duration};

use sd_notify::NotifyState;
use tracing::warn;

use super::{DaemonStatus, Readiness};
use crate::daemon::index_snapshot::SnapshotPhase;

const TICK: Duration = Duration::from_secs(10);

/// No progress for this long, once past `TimeoutStartSec` = failed start
const STALL_WINDOW: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
enum Message {
    Ready,
    ExtendTimeout(Duration),
}

/// Readiness + a progress fingerprint (fingerprint moved = startup advanced)
///
/// - `started` = serving or held (a node started held must not fail its start job waiting on a
///   readiness the operator withholds)
struct Startup {
    started: bool,
    progress: Progress,
}

/// - `Starting` = bootstrap phase + bytes (`None` = no bootstrap running)
/// - `Booted` = handed + served heights + every index's durable one
#[derive(Debug, Clone, PartialEq)]
enum Progress {
    Starting(Option<(SnapshotPhase, u64)>),
    Booted(Vec<Option<u32>>),
}

fn startup(status: &DaemonStatus) -> Startup {
    let readiness = status.current_readiness();
    let started = readiness.ready() || matches!(readiness, Readiness::Held(_));
    let Some(sources) = status.sources.get() else {
        let bootstrap = status.current_bootstrap().map(|at| (at.phase, at.bytes));
        return Startup { started, progress: Progress::Starting(bootstrap) };
    };
    let grpc_snapshot = sources.grpc_snapshots.load();
    let durable = grpc_snapshot.indexed().into_iter().flat_map(|indexed| indexed.durable());
    let durable = durable.map(|(_, tip)| tip.map(|tip| tip.height));
    let heights = [sources.progress.handed(), grpc_snapshot.tips().served.map(|tip| tip.height)];
    let heights = heights.into_iter().chain(durable).map(|h| h.map(u32::from));
    Startup { started, progress: Progress::Booted(heights.collect()) }
}

/// Reports startup until `READY=1`
pub(crate) fn spawn(status: Arc<DaemonStatus>) {
    tokio::spawn(async move {
        let mut reporter = Reporter::default();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            let message = reporter.observe(startup(&status));
            if let Some(message) = &message {
                send(message);
            }
            if message == Some(Message::Ready) {
                return;
            }
        }
    });
}

#[derive(Default)]
struct Reporter {
    progress: Option<Progress>,
}

impl Reporter {
    fn observe(&mut self, now: Startup) -> Option<Message> {
        if now.started {
            return Some(Message::Ready);
        }
        let moved = self.progress.as_ref().is_some_and(|before| *before != now.progress);
        self.progress = Some(now.progress);
        moved.then_some(Message::ExtendTimeout(STALL_WINDOW))
    }
}

fn send(message: &Message) {
    let state = match message {
        Message::Ready => NotifyState::Ready,
        // u32 µs = 71 min ceiling (saturates)
        Message::ExtendTimeout(by) => {
            NotifyState::ExtendTimeoutUsec(u32::try_from(by.as_micros()).unwrap_or(u32::MAX))
        }
    };
    if let Err(error) = sd_notify::notify(&[state]) {
        warn!(%error, "systemd notification failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One startup as systemd sees it: the timeout extended only by a tick that moved, then
    /// `READY=1` once serving or held (a node started held must not fail its start job)
    #[test]
    fn startup_extends_only_on_progress_then_reports_ready() {
        use SnapshotPhase::{Downloading, Verifying};
        let starting = |at| Startup { started: false, progress: Progress::Starting(at) };
        let booted = |started, heights: &[u32]| Startup {
            started,
            progress: Progress::Booted(heights.iter().copied().map(Some).collect()),
        };
        let extend = Some(Message::ExtendTimeout(STALL_WINDOW));
        let mut reporter = Reporter::default();

        #[rustfmt::skip]
        let ticks = [
            // case,                                       tick's startup,                   message sent
            ("first tick: nothing to compare against",     starting(Some((Downloading, 10))), None),
            ("bootstrap bytes moved",                      starting(Some((Downloading, 90))), extend.clone()),
            ("no progress",                                starting(Some((Downloading, 90))), None),
            ("phase moved, bytes alike",                   starting(Some((Verifying, 90))),   extend.clone()),
            ("bootstrap over, pipeline not up: changed",   starting(None),                    extend.clone()),
            ("still nothing",                              starting(None),                    None),
            ("pipeline booted: handed, served, durable",   booted(false, &[7, 5, 5]),         extend),
            ("serving or held",                            booted(true, &[9, 9, 9]),          Some(Message::Ready)),
        ];
        for (case, now, expected) in ticks {
            assert_eq!(reporter.observe(now), expected, "{case}");
        }
    }
}
