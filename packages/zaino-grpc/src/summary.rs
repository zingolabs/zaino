//! Serving summary every [`INTERVAL`] (per-method latency, bytes, codes = the `zaino.grpc.*`
//! metrics)
//!
//! - `Serving`: rate, connections held, then the window's problems by count (silent while nothing
//!   served or held)
//! - `Serving` level: WARN once any request failed, was refused, stalled or ran slow, else INFO
//! - first server fault (ERROR `Request failed`) + first unavailable answer (WARN `Request
//!   unavailable`) of a window logged as they happen, the rest only counted

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tonic::Code;
use tracing::{error, info, warn};

use crate::metrics::{code_label, method_label};
use crate::service::Method;

pub(crate) const INTERVAL: Duration = Duration::from_secs(60);

/// Time to first message past this = a slow request
const SLOW: Duration = Duration::from_secs(1);

/// Code this server answers for (any other = the client's: bad argument, not found, cancelled)
///
/// - `Refused` = not now (capacity, nothing served yet, validator down), `Failed` = this server
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blame {
    Refused,
    Failed,
}

fn blame(code: Code) -> Option<Blame> {
    match code {
        Code::Unavailable | Code::ResourceExhausted => Some(Blame::Refused),
        Code::Unknown | Code::Internal | Code::DataLoss => Some(Blame::Failed),
        _ => None,
    }
}

/// Everything counted since the last summary (`*_logged` = this window's first already logged)
struct Window {
    requests: AtomicU64,
    refused: AtomicU64,
    failed: AtomicU64,
    slow: AtomicU64,
    connections_refused: AtomicU64,
    stalled: AtomicU64,
    failure_logged: AtomicBool,
    unavailable_logged: AtomicBool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    requests: u64,
    refused: u64,
    failed: u64,
    slow: u64,
    connections_refused: u64,
    stalled: u64,
}

impl Window {
    const fn new() -> Self {
        Self {
            requests: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            slow: AtomicU64::new(0),
            connections_refused: AtomicU64::new(0),
            stalled: AtomicU64::new(0),
            failure_logged: AtomicBool::new(false),
            unavailable_logged: AtomicBool::new(false),
        }
    }

    fn finished(&self, code: Code, latency: Option<Duration>) {
        let count = |counter: &AtomicU64| counter.fetch_add(1, Ordering::Relaxed);
        count(&self.requests);
        match blame(code) {
            Some(Blame::Refused) => count(&self.refused),
            Some(Blame::Failed) => count(&self.failed),
            None => 0,
        };
        if latency.is_some_and(|latency| latency > SLOW) {
            count(&self.slow);
        }
    }

    /// Every counter read and zeroed (a request landing mid-swap counts in one window or the next)
    fn take(&self) -> Counts {
        let take = |counter: &AtomicU64| counter.swap(0, Ordering::Relaxed);
        self.failure_logged.store(false, Ordering::Relaxed);
        self.unavailable_logged.store(false, Ordering::Relaxed);
        Counts {
            requests: take(&self.requests),
            refused: take(&self.refused),
            failed: take(&self.failed),
            slow: take(&self.slow),
            connections_refused: take(&self.connections_refused),
            stalled: take(&self.stalled),
        }
    }
}

static WINDOW: Window = Window::new();

/// Stream close-out (`latency` = time to its first message, for an answered work request)
pub(crate) fn finished(code: Code, latency: Option<Duration>) {
    WINDOW.finished(code, latency);
}

pub(crate) fn connection_refused() {
    WINDOW.connections_refused.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn connection_stalled() {
    WINDOW.stalled.fetch_add(1, Ordering::Relaxed);
}

/// A server fault or unavailable answer (never an admission refusal): the window's first of each
/// is logged now, with the message the client got
pub(crate) fn answered_badly(method: Option<Method>, code: Code, message: &str) {
    let first = |logged: &AtomicBool| !logged.swap(true, Ordering::Relaxed);
    let method = method_label(method);
    match blame(code) {
        Some(Blame::Failed) if first(&WINDOW.failure_logged) => {
            error!(method, code = code_label(code), error = message, "Request failed");
        }
        Some(Blame::Refused) if first(&WINDOW.unavailable_logged) => {
            warn!(method, error = message, "Request unavailable");
        }
        _ => {}
    }
}

/// Logs the window since the last call (`elapsed` long, `connections` held now, `at_capacity`
/// admission refusals) and starts the next
pub(crate) fn summarise(elapsed: Duration, connections: usize, at_capacity: u64) {
    let counts = WINDOW.take();
    let problems =
        counts.refused + counts.failed + counts.slow + counts.stalled + counts.connections_refused;
    // no connection = no stream held either
    if counts.requests == 0 && connections == 0 && problems == 0 {
        return;
    }
    // requests × 1 s / window = per second
    let window = elapsed.max(Duration::from_nanos(1));
    let per_s = Duration::from_secs(counts.requests).div_duration_f64(window);
    let rps = (per_s * 100.0).round() / 100.0;
    let nonzero = |count: u64| (count > 0).then_some(count);

    macro_rules! serving {
        ($level:ident) => {
            $level!(
                rps,
                conns = connections,
                failed = nonzero(counts.failed),
                refused = nonzero(counts.refused),
                at_capacity = nonzero(at_capacity),
                slow = nonzero(counts.slow),
                stalled = nonzero(counts.stalled),
                conns_refused = nonzero(counts.connections_refused),
                "Serving"
            )
        };
    }
    match problems {
        0 => serving!(info),
        _ => serving!(warn),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - codes split by who they blame (`NotFound`, `Cancelled` = the client's: uncounted)
    /// - slow = past [`SLOW`] to the first message
    /// - taken = zeroed
    #[test]
    fn a_window_counts_refusals_failures_and_slow_requests() {
        let window = Window::new();
        window.finished(Code::Ok, Some(SLOW));
        window.finished(Code::Ok, Some(SLOW + Duration::from_millis(1)));
        for code in [Code::NotFound, Code::Cancelled, Code::Unavailable, Code::Internal] {
            window.finished(code, None);
        }

        let taken = window.take();

        let expected = Counts {
            requests: 6,
            refused: 1, // Unavailable
            failed: 1,  // Internal
            slow: 1,    // SLOW + 1 ms (SLOW itself is not)
            ..Counts::default()
        };
        assert_eq!(taken, expected, "NotFound + Cancelled = the client's, uncounted");
        assert_eq!(window.take(), Counts::default(), "taken = zeroed");
    }
}
