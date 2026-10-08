//! `/statusz`, `/readyz` + the `/metrics` gauges: each from one global snapshot load (G9)
//!
//! - Sources filled once by `indexer::boot` (admin thread starts first → `starting` until then)
//! - `/statusz` = `zaino_snapshot::Report` + process fields; owners' live tables read at request

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};

use serde::Serialize;
use tokio::sync::watch;
use zaino_persistence::{DiskView, IndexKind};
use zaino_snapshot::{Report, Snapshots};
use zaino_sync::SyncProgress;
use zaino_traffic::MemberTable;

use crate::progress::Disk;

static SOURCES: OnceLock<Sources> = OnceLock::new();

/// Config's enabled indexes, set before the snapshot bootstrap (stub + [`Report`] read one set)
static CONFIGURED: OnceLock<Vec<IndexKind>> = OnceLock::new();

/// Once per process, right after config validation
pub(crate) fn configure(kinds: impl IntoIterator<Item = IndexKind>) {
    let _ = CONFIGURED.set(kinds.into_iter().collect());
}

fn configured() -> &'static [IndexKind] {
    CONFIGURED.get().map_or(&[], Vec::as_slice)
}

/// Set once on the shutdown signal, never cleared (the process is exiting)
static DRAINING: AtomicBool = AtomicBool::new(false);

/// From here on `/readyz` fails with `draining` (first reason)
pub(crate) fn drain() {
    DRAINING.store(true, Ordering::Relaxed);
}

pub(crate) fn draining() -> bool {
    DRAINING.load(Ordering::Relaxed)
}

/// `members` = the balancer's (latency, failures, health); `disk` = the progress task's last walk
pub(crate) struct Sources {
    pub(crate) network: &'static str,
    pub(crate) started: Instant,
    pub(crate) snapshots: Snapshots<DiskView>,
    pub(crate) progress: SyncProgress,
    pub(crate) members: watch::Receiver<Arc<MemberTable>>,
    pub(crate) disk: watch::Receiver<Disk>,
}

/// Once per process (a second boot = a bug, ignored)
pub(crate) fn publish(sources: Sources) {
    let _ = SOURCES.set(sources);
}

/// Archive bootstrap progress, written by [`crate::bootstrap`] before the indexer boots
///
/// - `source` = archive host; `indexes` archives fetched, `total` = Σ their bytes
/// - `phase` = slowest archive's, `done` = its bytes (archives past it whole); `rate` = bytes/s
///   (`downloading` only)
#[cfg(feature = "snapshot")]
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Bootstrap {
    pub(crate) phase: Phase,
    pub(crate) source: String,
    pub(crate) height: u32,
    pub(crate) indexes: usize,
    pub(crate) done: u64,
    pub(crate) total: u64,
    pub(crate) rate: Option<u64>,
}

#[cfg(feature = "snapshot")]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Downloading,
    Verifying,
    Unpacking,
}

#[cfg(feature = "snapshot")]
static BOOTSTRAP: std::sync::Mutex<Option<Bootstrap>> = std::sync::Mutex::new(None);

/// `None` = bootstrap over (the indexer's own status takes over)
#[cfg(feature = "snapshot")]
pub(crate) fn bootstrap(progress: Option<Bootstrap>) {
    // Poison-tolerant: the lock only guards a value swap
    *BOOTSTRAP.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = progress;
}

/// Before boot: `snapshot_<phase>` + its progress while a bootstrap runs, else `starting`
pub(crate) fn not_booted() -> (Vec<String>, Option<serde_json::Value>) {
    #[cfg(feature = "snapshot")]
    if let Some(bootstrap) =
        BOOTSTRAP.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    {
        let phase = serde_json::to_value(bootstrap.phase).unwrap_or_default();
        let reason = format!("snapshot_{}", phase.as_str().unwrap_or_default());
        return (vec![reason], serde_json::to_value(bootstrap).ok());
    }
    (vec!["starting".to_owned()], None)
}

/// `/statusz` body: process fields + the snapshot's [`Report`] (field meanings: docs/running.md)
#[derive(Debug, Serialize)]
pub(crate) struct Status {
    version: &'static str,
    network: &'static str,
    uptime_s: u64,
    ready: bool,
    reasons: Vec<String>,
    grpc: Grpc,
    disk: Disk,
    #[serde(flatten)]
    report: Report,
}

#[derive(Debug, Serialize)]
struct Grpc {
    sent_bytes: u64,
}

/// `None` = still starting
fn current(live: bool) -> Option<Status> {
    let sources = SOURCES.get()?;
    let snap = sources.snapshots.load();
    let reasons = reasons(draining(), live, snap.unready().map(|reason| reason.label()));
    let members = sources.members.borrow().clone();
    Some(Status {
        version: env!("CARGO_PKG_VERSION"),
        network: sources.network,
        uptime_s: sources.started.elapsed().as_secs(),
        ready: reasons.is_empty(),
        reasons,
        grpc: Grpc { sent_bytes: zaino_grpc::sent_bytes_total() },
        disk: sources.disk.borrow().clone(),
        report: Report::of(&snap, &sources.progress, &members, configured()),
    })
}

