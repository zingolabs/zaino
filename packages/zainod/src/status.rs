//! `/readyz`, public `/status` + the `/metrics` gauges: each from one `GrpcSnapshot` load (G9)
//!
//! - [`DaemonStatus`] built once by `daemon()`, `Arc`-shared with every reader and writer
//! - `ready` ⇔ `state` = serving or degraded ⇔ `/readyz` 200 (degraded = a fresh peer-verified
//!   tip served, no trusted validator vouching for it)

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};

use serde::Serialize;
use tokio::sync::watch;
use zaino_grpc::{Hold, Retire};
use zaino_grpc_snapshot::{GrpcSnapshot, GrpcSnapshots, ServingStatus, StatusReason};
use zaino_persistence::{DiskView, IndexKind};
use zaino_sync::SyncProgress;
use zaino_traffic::Health;
use zcash_protocol::consensus::NetworkType;

use crate::admin::Heartbeat;
use crate::egress::Egress;

/// One process's status: config facts, then what each stage sets as it starts
///
/// - `retire` = the operator hold (`hold.rs` sets it, the gRPC service reads it from boot)
/// - `sources` set once the pipeline boots (`provisioning` until then)
/// - Second `sources` set panics
pub(crate) struct DaemonStatus {
    network: NetworkType,
    configured: Vec<IndexKind>,
    heartbeat: Heartbeat,
    retire: Retire,
    awaiting_validator: AtomicBool,
    bootstrap: std::sync::Mutex<Option<Bootstrap>>,
    sources: OnceLock<Sources>,
}

/// Booted pipeline's readers
///
/// - `block_rate` = blocks handed per second over its last ~60 s (`None` before its first tick)
/// - `egress` = last closed 10-min window
pub(crate) struct Sources {
    pub(crate) grpc_snapshots: GrpcSnapshots<DiskView>,
    pub(crate) progress: SyncProgress,
    pub(crate) block_rate: watch::Receiver<Option<f64>>,
    pub(crate) grpc_admission: Arc<zaino_grpc::AdmissionState>,
    pub(crate) egress: watch::Receiver<Option<Egress>>,
}

/// Closed set, first match wins: held, not booted, runtime stalled, `GrpcSnapshot`'s [`ServingStatus`]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DaemonState {
    Provisioning,
    Syncing,
    Serving,
    Degraded,
    Held,
    Down,
}

