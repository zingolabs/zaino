//! gRPC egress per tumbling UTC 10-min window, for the public `/status`
//!
//! - counter read at each window boundary (UTC epoch multiple of [`WINDOW`]): rate = bytes since
//!   the last reading over the time between them (a first, partial window measured from boot)
//! - published: Mbit/s, 3 significant figures, < 0.1 = 0 (near-idle windows hidden)
//! - `None` until the first window closes

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::error::IndexerError;

const WINDOW: Duration = Duration::from_secs(600);

/// The last closed window: `start` = its UTC start (unix seconds)
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub(crate) struct Egress {
    pub(crate) start: u64,
    pub(crate) mbps: f64,
}

/// `bytes` over `secs`, as published
fn published_mbps(bytes: u64, secs: f64) -> f64 {
    let mbps = bytes as f64 * 8.0 / secs / 1e6;
    if !mbps.is_finite() || mbps < 0.1 {
        return 0.0;
    }
    let magnitude = 10f64.powi(2 - mbps.log10().floor() as i32);
    (mbps * magnitude).round() / magnitude
}

/// Seconds from `now` (unix seconds) to the next window boundary, and that boundary
fn next_boundary(now: Duration) -> (Duration, u64) {
    let window = WINDOW.as_secs();
    let boundary = (now.as_secs() / window + 1) * window;
    (Duration::from_secs(boundary) - now, boundary)
}

/// Until `cancel`, each closed window into `published`
pub(crate) async fn run(
    published: watch::Sender<Option<Egress>>,
    cancel: CancellationToken,
) -> Result<(), IndexerError> {
    let mut last = (Instant::now(), zaino_grpc::sent_bytes_total());
    loop {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let (wait, boundary) = next_boundary(now);
        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(wait) => {}
        }
        let reading = (Instant::now(), zaino_grpc::sent_bytes_total());
        let secs = (reading.0 - last.0).as_secs_f64();
        let start = boundary - WINDOW.as_secs();
        let mbps = published_mbps(reading.1.saturating_sub(last.1), secs);
        published.send_replace(Some(Egress { start, mbps }));
        last = reading;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - 3 significant figures, under 0.1 Mbit/s hidden as 0
    /// - Boundaries on UTC 10-min multiples
    #[test]
    fn a_window_publishes_three_significant_figures_and_hides_near_idle() {
        let secs = WINDOW.as_secs_f64();
        #[rustfmt::skip]
        let cases = [
            // case,                       bytes over WINDOW, published Mbit/s
            ("idle",                       0,                 0.0),
            ("just under 0.1: hidden",     7_400_000,         0.0),
            ("0.1: shown",                 7_500_000,         0.1),
            ("3 sig figs below 10",        93_812_345,        1.25),
            ("3 sig figs below 100",       938_123_456,       12.5),
            ("3 sig figs past 1000",       93_812_345_678,    1_250.0),
        ];
        for (case, bytes, mbps) in cases {
            assert_eq!(published_mbps(bytes, secs), mbps, "{case}: {bytes} B over 600 s");
        }
        assert_eq!(published_mbps(1_000, 0.0), 0.0, "no time elapsed: nothing to say");

        let (wait, boundary) = next_boundary(Duration::from_secs(1_790_000_000));
        assert_eq!(boundary, 1_790_000_400, "next UTC 10-min multiple after 1_790_000_000");
        assert_eq!(wait, Duration::from_secs(400), "wait until 1_790_000_400");
        let (wait, boundary) = next_boundary(Duration::from_secs(1_790_000_400));
        assert_eq!(boundary, 1_790_001_000, "on a boundary: the next one");
        assert_eq!(wait, WINDOW, "on a boundary: a whole window");
    }
}
