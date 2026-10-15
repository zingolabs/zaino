//! systemd start notifications (`Type=notify`)
//!
//! - No-ops unless systemd set `NOTIFY_SOCKET`
//! - `READY=1` the first time `/readyz` passes (or the node is held): the start job lasts until
//!   serving at the tip, then nothing more is sent
//! - not ready yet: `EXTEND_TIMEOUT_USEC` per tick that saw progress
//! - past `TimeoutStartSec`: [`STALL_WINDOW`] without progress = failed start (slow but moving
//!   start never fails)

use std::{sync::Arc, time::Duration};

use tracing::warn;

use crate::status::{DaemonStatus, Startup};

const TICK: Duration = Duration::from_secs(10);

/// No progress for this long, once past `TimeoutStartSec` = failed start
const STALL_WINDOW: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
enum Message {
    Ready,
    ExtendTimeout(Duration),
}

/// Reports startup until `READY=1`
pub(crate) fn spawn(status: Arc<DaemonStatus>) {
    tokio::spawn(async move {
        let mut reporter = Reporter::default();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            let message = reporter.observe(status.startup());
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
    progress: Option<Vec<Option<u64>>>,
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
    #[cfg(unix)]
    {
        use sd_notify::NotifyState;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One startup as systemd sees it: the timeout extended only by a tick that moved, then
    /// `READY=1` once serving or held (a node started held must not fail its start job)
    #[test]
    fn startup_extends_only_on_progress_then_reports_ready() {
        let at = |started: bool, progress: &[u64]| Startup {
            started,
            progress: progress.iter().copied().map(Some).collect(),
        };
        let extend = Some(Message::ExtendTimeout(STALL_WINDOW));
        let mut reporter = Reporter::default();

        let steps = [
            (at(false, &[0, 10]), None),
            (at(false, &[0, 90]), extend.clone()),
            (at(false, &[0, 90]), None),
            (at(false, &[]), extend.clone()),
            (at(false, &[]), None),
            (at(false, &[7, 5, 5]), extend),
            (at(true, &[9, 9, 9]), Some(Message::Ready)),
        ];
        for (step, (now, expected)) in steps.into_iter().enumerate() {
            assert_eq!(reporter.observe(now), expected, "step {step}");
        }
    }
}
