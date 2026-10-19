//! Index read lanes: semaphore gate around `spawn_blocking` disk reads
//!
//! - the one bounded wait in the serve path (a page-faulting read waits for its lane's permit)

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Semaphore;

use crate::limits::GrpcLimits;
use crate::metrics;

/// Index read kind, own permit pool each (heavy never queues light): `Point` = one record / tree
/// state (µs warm), `Range` = one `GetBlockRange` window (<= 1 MiB), `Scan` = an address history
/// (bounded by the row budget)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    Point,
    Range,
    Scan,
}

impl Lane {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::Range => "range",
            Self::Scan => "scan",
        }
    }
}

/// The three read lanes, process-wide (clone = share)
#[derive(Clone, Debug)]
pub(crate) struct ReadLanes {
    point: Arc<Semaphore>,
    range: Arc<Semaphore>,
    scan: Arc<Semaphore>,
}

impl ReadLanes {
    pub(crate) fn new(limits: &GrpcLimits) -> Self {
        let lane = |max: NonZeroUsize| Arc::new(Semaphore::new(max.get()));
        Self {
            point: lane(limits.max_point_reads),
            range: lane(limits.max_range_reads),
            scan: lane(limits.max_scan_reads),
        }
    }

    /// `read` on the blocking pool under a `lane` permit (faults mmapped pages: never on a runtime
    /// worker, shared by metrics + other RPCs)
    ///
    /// - a panic in `read` resumes here (a broken invariant: zainod aborts)
    /// - runtime shutting down → `unavailable`
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        lane: Lane,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, tonic::Status> {
        let pool = match lane {
            Lane::Point => &self.point,
            Lane::Range => &self.range,
            Lane::Scan => &self.scan,
        };
        let waiting = Instant::now();
        let _permit = pool.acquire().await.expect("read lanes are never closed");
        metrics::disk_read_waited(lane, waiting.elapsed());
        tokio::task::spawn_blocking(read).await.map_err(|join| match join.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => tonic::Status::unavailable("shutting down"),
        })
    }
}
