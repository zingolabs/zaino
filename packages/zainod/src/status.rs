//! `/statusz` + `/readyz`: one JSON snapshot of the daemon for the admin thread
//!
//! - Sources filled once by `indexer::boot` (admin thread starts first → `starting` until then)
//! - Render = watch borrows + one chainview `ArcSwap` load (no disk, no locks across awaits)

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Instant,
};

use serde::Serialize;
use tokio::sync::watch;
use zaino_chainview::ChainViewSubscriber;
use zaino_nfs::NfsHandle;
use zaino_persistence::{DiskView, View as _};
use zaino_primitives::types::{self, Height};
use zaino_traffic::{Health, MemberTable};

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

/// `handed` = the NFS's last block handed to the indexes; `served` = its snapshots; `synced` =
/// [`crate::serving`]'s judgement; `members` = the balancer's (latency, failures; trusted first)
pub(crate) struct Sources {
    pub(crate) network: &'static str,
    pub(crate) started: Instant,
    pub(crate) chainview: ChainViewSubscriber,
    pub(crate) members: watch::Receiver<Arc<MemberTable>>,
    pub(crate) handed: watch::Receiver<Option<Height>>,
    pub(crate) served: NfsHandle<DiskView>,
    pub(crate) synced: watch::Receiver<bool>,
    pub(crate) indexes: Vec<IndexSource>,
    pub(crate) disabled: Vec<&'static str>,
}

/// One enabled index's committed view + its last disk walk (`index_report::run`)
pub(crate) struct IndexSource {
    pub(crate) name: &'static str,
    pub(crate) committed: watch::Receiver<DiskView>,
    pub(crate) usage: watch::Receiver<Option<Usage>>,
}

/// Once per process (a second boot = a bug, ignored)
pub(crate) fn publish(sources: Sources) {
    let _ = SOURCES.set(sources);
}

/// Snapshot bootstrap progress, written by [`crate::snapshot`] before the indexer boots
///
/// - `source` = archive host; `done` = bytes of `total` handled by `phase`; `rate` = bytes/s
///   (`downloading` only)
#[cfg(feature = "snapshot")]
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) phase: Phase,
    pub(crate) source: String,
    pub(crate) height: u32,
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

/// `/statusz` body (field meanings: usage.md "The admin listener")
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct Status {
    version: &'static str,
    network: &'static str,
    uptime_s: u64,
    ready: bool,
    reasons: Vec<String>,
    tip: Option<Tip>,
    best_height: Option<u32>,
    fetch_height: Option<u32>,
    served_height: Option<u32>,
    synced: bool,
    validators: Vec<Validator>,
    alarms: Alarms,
    mempool: Mempool,
    indexes: Vec<Index>,
    grpc: Grpc,
}

#[derive(Debug, Serialize, PartialEq)]
struct Grpc {
    sent_bytes: u64,
}

#[derive(Debug, Serialize, PartialEq)]
struct Tip {
    height: u32,
    hash: String,
    held_by: usize,
    configured: usize,
}

/// Configured validator (may hold the tip) + its reported p2p peers (telemetry only, chainview §1)
///
/// - `streaming` = push streams up (polling at reconcile cadence); `release` = `None` until the
///   first metadata read answers
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
    streaming: bool,
    release: Option<Release>,
    peers: Vec<Peer>,
}

#[derive(Debug, Serialize, PartialEq)]
struct Release {
    build: String,
    user_agent: String,
    protocol_version: u32,
    end_of_service: EndOfService,
}

/// `blocks_left` = from the validator's own tip
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
enum EndOfService {
    At { height: u32, estimated_unix: i64, blocks_left: Option<u32> },
    NotEnforced,
    Unknown,
}

#[derive(Debug, Serialize, PartialEq)]
struct Peer {
    addr: String,
    inbound: bool,
}