/// `rate` per second of `unit` (`None` = not measured yet, or a bootstrap phase past downloading)
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Progress {
    done: u64,
    total: u64,
    unit: Unit,
    rate: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Unit {
    Block,
    Byte,
}

/// Head of `/status` (`/readyz` = `state` + `detail`)
///
/// - `detail` only when `down`, `degraded` or `held` (its GOAWAYs), `progress` only while
///   provisioning or syncing, `notes` = raised alarms
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Readiness {
    state: DaemonState,
    progress: Option<Progress>,
    detail: Option<&'static str>,
    notes: Vec<String>,
}

impl Readiness {
    /// Serving, or degraded (serving a peer-verified tip no trusted validator vouches for)
    pub(crate) fn ready(&self) -> bool {
        matches!(self.state, DaemonState::Serving | DaemonState::Degraded)
    }
}

const RUNTIME_STALLED: &str = "Runtime stalled";

/// `zaino_grpc::AdmissionReport::overloaded`
const OVERLOADED: &str = "Overloaded";

/// Held, `hold_goaway_delay_secs` not yet passed
const GOAWAY_PENDING: &str = "GOAWAY pending";

/// Held past the delay: every connection, open or new, GOAWAY'd (`zainod hold` waits for this)
pub(crate) const GOAWAY_SENT: &str = "GOAWAY sent";

/// Where the process is: before boot (index-snapshot bootstrap, first validator answer) or booted
enum Stage {
    Starting { bootstrap: Option<Progress>, awaiting_validator: bool },
    Booted(Booted),
}

/// One snapshot's facts: `done` = served height (else handed), `target` = [`GrpcSnapshot::sync_target`]
struct Booted {
    serving_status: ServingStatus,
    overloaded: bool,
    done: Option<u32>,
    target: Option<u32>,
    rate: Option<f64>,
    notes: Vec<String>,
}

impl Booted {
    fn of(sources: &Sources, grpc_snapshot: &GrpcSnapshot<DiskView>) -> Self {
        let served = grpc_snapshot.tips().served.map(|tip| tip.height);
        let (progress, rate) = (&sources.progress, *sources.block_rate.borrow());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        let now = now.map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
        Self {
            serving_status: grpc_snapshot.serving_status(now),
            overloaded: sources.grpc_admission.report().overloaded,
            done: served.or(progress.handed()).map(u32::from),
            target: grpc_snapshot.sync_target().map(u32::from),
            rate,
            notes: zaino_grpc_snapshot::notes(grpc_snapshot),
        }
    }
}

/// Pure: the state rules over the hold + heartbeat + stage (held > the rest)
fn readiness(hold: Hold, live: bool, stage: Stage) -> Readiness {
    let at = |state, progress, detail, notes| Readiness { state, progress, detail, notes };
    let goaway = match hold {
        Hold::Off => None,
        Hold::Draining => Some(GOAWAY_PENDING),
        Hold::Drained => Some(GOAWAY_SENT),
    };
    if let Some(goaway) = goaway {
        let notes = match stage {
            Stage::Booted(booted) => booted.notes,
            Stage::Starting { .. } => Vec::new(),
        };
        return at(DaemonState::Held, None, Some(goaway), notes);
    }
    match (live, stage) {
        (false, Stage::Booted(booted)) => {
            at(DaemonState::Down, None, Some(RUNTIME_STALLED), booted.notes)
        }
        (_, Stage::Starting { awaiting_validator: true, .. }) => at(
            DaemonState::Down,
            None,
            Some(StatusReason::NoValidatorReachable.detail()),
            Vec::new(),
        ),
        (_, Stage::Starting { bootstrap, .. }) => {
            at(DaemonState::Provisioning, bootstrap, None, Vec::new())
        }
        (true, Stage::Booted(booted)) if booted.overloaded => {
            at(DaemonState::Down, None, Some(OVERLOADED), booted.notes)
        }
        (true, Stage::Booted(booted)) => match booted.serving_status {
            ServingStatus::Down(why) => {
                at(DaemonState::Down, None, Some(why.detail()), booted.notes)
            }
            ServingStatus::Degraded(why) => {
                at(DaemonState::Degraded, None, Some(why.detail()), booted.notes)
            }
            ServingStatus::Serving => at(DaemonState::Serving, None, None, booted.notes),
            ServingStatus::Syncing => {
                let progress = booted.target.map(|total| Progress {
                    done: u64::from(booted.done.unwrap_or(0)),
                    total: u64::from(total),
                    unit: Unit::Block,
                    rate: booted.rate,
                });
                at(DaemonState::Syncing, progress, None, booted.notes)
            }
        },
    }
}

impl DaemonStatus {
    pub(crate) fn new(
        network: NetworkType,
        configured: impl IntoIterator<Item = IndexKind>,
    ) -> Self {
        Self {
            network,
            configured: configured.into_iter().collect(),
            heartbeat: Heartbeat::default(),
            retire: Retire::default(),
            awaiting_validator: AtomicBool::new(false),
            bootstrap: std::sync::Mutex::new(None),
            sources: OnceLock::new(),
        }
    }

    pub(crate) fn beat(&self) {
        self.heartbeat.beat();
    }

    /// Serving runtime still scheduling (`/livez`, stale = `down`, "Runtime stalled")
    pub(crate) fn live(&self) -> bool {
        self.heartbeat.live()
    }

    /// Operator hold (`hold.rs`, from its marker): `held` above every other state, and the gRPC
    /// service's GOAWAYs
    pub(crate) fn retire(&self) -> &Retire {
        &self.retire
    }

    /// Boot waiting on its first validator answer (the upgrade schedule): `down`, not `provisioning`
    pub(crate) fn awaiting_validator(&self, awaiting: bool) {
        self.awaiting_validator.store(awaiting, Ordering::Relaxed);
    }

    pub(crate) fn publish(&self, sources: Sources) {
        assert!(self.sources.set(sources).is_ok(), "one pipeline boot per process");
    }

    fn current_readiness(&self) -> Readiness {
        let stage = match self.sources.get() {
            Some(sources) => Stage::Booted(Booted::of(sources, &sources.grpc_snapshots.load())),
            None => Stage::Starting {
                bootstrap: self.bootstrap_bytes(),
                awaiting_validator: self.awaiting_validator.load(Ordering::Relaxed),
            },
        };
        readiness(self.retire.state(), self.live(), stage)
    }

    /// `/readyz`: `(ready, {"state", "detail"})`
    pub(crate) fn readiness_json(&self) -> (bool, String) {
        let readiness = self.current_readiness();
        let body = serde_json::json!({ "state": readiness.state, "detail": readiness.detail });
        (readiness.ready(), body.to_string())
    }

    /// Public `/status` body, now
    fn public_status_json(&self) -> String {
        let sources = self.sources.get();
        let chain =
            sources.map(|sources| Chain::of(sources, &sources.grpc_snapshots.load(), self.network));
        let status = public_status(self.current_readiness(), self.network, chain, &self.configured);
        serde_json::to_string(&status).unwrap_or_default()
    }

    /// Every `GrpcSnapshot` gauge, set just before a scrape renders (nothing before boot)
    pub(crate) fn emit_gauges(&self) {
        if let Some(sources) = self.sources.get() {
            zaino_grpc_snapshot::emit_gauges(&sources.grpc_snapshots.load(), &sources.progress);
        }
    }

    /// - Booted: handed + served heights + every index's durable one
    /// - Before boot: bootstrap phase + bytes
    pub(crate) fn startup(&self) -> Startup {
        let readiness = self.current_readiness();
        let started = readiness.ready() || readiness.state == DaemonState::Held;
        let Some(sources) = self.sources.get() else {
            return Startup { started, progress: self.bootstrap_progress() };
        };
        let grpc_snapshot = sources.grpc_snapshots.load();
        let durable = grpc_snapshot.indexed().into_iter().flat_map(|indexed| indexed.durable());
        let durable = durable.map(|(_, tip)| tip.map(|tip| tip.height));
        let heights =
            [sources.progress.handed(), grpc_snapshot.tips().served.map(|tip| tip.height)];
        let progress =
            heights.into_iter().chain(durable).map(|h| h.map(|h| u64::from(u32::from(h))));
        Startup { started, progress: progress.collect() }
    }

    /// `None` = bootstrap over (the indexer's own status takes over)
    pub(crate) fn bootstrap(&self, progress: Option<Bootstrap>) {
        *self.bootstrap_lock() = progress;
    }

    /// Poison-tolerant: the lock only guards a value swap
    fn bootstrap_lock(&self) -> std::sync::MutexGuard<'_, Option<Bootstrap>> {
        self.bootstrap.lock().expect("bootstrap progress mutex poisoned")
    }

    pub(crate) fn current_bootstrap(&self) -> Option<Bootstrap> {
        self.bootstrap_lock().clone()
    }

    /// While a bootstrap runs: its bytes (`rate` downloading only)
    fn bootstrap_bytes(&self) -> Option<Progress> {
        self.current_bootstrap().map(|bootstrap| Progress {
            done: bootstrap.done,
            total: bootstrap.total,
            unit: Unit::Byte,
            rate: bootstrap.rate.map(|rate| rate as f64),
        })
    }

    fn bootstrap_progress(&self) -> Vec<Option<u64>> {
        let progress = self.current_bootstrap();
        progress.map_or_else(Vec::new, |bootstrap| {
            vec![Some(bootstrap.phase as u64), Some(bootstrap.done)]
        })
    }
}

