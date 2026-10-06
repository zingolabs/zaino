//! Serving caps, and the one bounded wait in the serve path.
//!
//! Every cap refuses rather than queues, except the [`ReadLanes`]: a read that must fault index
//! pages in waits for a permit of its lane, and at most `max_streams` streams can be waiting.

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::emit;

/// What one zainod will serve at once.
///
/// - `stall_timeout`: a stream holding data its client has not pulled for this long closes
///   its connection (a live peer that never reads would keep every permit it holds)
/// - `drain_timeout`: after `run`'s cancel, how long open connections get to finish their
///   in-flight streams (GOAWAY sent) before `run` returns and they are dropped (zero = at once)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrpcLimits {
    pub max_connections: NonZeroUsize,
    pub max_connections_per_ip: NonZeroUsize,
    pub max_streams_per_connection: NonZeroU32,
    pub max_streams: NonZeroUsize,
    pub max_subscriptions: NonZeroUsize,
    pub max_point_reads: NonZeroUsize,
    pub max_range_reads: NonZeroUsize,
    pub max_scan_reads: NonZeroUsize,
    pub stall_timeout: Duration,
    pub drain_timeout: Duration,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        let non_zero = |value| NonZeroUsize::new(value).expect("a literal default is non-zero");

        Self {
            max_connections: non_zero(4096),
            max_connections_per_ip: non_zero(32),
            max_streams_per_connection: NonZeroU32::new(8).expect("8 is non-zero"),
            max_streams: non_zero(2048),
            // one mempool stream per connected wallet
            max_subscriptions: non_zero(4096),
            // µs, warm: CPU-bound, so ~cores in parallel is the useful ceiling
            max_point_reads: non_zero(64),
            // 1 MiB windows: device queue depth
            max_range_reads: non_zero(32),
            // whole address histories, capped per request by rows (few in flight, never many)
            max_scan_reads: non_zero(4),
            // past pepper-sync's longest legitimate backpressure (scanning a fetched batch)
            stall_timeout: Duration::from_secs(300),
            // opt-in (zainod `[grpc.shutdown]`)
            drain_timeout: Duration::ZERO,
        }
    }
}

/// Which kind of index read, each its own permit pool (a heavy kind never queues a light one)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    /// One record or tree state: bounded, µs warm
    Point,
    /// One `GetBlockRange` window (≤ 1 MiB)
    Range,
    /// An address history (bounded by the row budget, not by the request)
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
pub struct ReadLanes {
    point: Arc<Semaphore>,
    range: Arc<Semaphore>,
    scan: Arc<Semaphore>,
}

impl ReadLanes {
    pub fn new(limits: &GrpcLimits) -> Self {
        let lane = |max: NonZeroUsize| Arc::new(Semaphore::new(max.get()));
        Self {
            point: lane(limits.max_point_reads),
            range: lane(limits.max_range_reads),
            scan: lane(limits.max_scan_reads),
        }
    }

    /// Waits for a permit of `lane`, recording the wait.
    pub(crate) async fn acquire(&self, lane: Lane) -> ReadPermit {
        let pool = match lane {
            Lane::Point => &self.point,
            Lane::Range => &self.range,
            Lane::Scan => &self.scan,
        };
        let waiting = Instant::now();
        let permit = Arc::clone(pool).acquire_owned().await.expect("read lanes are never closed");
        emit::disk_read_waited(lane, waiting.elapsed());

        ReadPermit { _permit: permit }
    }

    /// `read` on the blocking pool under a permit of `lane`: an index read faults mmapped pages
    /// and walks segments, never on a runtime worker (metrics, other RPCs and the followers share
    /// them)
    ///
    /// - a panic in `read` resumes here (a broken invariant: zainod aborts)
    /// - runtime shutting down → `unavailable`
    pub(crate) async fn read<T: Send + 'static>(
        &self,
        lane: Lane,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, tonic::Status> {
        let _permit = self.acquire(lane).await;
        tokio::task::spawn_blocking(read).await.map_err(|join| match join.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => tonic::Status::unavailable("shutting down"),
        })
    }
}

/// Held for one read; released on drop.
#[derive(Debug)]
pub(crate) struct ReadPermit {
    _permit: OwnedSemaphorePermit,
}
