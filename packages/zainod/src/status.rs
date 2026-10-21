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
use zaino_grpc::{Hold, OverloadCause, Retire};
use zaino_grpc_snapshot::{GrpcSnapshot, GrpcSnapshots, ServingStatus};
use zaino_persistence::{DiskView, IndexKind};
use zaino_sync::SyncProgress;
use zaino_traffic::Health;
use zcash_protocol::consensus::NetworkType;

use crate::admin::Heartbeat;
use crate::egress::Egress;
use crate::index_snapshot::SnapshotProgress;

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
    bootstrap: std::sync::Mutex<Option<SnapshotProgress>>,
    overload_notes: std::sync::Mutex<Vec<String>>,
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

/// One-hot by `state` (every state each scrape: 1 = now, 0 = the rest; no series left stale)
const STATE: &str = "zaino.state";

impl DaemonState {
    const ALL: [Self; 6] =
        [Self::Provisioning, Self::Syncing, Self::Serving, Self::Degraded, Self::Held, Self::Down];

    /// = its `/readyz` + `/status` spelling (serde's)
    fn label(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Syncing => "syncing",
            Self::Serving => "serving",
            Self::Degraded => "degraded",
            Self::Held => "held",
            Self::Down => "down",
        }
    }
}

pub(crate) fn describe_metrics() {
    metrics::describe_gauge!(
        STATE,
        "Daemon state, one-hot by state (= /readyz's: provisioning, syncing, serving, degraded, \
         held, down)"
    );
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

/// Why `down`, `degraded` or `held` (`detail()` = `/readyz` + `/status` `detail`)
///
/// - `Overloaded` = `zaino_grpc::AdmissionReport::overload`
/// - `GoawaySent` = held past `hold_goaway_delay_secs`, every connection GOAWAY'd (`zainod hold`
///   waits for this)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DaemonReason {
    NoValidatorReachable,
    TipUnvouched,
    RuntimeStalled,
    Overloaded(OverloadCause),
    GoawayPending,
    GoawaySent,
}

impl DaemonReason {
    pub(crate) const fn detail(self) -> &'static str {
        match self {
            Self::NoValidatorReachable => "No trusted validator reachable",
            Self::TipUnvouched => "No trusted validator vouches for the tip",
            Self::RuntimeStalled => "Runtime stalled",
            Self::Overloaded(cause) => cause.detail(),
            Self::GoawayPending => "GOAWAY pending",
            Self::GoawaySent => "GOAWAY sent",
        }
    }
}

/// Where the process is: before boot (index-snapshot bootstrap, first validator answer) or booted
enum Stage {
    Starting { bootstrap: Option<Progress>, awaiting_validator: bool },
    Booted(Booted),
}

/// One snapshot's facts: `done` = served height (else handed), `target` = [`GrpcSnapshot::sync_target`]
struct Booted {
    serving_status: ServingStatus,
    overload: Option<OverloadCause>,
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
            overload: sources.grpc_admission.report().overload,
            done: served.or(progress.handed()).map(u32::from),
            target: grpc_snapshot.sync_target().map(u32::from),
            rate,
            notes: zaino_grpc_snapshot::notes(grpc_snapshot),
        }
    }
}