/// `ending` = validators whose release halts within a week of their tip
#[derive(Debug, Serialize, PartialEq)]
struct Alarms {
    partitioned: bool,
    eclipsed: bool,
    stale: Vec<String>,
    ending: Vec<String>,
}

/// `transactions` = held (servable or not); `trusted_readers` = the `y` of `trusted: x/y`
#[derive(Debug, Serialize, PartialEq)]
struct Mempool {
    transactions: usize,
    verified: usize,
    ours_unverified: usize,
    fully_spread: usize,
    trusted_readers: usize,
}

/// `durable` = committed height; disabled indexes listed with nulls
#[derive(Debug, Clone, Serialize, PartialEq)]
struct Index {
    name: &'static str,
    enabled: bool,
    durable: Option<u32>,
    size_bytes: Option<u64>,
    tables: BTreeMap<String, u64>,
}

/// `None` = still starting
pub(crate) fn current(live: bool) -> Option<Status> {
    let sources = SOURCES.get()?;
    let view = sources.chainview.current();
    let endpoints = view.endpoints();
    let tip = *sources.chainview.subscribe_tip().borrow();
    let alarms = view.alarms();
    let members = sources.members.borrow().clone();

    let validators = endpoints
        .iter()
        .zip(&members.rows)
        .map(|(meta, member)| Validator {
            address: meta.address.clone(),
            state: meta.health.label(),
            agreement: meta.agreement.label(),
            height: meta.tip().map(|tip| u32::from(tip.height)),
            stale_blocks: meta.stale_blocks(),
            latency_ms: (member.health != Health::Pending)
                .then_some(member.latency.as_millis() as u64),
            failures: member.failures,
            observed_s_ago: meta.observed_at.map(|at| at.elapsed().as_secs()),
            streaming: meta.streaming,
            release: meta.release.as_ref().map(|release| Release {
                build: release.build.clone(),
                user_agent: release.user_agent.clone(),
                protocol_version: release.protocol_version,
                end_of_service: match release.end_of_service {
                    types::EndOfService::At { height, estimated_unix } => EndOfService::At {
                        height: u32::from(height),
                        estimated_unix,
                        blocks_left: meta.blocks_to_end_of_service(),
                    },
                    types::EndOfService::NotEnforced => EndOfService::NotEnforced,
                    types::EndOfService::Unknown => EndOfService::Unknown,
                },
            }),
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
            durable: index.committed.borrow().tip().map(|tip| u32::from(tip.height)),
            size_bytes: usage.as_ref().map(|usage| usage.total),
            tables: usage.map(|usage| usage.subdirs.into_iter().collect()).unwrap_or_default(),
        }
    });
    let disabled = sources.disabled.iter().map(|&name| Index {
        name,
        enabled: false,
        durable: None,
        size_bytes: None,
        tables: BTreeMap::new(),
    });
    let indexes: Vec<Index> = enabled.chain(disabled).collect();

    let readers = view.mempool_readers().count();
    let spreads: Vec<_> = view.spreads().map(|(_, spread)| spread).collect();
    let mempool = Mempool {
        transactions: spreads.len(),
        verified: spreads.iter().filter(|spread| spread.trusted.seen > 0).count(),
        ours_unverified: spreads.iter().filter(|s| s.ours && s.trusted.seen == 0).count(),
        fully_spread: spreads.iter().filter(|s| readers > 0 && s.trusted.seen == readers).count(),
        trusted_readers: readers,
    };
    let named = |set: zaino_chainview::EndpointSet| -> Vec<String> {
        let at = set.positions().filter_map(|position| endpoints.get(position));
        at.map(|meta| meta.address.clone()).collect()
    };

    let synced = *sources.synced.borrow();
    let reasons = reasons(draining(), live, view.unserved(), synced);
    Some(Status {
        version: env!("CARGO_PKG_VERSION"),
        network: sources.network,
        uptime_s: sources.started.elapsed().as_secs(),
        ready: reasons.is_empty(),
        reasons,
        tip: tip.map(|tip| Tip {
            height: u32::from(tip.block.height),
            hash: tip.block.hash.to_string(),
            held_by: tip.held_by.count(),
            configured: endpoints.len(),
        }),
        best_height: view.best().map(|best| u32::from(best.height)),
        fetch_height: (*sources.handed.borrow()).map(u32::from),
        served_height: sources.served.snapshot().map(|snap| u32::from(snap.tip().height)),
        synced,
        alarms: Alarms {
            partitioned: alarms.partitioned(),
            eclipsed: alarms.eclipsed(),
            stale: named(alarms.stale()),
            ending: named(alarms.ending()),
        },
        mempool,
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

/// Booted: fetch + served heights + every index's committed one; before: snapshot phase + bytes
pub(crate) fn startup(live: bool) -> Startup {
    let Some(status) = current(live) else {
        return Startup { ready: false, reasons: not_booted().0, progress: snapshot_progress() };
    };
    let heights = status.indexes.iter().map(|index| index.durable);
    let nfs = [status.fetch_height, status.served_height];
    let progress = nfs.into_iter().chain(heights).map(|h| h.map(u64::from));
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

/// Ready = not draining + runtime live + a held verified tip + served at it
fn reasons(
    draining: bool,
    live: bool,
    unserved: Option<zaino_chainview::Unserved>,
    synced: bool,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if draining {
        reasons.push("draining".to_owned());
    }
    if !live {
        reasons.push("heartbeat_stale".to_owned());
    }
    match unserved {
        Some(zaino_chainview::Unserved::NoBestTip) => reasons.push("headers_syncing".to_owned()),
        Some(zaino_chainview::Unserved::NotHeld { .. }) => reasons.push("tip_not_held".to_owned()),
        None => {}
    }
    if !synced {
        reasons.push("syncing".to_owned());
    }
    reasons
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every not-ready cause named (draining first); all clear = ready
    #[test]
    fn readiness_names_each_blocker() {
        use zaino_chainview::Unserved::{NoBestTip, NotHeld};
        assert_eq!(
            reasons(true, false, Some(NoBestTip), false),
            ["draining", "heartbeat_stale", "headers_syncing", "syncing"]
        );
        let unheld = Some(NotHeld { height: 7, configured: 2 });
        assert_eq!(reasons(false, true, unheld, false), ["tip_not_held", "syncing"]);
        assert_eq!(reasons(false, true, None, false), ["syncing"]);
        assert!(reasons(false, true, None, true).is_empty());
        assert_eq!(reasons(true, true, None, true), ["draining"], "healthy but draining");
    }

    /// Status page contract: one `status` tag per end-of-service case, fields only where they exist
    #[test]
    fn a_release_serializes_its_end_of_service_by_status() {
        let release = |end_of_service| {
            let release = Release {
                build: "v6.4.2".to_owned(),
                user_agent: "/Zebra:6.4.2/".to_owned(),
                protocol_version: 170_140,
                end_of_service,
            };
            serde_json::to_value(release).expect("serializes")
        };
        let mainnet = EndOfService::At {
            height: 3_564_960,
            estimated_unix: 1_790_000_000,
            blocks_left: Some(4_960),
        };
        assert_eq!(
            release(mainnet),
            serde_json::json!({
                "build": "v6.4.2", "user_agent": "/Zebra:6.4.2/", "protocol_version": 170_140,
                "end_of_service": {
                    "status": "at", "height": 3_564_960, "estimated_unix": 1_790_000_000,
                    "blocks_left": 4_960,
                },
            })
        );
        let status = |eos| release(eos)["end_of_service"].clone();
        assert_eq!(
            status(EndOfService::NotEnforced),
            serde_json::json!({ "status": "not_enforced" })
        );
        assert_eq!(status(EndOfService::Unknown), serde_json::json!({ "status": "unknown" }));
    }
}
