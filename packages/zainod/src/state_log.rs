//! Readiness → the log: one line per change of `/readyz`'s state or detail, then one every REPEAT
//! while not routed (down, degraded, held)
//!
//! - activity stays with its owners (`progress.rs` syncing, zaino-grpc's serving summary)
//! - level by state: down (overloaded included) = ERROR, held / degraded = WARN, the rest = INFO

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{error, field::display, info, warn, Instrument as _, Level, Span};
use zaino_primitives::display::Human;

use crate::status::{DaemonReason, DaemonState, DaemonStatus, Readiness};

const POLL: Duration = Duration::from_secs(1);
const REPEAT: Duration = Duration::from_secs(60);

/// One line due (`after` = time in the previous state, `None` at start)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Changed { from: Option<DaemonState>, after: Option<Duration> },
    NotReady { elapsed: Duration },
}

/// The state + detail last logged, since when, last line when
#[derive(Debug, Clone, Copy)]
struct Logged {
    state: DaemonState,
    reason: Option<DaemonReason>,
    since: Instant,
    line_at: Instant,
}

#[derive(Debug, Default)]
struct StateLog(Option<Logged>);

impl StateLog {
    /// The line due `now` for readiness `(state, reason)`, if any
    fn step(
        &mut self,
        now: Instant,
        state: DaemonState,
        reason: Option<DaemonReason>,
    ) -> Option<Line> {
        match &mut self.0 {
            Some(logged) if (logged.state, logged.reason) == (state, reason) => {
                let routed =
                    !matches!(state, DaemonState::Down | DaemonState::Degraded | DaemonState::Held);
                if routed || now - logged.line_at < REPEAT {
                    return None;
                }
                logged.line_at = now;
                Some(Line::NotReady { elapsed: now - logged.since })
            }
            previous => {
                let line = Line::Changed {
                    from: previous.map(|logged| logged.state),
                    after: previous.map(|logged| now - logged.since),
                };
                *previous = Some(Logged { state, reason, since: now, line_at: now });
                Some(line)
            }
        }
    }
}

fn level(state: DaemonState) -> Level {
    match state {
        DaemonState::Down => Level::ERROR,
        DaemonState::Held | DaemonState::Degraded => Level::WARN,
        DaemonState::Provisioning | DaemonState::Syncing | DaemonState::Serving => Level::INFO,
    }
}

fn emit(line: Line, readiness: &Readiness, alarms: &[String]) {
    let state = readiness.state().label();
    let detail = readiness.reason().map(DaemonReason::message);
    let notes = (!alarms.is_empty()).then(|| alarms.join(" · "));
    macro_rules! at_level {
        ($($fields:tt)*) => {
            match level(readiness.state()) {
                Level::ERROR => error!($($fields)*),
                Level::WARN => warn!($($fields)*),
                _ => info!($($fields)*),
            }
        };
    }
    match line {
        Line::Changed { from, after } => {
            let (from, after) = (from.map(DaemonState::label), after.map(|d| display(Human(d))));
            at_level!(from, to = state, detail, after, notes, "State changed");
        }
        Line::NotReady { elapsed } => {
            at_level!(state, detail, "for" = %Human(elapsed), notes, "Not ready");
        }
    }
}

/// Every POLL for the process's life: `status`'s readiness → its due line, under the caller's span
pub(crate) fn spawn(status: Arc<DaemonStatus>) {
    let run = async move {
        let mut log = StateLog::default();
        let mut tick = tokio::time::interval(POLL);
        loop {
            tick.tick().await;
            let readiness = status.current_readiness();
            if let Some(line) = log.step(Instant::now(), readiness.state(), readiness.reason()) {
                emit(line, &readiness, &status.alarms());
            }
        }
    };
    tokio::spawn(run.instrument(Span::current()));
}

#[cfg(test)]
mod tests {
    use zaino_grpc::OverloadCause;

    use super::*;

    /// One node's day, second by second where it matters:
    /// - each change (state or detail) logged once at its moment, `after` = time in the last
    /// - routed states (provisioning, syncing, serving) never repeat; down / held every REPEAT
    /// - level: down = ERROR, held / degraded = WARN, the rest = INFO
    #[test]
    fn changes_log_once_and_unrouted_states_repeat_every_minute() {
        use DaemonState::{Degraded, Down, Held, Provisioning, Serving, Syncing};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mins = |m: u64| Duration::from_secs(m * 60);
        let changed = |from, after| Some(Line::Changed { from, after });
        let not_ready = |elapsed| Some(Line::NotReady { elapsed });
        let network = Some(DaemonReason::Overloaded(OverloadCause::NetworkSaturated));
        let cpu = Some(DaemonReason::Overloaded(OverloadCause::CpuPressure));
        let (pending, sent) = (Some(DaemonReason::GoawayPending), Some(DaemonReason::GoawaySent));

        let mut log = StateLog::default();
        let timeline = [
            (at(0), Provisioning, None, changed(None, None)),
            (at(1), Provisioning, None, None),
            (at(600), Syncing, None, changed(Some(Provisioning), Some(mins(10)))),
            (at(1_200), Syncing, None, None),
            (at(1_200), Serving, None, changed(Some(Syncing), Some(mins(10)))),
            (at(4_800), Serving, None, None),
            (at(4_800), Down, network, changed(Some(Serving), Some(mins(60)))),
            (at(4_830), Down, network, None),
            (at(4_860), Down, network, not_ready(mins(1))),
            (at(4_890), Down, network, None),
            (at(4_920), Down, network, not_ready(mins(2))),
            (at(4_940), Down, cpu, changed(Some(Down), Some(Duration::from_secs(140)))),
            (at(5_000), Down, cpu, not_ready(Duration::from_secs(60))),
            (at(5_000), Held, pending, changed(Some(Down), Some(mins(1)))),
            (at(5_010), Held, sent, changed(Some(Held), Some(Duration::from_secs(10)))),
            (at(5_070), Held, sent, not_ready(mins(1))),
            (at(5_130), Serving, None, changed(Some(Held), Some(mins(2)))),
            (at(5_190), Serving, None, None),
        ];
        let logged: Vec<_> =
            timeline.iter().map(|&(now, state, reason, _)| log.step(now, state, reason)).collect();
        let expected: Vec<_> = timeline.iter().map(|&(.., line)| line).collect();
        assert_eq!(logged, expected);

        let levels = [Provisioning, Syncing, Serving, Degraded, Held, Down].map(level);
        let expected =
            [Level::INFO, Level::INFO, Level::INFO, Level::WARN, Level::WARN, Level::ERROR];
        assert_eq!(levels, expected);
    }
}