/// Pure: the state rules over the hold + heartbeat + stage (held > the rest)
fn readiness(hold: Hold, live: bool, stage: Stage) -> Readiness {
    let at = |state, progress, reason: Option<DaemonReason>, notes| Readiness {
        state,
        progress,
        detail: reason.map(DaemonReason::detail),
        notes,
    };
    let goaway = match hold {
        Hold::Off => None,
        Hold::Draining => Some(DaemonReason::GoawayPending),
        Hold::Drained => Some(DaemonReason::GoawaySent),
    };
    if let Some(goaway) = goaway {
        let notes = match stage {
            Stage::Booted(booted) => booted.notes,
            Stage::Starting { .. } => Vec::new(),
        };
        return at(DaemonState::Held, None, Some(goaway), notes);
    }
    let unvouched = Some(DaemonReason::TipUnvouched);
    match (live, stage) {
        (false, Stage::Booted(booted)) => {
            at(DaemonState::Down, None, Some(DaemonReason::RuntimeStalled), booted.notes)
        }
        (_, Stage::Starting { awaiting_validator: true, .. }) => {
            at(DaemonState::Down, None, Some(DaemonReason::NoValidatorReachable), Vec::new())
        }
        (_, Stage::Starting { bootstrap, .. }) => {
            at(DaemonState::Provisioning, bootstrap, None, Vec::new())
        }
        (true, Stage::Booted(Booted { overload: Some(cause), notes, .. })) => {
            at(DaemonState::Down, None, Some(DaemonReason::Overloaded(cause)), notes)
        }
        (true, Stage::Booted(booted)) => match booted.serving_status {
            ServingStatus::Down => at(DaemonState::Down, None, unvouched, booted.notes),
            ServingStatus::Degraded => at(DaemonState::Degraded, None, unvouched, booted.notes),
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
            overload_notes: std::sync::Mutex::new(Vec::new()),
            sources: OnceLock::new(),
        }
    }

    /// The operator's overload overrides in force (`overload.rs`): `/status` notes
    pub(crate) fn set_overload_notes(&self, notes: Vec<String>) {
        *self.overload_notes.lock().expect("notes mutex poisoned") = notes;
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
            Some(sources) => {
                let mut booted = Booted::of(sources, &sources.grpc_snapshots.load());
                booted.notes.extend(self.overload_notes.lock().expect("notes poisoned").clone());
                Stage::Booted(booted)
            }
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

    /// The state (`/readyz`'s) + every `GrpcSnapshot` gauge (none before boot), set just before a
    /// scrape renders
    pub(crate) fn emit_gauges(&self) {
        let now = self.current_readiness().state;
        for state in DaemonState::ALL {
            metrics::gauge!(STATE, "state" => state.label()).set(f64::from(u8::from(state == now)));
        }
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
    pub(crate) fn bootstrap(&self, progress: Option<SnapshotProgress>) {
        *self.bootstrap_lock() = progress;
    }

    /// Poison-tolerant: the lock only guards a value swap
    fn bootstrap_lock(&self) -> std::sync::MutexGuard<'_, Option<SnapshotProgress>> {
        self.bootstrap.lock().expect("bootstrap progress mutex poisoned")
    }

    pub(crate) fn current_bootstrap(&self) -> Option<SnapshotProgress> {
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
        let stage = |serving_status, overload, target: Option<u32>, rate: Option<f64>| {
            let notes = vec!["1 validator on a stale tip".to_owned()];
            let done = Some(3_514_194);
            Stage::Booted(Booted { serving_status, overload, done, target, rate, notes })
        };
        let booted = |serving_status, target, rate| stage(serving_status, None, target, rate);
        let saturated = Some(OverloadCause::NetworkSaturated);
        let overloaded =
            |serving_status| stage(serving_status, saturated, Some(3_515_406), Some(6.2));
        let starting =
            |bootstrap, awaiting_validator| Stage::Starting { bootstrap, awaiting_validator };
        let stale = serde_json::json!(["1 validator on a stale tip"]);
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
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded: network saturated", "notes": stale }),
            ),
            (
                "overloaded beats down",
                true,
                overloaded(ServingStatus::Down),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded: network saturated", "notes": stale }),
            ),
            (
                "overloaded beats degraded",
                true,
                overloaded(ServingStatus::Degraded),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded: network saturated", "notes": stale }),
            ),
            (
                "runtime stalled beats degraded",
                false,
                booted(ServingStatus::Degraded, None, None),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "unvouched tip, fresh: peers carried it",
                true,
                booted(ServingStatus::Degraded, Some(3_514_194), None),
                serde_json::json!({
                    "state": "degraded", "progress": null,
                    "detail": "No trusted validator vouches for the tip", "notes": stale,
                }),
            ),
            (
                "unvouched tip, stale",
                true,
                booted(ServingStatus::Down, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "down", "progress": null,
                    "detail": "No trusted validator vouches for the tip", "notes": stale,
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
                overloaded(ServingStatus::Down),
                held("GOAWAY sent", stale.clone()),
            ),
            (
                "held while degraded",
                Hold::Drained,
                true,
                booted(ServingStatus::Degraded, None, None),
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
        let indexes = serde_json::json!({ "enabled": ["utxo_set", "compact_block"], "total": 6 });
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
            overload: None,
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

    /// `zaino_state` = `/readyz`'s state, one-hot over all six (each label = its serde spelling)
    ///
    /// - not booted = provisioning, an operator hold = held: one 1, five 0s each time
    #[test]
    fn the_state_gauge_is_readyz_one_hot() {
        let (recorder, rendered) = crate::metrics::recorder().expect("native config is valid");
        let _recording = metrics::set_default_local_recorder(&recorder);
        let status = DaemonStatus::new(NetworkType::Regtest, []);
        // sorted: the exporter renders a family's series in no fixed order
        let one_hot = |now: DaemonState| -> Vec<String> {
            let mut lines = DaemonState::ALL.map(|state| {
                format!("zaino_state{{state=\"{}\"}} {}", state.label(), u8::from(state == now))
            });
            lines.sort();
            lines.to_vec()
        };
        let scraped = || {
            status.emit_gauges();
            let text = rendered.render();
            let mut lines: Vec<_> = text
                .lines()
                .filter(|line| line.starts_with("zaino_state{"))
                .map(str::to_owned)
                .collect();
            lines.sort();
            lines
        };

        let spellings = DaemonState::ALL.map(|state| (serde_json::json!(state), state.label()));
        assert!(spellings.iter().all(|(serde, label)| serde == label), "{spellings:?}");
        assert_eq!(scraped(), one_hot(DaemonState::Provisioning), "not booted");
        status.retire().hold(std::time::Duration::ZERO);
        assert_eq!(scraped(), one_hot(DaemonState::Held), "operator hold");
    }
}
