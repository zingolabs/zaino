//! systemd service notifications (`Type=notify`); no-ops unless systemd set `NOTIFY_SOCKET`
//!
//! - `READY=1` the first time `/readyz` passes (or the node is held): the start job lasts until
//!   serving at the tip
//! - not ready yet: `EXTEND_TIMEOUT_USEC` per tick that saw progress (past `TimeoutStartSec`,
//!   [`STALL_WINDOW`] without progress = failed start; a slow but moving one never fails)
//! - `STATUS=` = the `/statusz` state (`: detail` when down), on change
//! - shutdown signal: `STOPPING=1` + an extension covering the drain

use std::time::Duration;

use tracing::warn;

use crate::status::Startup;

const TICK: Duration = Duration::from_secs(10);

/// No progress for this long, once past `TimeoutStartSec` = failed start
const STALL_WINDOW: Duration = Duration::from_secs(300);

/// Index flush after the drain (past it, systemd's own `TimeoutStopSec` still applies)
const FLUSH_ALLOWANCE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
enum Message {
    Ready,
    Stopping,
    Status(String),
    ExtendTimeout(Duration),
}

/// Reports startup until the shutdown signal (then [`stopping`] owns the messages)
pub(crate) fn spawn() {
    tokio::spawn(async {
        let mut reporter = Reporter::default();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if crate::status::draining() {
                return;
            }
            send(&reporter.observe(crate::status::startup(crate::admin::live())));
        }
    });
}

/// Shutdown signal received; `drain` = serving delay + connection grace still to come
pub(crate) fn stopping(drain: Duration) {
    send(&[
        Message::Stopping,
        Message::Status("draining".to_owned()),
        Message::ExtendTimeout(drain + FLUSH_ALLOWANCE),
    ]);
}

#[derive(Default)]
struct Reporter {
    started: bool,
    status: String,
    progress: Option<Vec<Option<u64>>>,
}

impl Reporter {
    fn observe(&mut self, now: Startup) -> Vec<Message> {
        let mut messages = Vec::new();
        if now.status != self.status {
            messages.push(Message::Status(now.status.clone()));
            self.status = now.status;
        }
        if self.started {
            return messages;
        }
        if now.started {
            self.started = true;
            messages.push(Message::Ready);
            return messages;
        }
        if self.progress.as_ref().is_some_and(|before| *before != now.progress) {
            messages.push(Message::ExtendTimeout(STALL_WINDOW));
        }
        self.progress = Some(now.progress);
        messages
    }
}

fn send(messages: &[Message]) {
    if messages.is_empty() {
        return;
    }
    #[cfg(unix)]
    {
        use sd_notify::NotifyState;

        let states: Vec<NotifyState<'_>> = messages
            .iter()
            .map(|message| match message {
                Message::Ready => NotifyState::Ready,
                Message::Stopping => NotifyState::Stopping,
                Message::Status(status) => NotifyState::Status(status),
                // u32 µs = 71 min ceiling (saturates)
                Message::ExtendTimeout(by) => NotifyState::ExtendTimeoutUsec(
                    u32::try_from(by.as_micros()).unwrap_or(u32::MAX),
                ),
            })
            .collect();
        if let Err(error) = sd_notify::notify(&states) {
            warn!(%error, "systemd notification failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One startup as systemd sees it: a status line per change, the timeout extended only by a
    /// tick that moved, `READY=1` once, then status changes alone (a later lag is no new start)
    #[test]
    fn startup_extends_only_on_progress_reports_ready_once_then_only_status() {
        let at = |started: bool, status: &str, progress: &[u64]| Startup {
            started,
            status: status.to_owned(),
            progress: progress.iter().copied().map(Some).collect(),
        };
        let status = |text: &str| Message::Status(text.to_owned());
        let extend = Message::ExtendTimeout(STALL_WINDOW);
        let mut reporter = Reporter::default();
        let stalled = "down: Runtime stalled";

        let steps = [
            (at(false, "provisioning", &[0, 10]), vec![status("provisioning")]),
            (at(false, "provisioning", &[0, 90]), vec![extend.clone()]),
            (at(false, "provisioning", &[0, 90]), vec![]),
            (at(false, "provisioning", &[]), vec![extend.clone()]),
            (at(false, "provisioning", &[]), vec![]),
            (at(false, "syncing", &[7, 5, 5]), vec![status("syncing"), extend.clone()]),
            (at(true, "serving", &[9, 9, 9]), vec![status("serving"), Message::Ready]),
            (at(false, stalled, &[9, 9, 9]), vec![status(stalled)]),
            (at(false, stalled, &[12, 9, 9]), vec![]),
            (at(true, "serving", &[12, 12, 12]), vec![status("serving")]),
        ];
        for (step, (now, expected)) in steps.into_iter().enumerate() {
            assert_eq!(reporter.observe(now), expected, "step {step}");
        }

        // started held, mid-sync: the start job ends there (the hold withholds readiness)
        let mut held = Reporter::default();
        assert_eq!(held.observe(at(false, "syncing", &[3])), vec![status("syncing")]);
        let expected = vec![status("held"), Message::Ready];
        assert_eq!(held.observe(at(true, "held", &[3])), expected, "held = started");
        assert_eq!(held.observe(at(false, "syncing", &[4])), vec![status("syncing")], "resumed");
    }
}
