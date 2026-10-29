//! Readiness → the log: one line per change of `/readyz`'s state or detail, then one every REPEAT
//! while it has a reason (down, degraded, held)
//!
//! - activity stays with its owners (`telemetry::progress` syncing, zaino-grpc's serving summary)
//! - level by state: down (overloaded included) = ERROR, held / degraded = WARN, the rest = INFO

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{error, field::display, info, warn, Instrument as _, Level, Span};
use zaino_primitives::display::Human;

use super::{DaemonReason, DaemonStatus, Readiness};

const POLL: Duration = Duration::from_secs(1);
const REPEAT: Duration = Duration::from_secs(60);

/// One line due (`after` = time in the previous state, `None` at start)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Changed { from: Option<&'static str>, after: Option<Duration> },
    NotReady { elapsed: Duration },
}

/// The state + reason last logged, since when, last line when
#[derive(Debug, Clone, Copy)]
struct Logged {
    state: &'static str,
    reason: Option<DaemonReason>,
    since: Instant,
    line_at: Instant,
}

#[derive(Debug, Default)]
struct StateLog(Option<Logged>);

impl StateLog {
    /// The line due `now` for `readiness`, if any
    fn step(&mut self, now: Instant, readiness: &Readiness) -> Option<Line> {
        let (state, reason) = (readiness.label(), readiness.reason());
        match &mut self.0 {
            Some(logged) if (logged.state, logged.reason) == (state, reason) => {
                if reason.is_none() || now - logged.line_at < REPEAT {
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

fn level(readiness: &Readiness) -> Level {
    match readiness {
        Readiness::Down(_) => Level::ERROR,
        Readiness::Held(_) | Readiness::Degraded(_) => Level::WARN,
        Readiness::Provisioning(_) | Readiness::Syncing(_) | Readiness::Serving(_) => Level::INFO,
    }
}

fn emit(line: Line, readiness: &Readiness, alarms: &[String]) {
    let state = readiness.label();
    let detail = readiness.reason().map(DaemonReason::message);
    let notes = (!alarms.is_empty()).then(|| alarms.join(" · "));
    macro_rules! at_level {
        ($($fields:tt)*) => {
            match level(readiness) {
                Level::ERROR => error!($($fields)*),
                Level::WARN => warn!($($fields)*),
                _ => info!($($fields)*),
            }
        };
    }
    match line {
        Line::Changed { from, after } => {
            let after = after.map(|d| display(Human(d)));
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
            if let Some(line) = log.step(Instant::now(), &readiness) {
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
    /// - each change (state or reason) logged once at its moment, `after` = time in the last
    /// - reasonless states (provisioning, syncing, serving) never repeat; down / degraded / held
    ///   every REPEAT
    #[test]
    fn changes_log_once_and_unrouted_states_repeat_every_minute() {
        use Readiness::{Degraded, Down, Held, Provisioning, Serving, Syncing};
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let (secs, mins) = (Duration::from_secs, |m: u64| Duration::from_secs(m * 60));
        let changed = |from, after| Some(Line::Changed { from, after });
        let not_ready = |elapsed| Some(Line::NotReady { elapsed });
        let io = DaemonReason::Overloaded(OverloadCause::IoPressure);
        let cpu = DaemonReason::Overloaded(OverloadCause::CpuPressure);
        let (pending, sent) = (DaemonReason::GoawayPending, DaemonReason::GoawaySent);
        let unvouched = DaemonReason::TipUnvouched;

        let mut log = StateLog::default();
        #[rustfmt::skip]
        let timeline = [
            // case,                               now,       readiness,           line due
            ("boot",                               at(0),     Provisioning(None),  changed(None, None)),
            ("provisioning: never repeats",        at(1),     Provisioning(None),  None),
            ("syncing",                            at(600),   Syncing(None),       changed(Some("provisioning"), Some(mins(10)))),
            ("syncing: never repeats",             at(1_200), Syncing(None),       None),
            ("serving",                            at(1_200), Serving(None),       changed(Some("syncing"), Some(mins(10)))),
            ("serving: never repeats",             at(4_800), Serving(None),       None),
            ("overloaded",                         at(4_800), Down(io),            changed(Some("serving"), Some(mins(60)))),
            ("down: under REPEAT",                 at(4_830), Down(io),            None),
            ("down: REPEAT after the change",      at(4_860), Down(io),            not_ready(mins(1))),
            ("down: under REPEAT since the last",  at(4_890), Down(io),            None),
            ("down: second REPEAT",                at(4_920), Down(io),            not_ready(mins(2))),
            ("down, new reason = a change",        at(4_940), Down(cpu),           changed(Some("down"), Some(secs(140)))),
            ("down: REPEAT after the new reason",  at(5_000), Down(cpu),           not_ready(secs(60))),
            ("held",                               at(5_000), Held(pending),       changed(Some("down"), Some(mins(1)))),
            ("held, GOAWAYs sent = a change",      at(5_010), Held(sent),          changed(Some("held"), Some(secs(10)))),
            ("held: REPEAT",                       at(5_070), Held(sent),          not_ready(mins(1))),
            ("resumed: serving",                   at(5_130), Serving(None),       changed(Some("held"), Some(mins(2)))),
            ("degraded",                           at(5_190), Degraded(unvouched), changed(Some("serving"), Some(mins(1)))),
            ("degraded: REPEAT",                   at(5_250), Degraded(unvouched), not_ready(mins(1))),
            ("vouched again: serving",             at(5_260), Serving(None),       changed(Some("degraded"), Some(secs(70)))),
        ];
        for (case, now, readiness, line) in timeline {
            assert_eq!(log.step(now, &readiness), line, "{case}: {readiness:?}");
        }
    }
}
