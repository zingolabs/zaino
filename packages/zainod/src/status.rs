//! `/statusz` + `/readyz`: one JSON snapshot of the daemon for the admin thread
//!
//! - Sources filled once by `indexer::boot` (admin thread starts first → `starting` until then)
//! - Render = watch borrows + one chainview `ArcSwap` load (no disk, no locks across awaits)

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::Instant,
};

use serde::Serialize;
use tokio::sync::watch;
use zaino_chainview::ChainViewSubscriber;
use zaino_primitives::types::Height;
use zaino_sync::Reads;

use crate::index_report::Usage;

static SOURCES: OnceLock<Sources> = OnceLock::new();

/// Set once on the shutdown signal, never cleared (the process is exiting)
static DRAINING: AtomicBool = AtomicBool::new(false);

/// From here on `/readyz` fails with `draining` (first reason)
pub(crate) fn drain() {
    DRAINING.store(true, Ordering::Relaxed);
}

pub(crate) fn draining() -> bool {
    DRAINING.load(Ordering::Relaxed)
}

pub(crate) struct Sources {
    pub(crate) network: &'static str,
    pub(crate) started: Instant,
    pub(crate) chainview: ChainViewSubscriber,
    pub(crate) fetched: watch::Receiver<Option<Height>>,
    pub(crate) indexes: Vec<IndexSource>,
    pub(crate) disabled: Vec<&'static str>,
}

/// One enabled index's `Published` watches + its last disk walk (`index_report::run`)
pub(crate) struct IndexSource {
    pub(crate) name: &'static str,
    pub(crate) finalized: watch::Receiver<Option<Height>>,
    pub(crate) applied: watch::Receiver<Option<Height>>,
    pub(crate) merged: watch::Receiver<Option<Height>>,
    pub(crate) synced: watch::Receiver<bool>,
    pub(crate) reads: Option<Reads>,
    pub(crate) usage: watch::Receiver<Option<Usage>>,
}

/// Once per process (a second boot = a bug, ignored)
pub(crate) fn publish(sources: Sources) {
    let _ = SOURCES.set(sources);
}

/// Snapshot bootstrap progress, written by [`crate::snapshot`] before the indexer boots
#[cfg(feature = "snapshot")]
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) phase: Phase,
    /// Host the archive downloads from
    pub(crate) source: String,
    pub(crate) height: u32,
    /// Bytes of `total` handled by `phase`
    pub(crate) done: u64,
    pub(crate) total: u64,
    /// Bytes/s (`downloading` only)
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
static SNAPSHOT: std::sync::Mutex<Option<Snapshot>> = std::sync::Mutex::new(None);

/// `None` = bootstrap over (the indexer's own status takes over)
#[cfg(feature = "snapshot")]
pub(crate) fn snapshot(progress: Option<Snapshot>) {
    // Poison-tolerant: the lock only guards a value swap
    *SNAPSHOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = progress;
}

/// Before boot: `snapshot_<phase>` + its progress while a bootstrap runs, else `starting`
fn not_booted() -> (Vec<String>, Option<serde_json::Value>) {
    #[cfg(feature = "snapshot")]
    if let Some(snapshot) =
        SNAPSHOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    {
        let phase = serde_json::to_value(snapshot.phase).unwrap_or_default();
        let reason = format!("snapshot_{}", phase.as_str().unwrap_or_default());
        return (vec![reason], serde_json::to_value(snapshot).ok());
    }
    (vec!["starting".to_owned()], None)
}

#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct Status {
    version: &'static str,
    network: &'static str,
    uptime_s: u64,
    ready: bool,
    reasons: Vec<String>,
    quorum: Option<Quorum>,
    /// Last block fetched (sync progress between index batch commits)
    fetch_height: Option<u32>,
    validators: Vec<Validator>,
    alarms: Alarms,
    indexes: Vec<Index>,
    grpc: Grpc,
}

#[derive(Debug, Serialize, PartialEq)]
struct Grpc {
    sent_bytes: u64,
}