/// `GET /readyz` + `GET /status` on the gRPC listener
impl zaino_grpc::Health for DaemonStatus {
    fn readyz(&self) -> (bool, String) {
        self.readiness_json()
    }

    fn status(&self) -> String {
        self.public_status_json()
    }
}

/// Archive bootstrap progress, written by [`crate::bootstrap`] before the indexer boots
///
/// - `source` = archive host, `indexes` archives fetched, `total` = Σ their bytes
/// - `phase` = slowest archive's, `done` = its bytes (archives past it whole)
/// - `rate` = bytes/s (`downloading` only)
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Bootstrap {
    pub(crate) phase: Phase,
    pub(crate) source: String,
    pub(crate) height: u32,
    pub(crate) indexes: usize,
    pub(crate) done: u64,
    pub(crate) total: u64,
    pub(crate) rate: Option<u64>,
}

/// Declared in pipeline order (`Ord`: earliest = slowest)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    Downloading,
    Verifying,
    Unpacking,
}

/// Public `/status` (gRPC listener): readiness + chain position + counts, safe for the open
/// internet (no validator address, peer, disk, mempool or tailnet name)
#[derive(Debug, Serialize, PartialEq)]
struct PublicStatus {
    #[serde(flatten)]
    readiness: Readiness,
    version: &'static str,
    network: &'static str,
    tip: Option<PublicBlock>,
    served: Option<u32>,
    validators: Option<ValidatorCounts>,
    indexes: IndexCounts,
    egress: Option<Egress>,
    info: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, PartialEq)]
