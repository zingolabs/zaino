//! Serving caps + the one bounded wait in the serve path
//!
//! - Every cap refuses, never queues, except [`ReadLanes`] (a page-faulting read waits for its
//!   lane's permit, at most `max_work_streams` waiting)

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use zaino_primitives::serde_secs;

use crate::client::TrustedProxies;
use crate::emit;

/// What one zainod serves at once + which peers name their client (zainod's `[grpc]`)
///
/// - each field's contract: usage.md "Serve stack"
/// - `max_streams_per_connection` = every h2 stream, `max_work_streams` = admitted work only
/// - `unread_timeout` (client stopped reading) != `silent_timeout` (server has nothing to send)
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcLimits {
    pub max_connections: NonZeroUsize,
    pub max_connections_per_ip: NonZeroUsize,
    pub max_streams_per_connection: NonZeroU32,
    pub max_work_streams: NonZeroUsize,
    pub max_subscriptions: NonZeroUsize,
    pub max_point_reads: NonZeroUsize,
    pub max_range_reads: NonZeroUsize,
    pub max_scan_reads: NonZeroUsize,
    #[serde(rename = "unread_timeout_secs", with = "serde_secs::nonzero")]
    pub unread_timeout: Duration,
    #[serde(rename = "silent_timeout_secs", with = "serde_secs::nonzero")]
    pub silent_timeout: Duration,
    #[serde(rename = "max_connection_age_secs", with = "serde_secs::zero_is_never")]
    pub max_connection_age: Option<Duration>,
    #[serde(rename = "max_connection_age_grace_secs", with = "serde_secs::any")]
    pub max_connection_age_grace: Duration,
    #[serde(rename = "max_connection_idle_secs", with = "serde_secs::zero_is_never")]
    pub max_connection_idle: Option<Duration>,
    pub adaptive_work_streams: bool,
    pub trusted_proxies: TrustedProxies,
    #[serde(rename = "shutdown_grace_secs", with = "serde_secs::any")]
    pub drain_timeout: Duration,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        const fn non_zero(value: usize) -> NonZeroUsize {
            NonZeroUsize::new(value).expect("a literal default is non-zero")
        }
        Self {
            max_connections: const { non_zero(4096) },
            max_connections_per_ip: const { non_zero(32) },
            max_streams_per_connection: const { NonZeroU32::new(8).expect("8 is non-zero") },
            max_work_streams: const { non_zero(2048) },
            // one mempool stream per connected wallet
            max_subscriptions: const { non_zero(4096) },
            // µs, warm: CPU-bound, so ~cores in parallel is the useful ceiling
            max_point_reads: const { non_zero(64) },
            // 1 MiB windows: device queue depth
            max_range_reads: const { non_zero(32) },
            // whole address histories, capped per request by rows (few in flight, never many)
            max_scan_reads: const { non_zero(4) },
            // past pepper-sync's longest legitimate backpressure (scanning a fetched batch)
            unread_timeout: Duration::from_secs(300),
            silent_timeout: Duration::from_secs(300),
            // long-held wallet connections re-resolve DNS (gRPC A9)
            max_connection_age: Some(Duration::from_secs(30 * 60)),
            // a whole `GetBlockRange` sync
            max_connection_age_grace: Duration::from_secs(600),
            max_connection_idle: Some(Duration::from_secs(30 * 60)),
            adaptive_work_streams: true,
            trusted_proxies: TrustedProxies::default(),
            drain_timeout: Duration::from_secs(10),
        }
    }
}

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
        emit::disk_read_waited(lane, waiting.elapsed());
        tokio::task::spawn_blocking(read).await.map_err(|join| match join.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => tonic::Status::unavailable("shutting down"),
        })
    }
}