/// `/readyz`: `(ready, {"ready", "reasons"})`
pub(crate) fn readiness_json(live: bool) -> (bool, String) {
    let reasons = match SOURCES.get() {
        Some(sources) => {
            let snap = sources.snapshots.load();
            reasons(draining(), live, snap.unready().map(|reason| reason.label()))
        }
        None => not_booted().0,
    };
    let ready = reasons.is_empty();
    (ready, serde_json::json!({ "ready": ready, "reasons": reasons }).to_string())
}

/// Every snapshot gauge, set just before a scrape renders (nothing before boot)
pub(crate) fn emit_gauges() {
    if let Some(sources) = SOURCES.get() {
        zaino_snapshot::emit_gauges(&sources.snapshots.load(), &sources.progress);
    }
}

/// Readiness + a progress fingerprint for [`crate::notify`] (fingerprint moved = startup advanced)
pub(crate) struct Startup {
    pub(crate) ready: bool,
    pub(crate) reasons: Vec<String>,
    pub(crate) progress: Vec<Option<u64>>,
}

/// Booted: handed + served heights + every index's durable one; before: bootstrap phase + bytes
pub(crate) fn startup(live: bool) -> Startup {
    let Some(sources) = SOURCES.get() else {
        return Startup { ready: false, reasons: not_booted().0, progress: bootstrap_progress() };
    };
    let snap = sources.snapshots.load();
    let reasons = reasons(draining(), live, snap.unready().map(|reason| reason.label()));
    let durable = snap.indexed().into_iter().flat_map(|indexed| indexed.durable());
    let durable = durable.map(|(_, tip)| tip.map(|tip| tip.height));
    let heights = [sources.progress.handed(), snap.tips().served.map(|tip| tip.height)];
    let progress = heights.into_iter().chain(durable).map(|h| h.map(|h| u64::from(u32::from(h))));
    Startup { ready: reasons.is_empty(), reasons, progress: progress.collect() }
}

fn bootstrap_progress() -> Vec<Option<u64>> {
    #[cfg(feature = "snapshot")]
    if let Some(bootstrap) =
        BOOTSTRAP.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
    {
        return vec![Some(bootstrap.phase as u64), Some(bootstrap.done)];
    }
    Vec::new()
}

/// `/statusz`: everything (`starting` / bootstrap stub + configured `indexes` until boot publishes)
pub(crate) fn status_json(live: bool) -> String {
    match current(live) {
        Some(status) => serde_json::to_string(&status).unwrap_or_default(),
        None => {
            let (reasons, bootstrap) = not_booted();
            stub(reasons, bootstrap, configured()).to_string()
        }
    }
}

/// Before boot: readiness, the configured `indexes` (no `durable` yet), bootstrap progress if any
fn stub(
    reasons: Vec<String>,
    bootstrap: Option<serde_json::Value>,
    configured: &[IndexKind],
) -> serde_json::Value {
    let indexes = zaino_snapshot::indexes(configured, &[]);
    let mut stub = serde_json::json!({ "ready": false, "reasons": reasons, "indexes": indexes });
    if let Some(bootstrap) = bootstrap {
        stub["snapshot"] = bootstrap;
    }
    stub
}

/// Ready = not draining + runtime live + the snapshot's own reasons (`Unready`, in order)
fn reasons<'a>(draining: bool, live: bool, unready: impl Iterator<Item = &'a str>) -> Vec<String> {
    let process = [(draining, "draining"), (!live, "heartbeat_stale")];
    let process = process.into_iter().filter(|(raised, _)| *raised).map(|(_, reason)| reason);
    process.chain(unready).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process reasons first (draining, then a stale heartbeat), then the snapshot's
    #[test]
    fn readiness_names_each_blocker_process_first() {
        let unready = || ["headers_syncing", "syncing"].into_iter();
        let all = reasons(true, false, unready());
        assert_eq!(all, ["draining", "heartbeat_stale", "headers_syncing", "syncing"]);
        assert_eq!(reasons(false, true, unready()), ["headers_syncing", "syncing"]);
        assert!(reasons(false, true, std::iter::empty()).is_empty());
        assert_eq!(reasons(true, true, std::iter::empty()), ["draining"], "healthy but draining");
    }

    /// Before boot `/statusz` already lists every foldable index as configured (compact-block
    /// pair on, the rest off), then the bootstrap progress verbatim when one runs
    #[test]
    fn pre_boot_stub_lists_configured_indexes_and_bootstrap_progress() {
        let configured = [IndexKind::CompactBlock, IndexKind::ValueBalance];
        let index = |name: &str, enabled: bool| serde_json::json!({ "name": name, "enabled": enabled, "durable": null });
        let indexes = serde_json::json!([
            index("value_balance", true),
            index("compact_block", true),
            index("block_hash", false),
            index("tree_state", false),
            index("transparent_address", false),
        ]);
        assert_eq!(
            stub(vec!["starting".to_owned()], None, &configured),
            serde_json::json!({ "ready": false, "reasons": ["starting"], "indexes": indexes }),
        );
        let progress = serde_json::json!({ "phase": "downloading", "indexes": 2, "total": 9 });
        assert_eq!(
            stub(vec!["snapshot_downloading".to_owned()], Some(progress.clone()), &configured),
            serde_json::json!({
                "ready": false, "reasons": ["snapshot_downloading"], "indexes": indexes,
                "snapshot": progress,
            }),
        );
    }
}