struct PublicBlock {
    height: u32,
    hash: String,
}

/// `connected` = answering (live or catching up)
#[derive(Debug, Serialize, PartialEq)]
struct ValidatorCounts {
    configured: usize,
    connected: usize,
}

/// `enabled` of the `total` indexes zainod can build
#[derive(Debug, Serialize, PartialEq)]
struct IndexCounts {
    enabled: Vec<&'static str>,
    total: usize,
}

/// Booted facts of the public status (`None` before boot)
struct Chain {
    tip: Option<PublicBlock>,
    served: Option<u32>,
    validators: ValidatorCounts,
    egress: Option<Egress>,
    info: Option<serde_json::Value>,
}

impl Chain {
    fn of(sources: &Sources, grpc_snapshot: &GrpcSnapshot<DiskView>, network: NetworkType) -> Self {
        let tips = grpc_snapshot.tips();
        let trusted_validators = grpc_snapshot.chain_view().trusted_validators();
        let answering = |meta: &&zaino_chainview::TrustedValidatorMeta| {
            matches!(meta.health, Health::Live | Health::CatchingUp)
        };
        let info = zaino_grpc::lightd_info(grpc_snapshot, network, env!("CARGO_PKG_VERSION"));
        Self {
            tip: tips.best.map(|best| PublicBlock {
                height: u32::from(best.height),
                hash: best.hash.to_string(),
            }),
            served: tips.served.map(|served| u32::from(served.height)),
            validators: ValidatorCounts {
                configured: trusted_validators.len(),
                connected: trusted_validators.iter().filter(answering).count(),
            },
            egress: *sources.egress.borrow(),
            info: info.map(lightd_json),
        }
    }
}

/// `GetLightdInfo` as lightwalletd's JSON names it (camelCase, every field)
fn lightd_json(info: zaino_grpc::LightdInfo) -> serde_json::Value {
    serde_json::json!({
        "version": info.version,
        "vendor": info.vendor,
        "taddrSupport": info.taddr_support,
        "chainName": info.chain_name,
        "saplingActivationHeight": info.sapling_activation_height,
        "consensusBranchId": info.consensus_branch_id,
        "blockHeight": info.block_height,
        "gitCommit": info.git_commit,
        "branch": info.branch,
        "buildDate": info.build_date,
        "buildUser": info.build_user,
        "estimatedHeight": info.estimated_height,
        "zcashdBuild": info.zcashd_build,
        "zcashdSubversion": info.zcashd_subversion,
        "donationAddress": info.donation_address,
        "upgradeName": info.upgrade_name,
        "upgradeHeight": info.upgrade_height,
        "lightwalletProtocolVersion": info.lightwallet_protocol_version,
    })
}

fn public_status(
    readiness: Readiness,
    network: NetworkType,
    chain: Option<Chain>,
    configured: &[IndexKind],
) -> PublicStatus {
    let all = IndexKind::ALL;
    let enabled = all.into_iter().filter(|kind| configured.contains(kind)).map(IndexKind::name);
    let (tip, served, validators, egress, info) = match chain {
        Some(chain) => (chain.tip, chain.served, Some(chain.validators), chain.egress, chain.info),
        None => (None, None, None, None, None),
    };
    PublicStatus {
        readiness,
        version: env!("CARGO_PKG_VERSION"),
        network: crate::config::network_name(network),
        tip,
        served,
        validators,
        indexes: IndexCounts { enabled: enabled.collect(), total: all.len() },
        egress,
        info,
    }
}