#[derive(Debug, Serialize, PartialEq)]
struct Quorum {
    height: u32,
    hash: String,
    agreed: usize,
    threshold: usize,
    configured: usize,
}

/// Configured validator (votes) + the p2p peers it reports (telemetry only, chainview §1)
#[derive(Debug, Serialize, PartialEq)]
struct Validator {
    address: String,
    state: &'static str,
    agreement: &'static str,
    height: Option<u32>,
    stale_blocks: Option<u32>,
    latency_ms: Option<u64>,
    failures: u32,
    observed_s_ago: Option<u64>,
    peers: Vec<Peer>,
}

#[derive(Debug, Serialize, PartialEq)]
struct Peer {
    addr: String,
    inbound: bool,
}

#[derive(Debug, Serialize, PartialEq)]
struct Alarms {
    partitioned: bool,
    eclipsed: bool,
    stale: Vec<String>,
}

/// `synced` = serving gate open (built to the quorum tip); disabled indexes listed with nulls
/// - `durable` = on disk; `merged` = held for the next bulk commit; `applied` = highest block
///   served (in-memory view tip, ≥ `durable`)
#[derive(Debug, Clone, Serialize, PartialEq)]
struct Index {
    name: &'static str,
    enabled: bool,
    synced: bool,
    durable: Option<u32>,
    merged: Option<u32>,
    applied: Option<u32>,
    size_bytes: Option<u64>,
    tables: BTreeMap<String, u64>,
    requests: Option<u64>,
}

/// `None` = still starting
pub(crate) fn current(live: bool) -> Option<Status> {
    let sources = SOURCES.get()?;
    let view = sources.chainview.current();
    let endpoints = view.endpoints();
    let tip = *sources.chainview.subscribe_tip().borrow();
    let quorum = sources.chainview.quorum();
    let alarms = view.alarms();

    let validators = endpoints
        .iter()
        .map(|meta| Validator {
            address: meta.address.clone(),
            state: meta.state.label(),
            agreement: meta.agreement.label(),
            height: meta.tip().map(|tip| u32::from(tip.height)),
            stale_blocks: meta.stale_blocks(),
            latency_ms: meta.latency.mean().map(|mean| mean.as_millis() as u64),
            failures: meta.failures,
            observed_s_ago: meta.observed_at.map(|at| at.elapsed().as_secs()),
            peers: meta
                .peers
                .iter()
                .map(|peer| Peer { addr: peer.addr.clone(), inbound: peer.inbound })
                .collect(),
        })
        .collect();

    let enabled = sources.indexes.iter().map(|index| {
        let usage = index.usage.borrow().clone();
        Index {
            name: index.name,
            enabled: true,
            synced: *index.synced.borrow(),
            durable: (*index.finalized.borrow()).map(u32::from),
            merged: (*index.merged.borrow()).map(u32::from),
            applied: (*index.applied.borrow()).map(u32::from),
            size_bytes: usage.as_ref().map(|usage| usage.total),
            tables: usage.map(|usage| usage.subdirs.into_iter().collect()).unwrap_or_default(),
            requests: index.reads.as_ref().map(Reads::total),
        }
    });
    let disabled = sources.disabled.iter().map(|&name| Index {
        name,
        enabled: false,
        synced: false,
        durable: None,
        merged: None,
        applied: None,
        size_bytes: None,
        tables: BTreeMap::new(),
        requests: None,
    });
    let indexes: Vec<Index> = enabled.chain(disabled).collect();

    let reasons = reasons(draining(), live, tip.is_some(), &indexes);
    Some(Status {
        version: env!("CARGO_PKG_VERSION"),
        network: sources.network,
        uptime_s: sources.started.elapsed().as_secs(),
        ready: reasons.is_empty(),
        reasons,
        quorum: tip.map(|tip| Quorum {
            height: u32::from(tip.block.height),
            hash: tip.block.hash.to_string(),
            agreed: tip.agreed_by.count(),
            threshold: quorum.threshold(),
            configured: quorum.configured(),
        }),
        fetch_height: (*sources.fetched.borrow()).map(u32::from),
        alarms: Alarms {
            partitioned: alarms.partitioned(),
            eclipsed: alarms.eclipsed(),
            stale: alarms
                .stale()
                .positions()
                .filter_map(|i| endpoints.get(i))
                .map(|meta| meta.address.clone())
                .collect(),
        },
        validators,
        indexes,
        grpc: Grpc { sent_bytes: zaino_grpc::sent_bytes_total() },
    })
}

/// `/readyz`: `(ready, {"ready", "reasons"})`
pub(crate) fn readiness_json(live: bool) -> (bool, String) {
    let (ready, reasons) = match current(live) {
        Some(status) => (status.ready, status.reasons),
        None => (false, not_booted().0),
    };
    (ready, serde_json::json!({ "ready": ready, "reasons": reasons }).to_string())
}

/// Readiness + a progress fingerprint for [`crate::notify`] (fingerprint moved = startup advanced)
pub(crate) struct Startup {
    pub(crate) ready: bool,
    pub(crate) reasons: Vec<String>,
    pub(crate) progress: Vec<Option<u64>>,
}

/// Booted: fetch height + every index's three heights; before: snapshot phase + bytes done
pub(crate) fn startup(live: bool) -> Startup {
    let Some(status) = current(live) else {
        return Startup { ready: false, reasons: not_booted().0, progress: snapshot_progress() };
    };
    let heights =
        status.indexes.iter().flat_map(|index| [index.durable, index.merged, index.applied]);
    let progress = std::iter::once(status.fetch_height).chain(heights).map(|h| h.map(u64::from));
    Startup { ready: status.ready, reasons: status.reasons, progress: progress.collect() }
}

fn snapshot_progress() -> Vec<Option<u64>> {
    #[cfg(feature = "snapshot")]
    if let Some(snapshot) =
        SNAPSHOT.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
    {
        return vec![Some(snapshot.phase as u64), Some(snapshot.done)];
    }
    Vec::new()
}

/// `/statusz`: everything (`starting` / snapshot stub until boot publishes)
pub(crate) fn status_json(live: bool) -> String {
    match current(live) {
        Some(status) => serde_json::to_string(&status).unwrap_or_default(),
        None => {
            let (reasons, snapshot) = not_booted();
            let mut stub = serde_json::json!({ "ready": false, "reasons": reasons });
            if let Some(snapshot) = snapshot {
                stub["snapshot"] = snapshot;
            }
            stub.to_string()
        }
    }
}

/// Ready = not draining + runtime live + a quorum tip + every enabled index serving
fn reasons(draining: bool, live: bool, quorum_tip: bool, indexes: &[Index]) -> Vec<String> {
    let mut reasons = Vec::new();
    if draining {
        reasons.push("draining".to_owned());
    }
    if !live {
        reasons.push("heartbeat_stale".to_owned());
    }
    if !quorum_tip {
        reasons.push("no_quorum_tip".to_owned());
    }
    reasons.extend(
        indexes
            .iter()
            .filter(|index| index.enabled && !index.synced)
            .map(|index| format!("{}_syncing", index.name)),
    );
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every not-ready cause named (draining first); disabled indexes never block; all clear =
    /// ready
    #[test]
    fn readiness_names_each_blocker() {
        let index = |name, enabled, synced| Index {
            name,
            enabled,
            synced,
            durable: None,
            merged: None,
            applied: None,
            size_bytes: None,
            tables: BTreeMap::new(),
            requests: None,
        };
        let indexes = [
            index("compact_block", true, true),
            index("tree_state", true, false),
            index("transparent_address", false, false),
        ];
        assert_eq!(
            reasons(true, false, false, &indexes),
            ["draining", "heartbeat_stale", "no_quorum_tip", "tree_state_syncing"]
        );
        assert_eq!(reasons(false, true, true, &indexes), ["tree_state_syncing"]);
        let serving = [indexes[0].clone(), indexes[2].clone()];
        assert!(reasons(false, true, true, &serving).is_empty());
        assert_eq!(reasons(true, true, true, &serving), ["draining"], "healthy but draining");
    }
}