/// Readiness + a progress fingerprint for [`crate::notify`] (fingerprint moved = startup advanced)
///
/// - `started` = serving or held (a node started held must not fail its start job waiting on a
///   readiness the operator withholds)
pub(crate) struct Startup {
    pub(crate) started: bool,
    pub(crate) progress: Vec<Option<u64>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every state rule, first match wins, as `/status` serializes it
    ///
    /// - Across all: `ready` ⇔ serving or degraded, `detail` only when down, degraded or held
    /// - `progress` only provisioning or syncing
    #[test]
    fn the_state_is_the_first_matching_rule_with_its_progress_detail_and_notes() {
        let stage = |serving_status, overloaded, target: Option<u32>, rate: Option<f64>| {
            let notes = vec!["1 validator on a stale tip".to_owned()];
            let done = Some(3_514_194);
            Stage::Booted(Booted { serving_status, overloaded, done, target, rate, notes })
        };
        let booted = |serving_status, target, rate| stage(serving_status, false, target, rate);
        let overloaded = |serving_status| stage(serving_status, true, Some(3_515_406), Some(6.2));
        let starting =
            |bootstrap, awaiting_validator| Stage::Starting { bootstrap, awaiting_validator };
        let stale = serde_json::json!(["1 validator on a stale tip"]);
        let no_holder = ServingStatus::Down(StatusReason::TipNotHeld);
        let unreachable = ServingStatus::Down(StatusReason::NoValidatorReachable);
        let cases = [
            (
                "plain startup",
                true,
                starting(None, false),
                serde_json::json!({ "state": "provisioning", "progress": null, "detail": null, "notes": [] }),
            ),
            (
                "boot waiting on a validator",
                true,
                starting(None, true),
                serde_json::json!({
                    "state": "down", "progress": null, "detail": "No trusted validator reachable",
                    "notes": [],
                }),
            ),
            (
                "runtime stalled beats serving",
                false,
                booted(ServingStatus::Serving, Some(3_514_194), None),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "runtime stalled beats overloaded",
                false,
                overloaded(ServingStatus::Serving),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "overloaded beats syncing",
                true,
                overloaded(ServingStatus::Syncing),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded", "notes": stale }),
            ),
            (
                "overloaded beats unreachable validators",
                true,
                overloaded(unreachable),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded", "notes": stale }),
            ),
            (
                "overloaded beats degraded",
                true,
                overloaded(ServingStatus::Degraded(StatusReason::NoValidatorReachable)),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded", "notes": stale }),
            ),
            (
                "runtime stalled beats degraded",
                false,
                booted(ServingStatus::Degraded(StatusReason::TipNotHeld), None, None),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "every validator down, a fresh peer-verified tip served",
                true,
                booted(
                    ServingStatus::Degraded(StatusReason::NoValidatorReachable),
                    Some(3_514_194),
                    None,
                ),
                serde_json::json!({
                    "state": "degraded", "progress": null,
                    "detail": "No trusted validator reachable", "notes": stale,
                }),
            ),
            (
                "best unheld, a fresh peer-verified tip served",
                true,
                booted(ServingStatus::Degraded(StatusReason::TipNotHeld), Some(3_514_194), None),
                serde_json::json!({
                    "state": "degraded", "progress": null,
                    "detail": "No trusted validator holds the verified tip", "notes": stale,
                }),
            ),
            (
                "every validator down",
                true,
                booted(unreachable, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "down", "progress": null, "detail": "No trusted validator reachable",
                    "notes": stale,
                }),
            ),
            (
                "best unheld",
                true,
                booted(no_holder, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "down", "progress": null,
                    "detail": "No trusted validator holds the verified tip", "notes": stale,
                }),
            ),
            (
                "syncing",
                true,
                booted(ServingStatus::Syncing, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "syncing", "detail": null, "notes": stale,
                    "progress": { "done": 3_514_194, "total": 3_515_406, "unit": "block", "rate": 6.2 },
                }),
            ),
            (
                "syncing, no header chain yet",
                true,
                booted(ServingStatus::Syncing, None, None),
                serde_json::json!({ "state": "syncing", "progress": null, "detail": null, "notes": stale }),
            ),
            (
                "serving",
                true,
                booted(ServingStatus::Serving, Some(3_514_194), Some(1.0)),
                serde_json::json!({ "state": "serving", "progress": null, "detail": null, "notes": stale }),
            ),
            (
                "bootstrap downloading",
                true,
                starting(
                    Some(Progress {
                        done: 4 << 30,
                        total: 9 << 30,
                        unit: Unit::Byte,
                        rate: Some(5e7),
                    }),
                    false,
                ),
                serde_json::json!({
                    "state": "provisioning", "detail": null, "notes": [],
                    "progress": { "done": 4u64 << 30, "total": 9u64 << 30, "unit": "byte", "rate": 5e7 },
                }),
            ),
        ];
        for (case, live, stage, expected) in cases {
            let readiness = readiness(Hold::Off, live, stage);
            let state = readiness.state;
            assert_eq!(serde_json::to_value(&readiness).expect("serializes"), expected, "{case}");
            let serving = matches!(state, DaemonState::Serving | DaemonState::Degraded);
            assert_eq!(readiness.ready(), serving, "{case}: ready ⇔ serving or degraded");
            let detailed = matches!(state, DaemonState::Down | DaemonState::Degraded);
            assert_eq!(readiness.detail.is_some(), detailed, "{case}: detail ⇔ down or degraded");
            let progressing = matches!(state, DaemonState::Provisioning | DaemonState::Syncing);
            assert!(readiness.progress.is_none() || progressing, "{case}: progress off-state");
        }

        // held: the operator's intent over every condition, notes kept, detail = its GOAWAYs
        let held = |detail, notes| serde_json::json!({ "state": "held", "progress": null, "detail": detail, "notes": notes });
        let held_cases = [
            (
                "held while serving",
                Hold::Draining,
                true,
                booted(ServingStatus::Serving, None, None),
                held("GOAWAY pending", stale.clone()),
            ),
            (
                "held while overloaded",
                Hold::Drained,
                true,
                overloaded(unreachable),
                held("GOAWAY sent", stale.clone()),
            ),
            (
                "held while degraded",
                Hold::Drained,
                true,
                booted(ServingStatus::Degraded(StatusReason::NoValidatorReachable), None, None),
                held("GOAWAY sent", stale.clone()),
            ),
            (
                "held with a stalled runtime",
                Hold::Draining,
                false,
                booted(ServingStatus::Syncing, None, None),
                held("GOAWAY pending", stale.clone()),
            ),
            (
                "held while booting, no validator",
                Hold::Drained,
                true,
                starting(None, true),
                held("GOAWAY sent", serde_json::json!([])),
            ),
        ];
        for (case, hold, live, stage, expected) in held_cases {
            let readiness = readiness(hold, live, stage);
            assert_eq!(serde_json::to_value(&readiness).expect("serializes"), expected, "{case}");
            assert!(!readiness.ready(), "{case}: never ready");
        }
    }

    /// Public `/status`, field by field
    ///
    /// - Before boot (chain facts `null`), then booted with a tip, one of two validators
    ///   answering, an egress window and `GetLightdInfo`
    /// - Never a validator address, peer, disk or mempool
    #[test]
    fn the_public_status_projects_readiness_and_counts_only() {
        let configured = [IndexKind::CompactBlock, IndexKind::UtxoSet];
        let starting = Stage::Starting { bootstrap: None, awaiting_validator: false };
        let readiness = |stage| readiness(Hold::Off, true, stage);
        let indexes = serde_json::json!({ "enabled": ["utxo_set", "compact_block"], "total": 5 });
        let version = env!("CARGO_PKG_VERSION");

        let before = public_status(readiness(starting), NetworkType::Main, None, &configured);
        assert_eq!(
            serde_json::to_value(before).expect("serializes"),
            serde_json::json!({
                "state": "provisioning", "progress": null, "detail": null, "notes": [],
                "version": version, "network": "mainnet", "tip": null, "served": null,
                "validators": null, "indexes": indexes, "egress": null, "info": null,
            }),
        );

        let serving = Stage::Booted(Booted {
            serving_status: ServingStatus::Serving,
            overloaded: false,
            done: Some(3_515_400),
            target: Some(3_515_406),
            rate: None,
            notes: Vec::new(),
        });
        let chain = Chain {
            tip: Some(PublicBlock { height: 3_515_406, hash: "00ab".to_owned() }),
            served: Some(3_515_400),
            validators: ValidatorCounts { configured: 2, connected: 1 },
            egress: Some(Egress { start: 1_790_000_400, mbps: 12.5 }),
            info: Some(serde_json::json!({ "blockHeight": 3_515_400 })),
        };
        let after = public_status(readiness(serving), NetworkType::Main, Some(chain), &configured);
        assert_eq!(
            serde_json::to_value(after).expect("serializes"),
            serde_json::json!({
                "state": "serving", "progress": null, "detail": null, "notes": [],
                "version": version, "network": "mainnet",
                "tip": { "height": 3_515_406, "hash": "00ab" }, "served": 3_515_400,
                "validators": { "configured": 2, "connected": 1 }, "indexes": indexes,
                "egress": { "start": 1_790_000_400, "mbps": 12.5 },
                "info": { "blockHeight": 3_515_400 },
            }),
        );
    }
}
