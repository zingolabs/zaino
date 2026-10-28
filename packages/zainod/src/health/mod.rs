//! `/readyz`, public `/status` + the `/metrics` gauges: each from one `GrpcSnapshot` load (G9)
//!
//! - [`DaemonStatus`] built once by `daemon()`, `Arc`-shared with every reader and writer
//! - `ready` ⇔ `state` = serving or degraded ⇔ `/readyz` 200 (degraded = a fresh peer-verified
//!   tip served, no trusted validator vouching for it)

use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};

use serde::Serialize;
use tokio::sync::watch;
use zaino_grpc::{Hold, OverloadCause, Retire};
use zaino_grpc_snapshot::{GrpcSnapshot, GrpcSnapshots, ServingStatus};
use zaino_persistence::DiskView;
use zaino_primitives::metric::Metric;
use zaino_sync::IndexKind;
use zaino_sync::SyncProgress;
use zaino_traffic::Health;
use zcash_protocol::consensus::NetworkType;

use crate::daemon::index_snapshot::SnapshotProgress;
use crate::telemetry::egress::Egress;
use admin::Heartbeat;

pub(crate) mod admin;
pub(super) mod notify;
pub(super) mod state_log;

/// One process's status: config facts, then what each stage sets as it starts
///
/// - `retire` = the operator hold (`command::hold` sets it, the gRPC service reads it from boot)
/// - `sources` set once the pipeline boots (`provisioning` until then)
/// - Second `sources` set panics
pub(crate) struct DaemonStatus {
    network: NetworkType,
    configured: Vec<IndexKind>,
    heartbeat: Heartbeat,
    retire: Retire,
    awaiting_validator: AtomicBool,
    bootstrap: watch::Sender<Option<SnapshotProgress>>,
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

/// One-hot by `state` (every state each scrape: 1 = now, 0 = the rest; no series left stale)
const STATE: &str = "zaino.state";
/// One-hot by `reason` as STATE (all 0 = none: serving, syncing, provisioning)
const REASON: &str = "zaino.state_reason";

pub(crate) const METRICS: &[Metric] = &[
    Metric::gauge(
        STATE,
        "Daemon state, one-hot by state (= /readyz's: provisioning, syncing, serving, degraded, \
         held, down)",
    ),
    Metric::gauge(
        REASON,
        "Why down, degraded or held, one-hot by reason (= /readyz's detail.reason; overloaded = \
         its cause in zaino_grpc_overload_latched)",
    ),
];

/// `/readyz` body + head of `/status`: the state, `detail` its own measure (`null` = none yet)
///
/// - provisioning = index-snapshot bytes, syncing = blocks, serving = last egress window
/// - degraded / down / held = [`DaemonReason`]
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub(crate) enum Readiness {
    Provisioning(Option<SnapshotProgress>),
    Syncing(Option<Catchup>),
    Serving(Option<Egress>),
    Degraded(DaemonReason),
    Down(DaemonReason),
    Held(DaemonReason),
}

impl Readiness {
    /// Every [`Self::label`], for the one-hot gauge
    const LABELS: [&'static str; 6] =
        ["provisioning", "syncing", "serving", "degraded", "down", "held"];

    /// = the serialized `state` tag
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Provisioning(_) => "provisioning",
            Self::Syncing(_) => "syncing",
            Self::Serving(_) => "serving",
            Self::Degraded(_) => "degraded",
            Self::Down(_) => "down",
            Self::Held(_) => "held",
        }
    }

    pub(crate) fn reason(&self) -> Option<DaemonReason> {
        match self {
            Self::Degraded(reason) | Self::Down(reason) | Self::Held(reason) => Some(*reason),
            Self::Provisioning(_) | Self::Syncing(_) | Self::Serving(_) => None,
        }
    }

    /// Serving, or degraded (serving a peer-verified tip no trusted validator vouches for)
    pub(crate) fn ready(&self) -> bool {
        matches!(self, Self::Serving(_) | Self::Degraded(_))
    }
}

/// `height` = served (else handed), `target` = [`GrpcSnapshot::sync_target`]
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Catchup {
    height: u32,
    target: u32,
    blocks_per_sec: Option<f64>,
}

/// Why `down`, `degraded` or `held`
///
/// - `Overloaded` = `zaino_grpc::AdmissionState::overload`
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
    /// Every `label()`, in declaration order
    const LABELS: [&'static str; 6] = [
        "no_validator_reachable",
        "tip_unvouched",
        "runtime_stalled",
        "overloaded",
        "goaway_pending",
        "goaway_sent",
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NoValidatorReachable => "no_validator_reachable",
            Self::TipUnvouched => "tip_unvouched",
            Self::RuntimeStalled => "runtime_stalled",
            Self::Overloaded(_) => "overloaded",
            Self::GoawayPending => "goaway_pending",
            Self::GoawaySent => "goaway_sent",
        }
    }

    pub(crate) const fn message(self) -> &'static str {
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

/// `{"reason": label(), "message": message()}`
impl Serialize for DaemonReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire {
            reason: &'static str,
            message: &'static str,
        }
        Wire { reason: self.label(), message: self.message() }.serialize(serializer)
    }
}

/// Where the process is: before boot (index-snapshot bootstrap, first validator answer) or booted
enum Stage {
    Starting { bootstrap: Option<SnapshotProgress>, awaiting_validator: bool },
    Booted(Booted),
}

/// One snapshot's facts: `height` = served (else handed), `target` = [`GrpcSnapshot::sync_target`]
struct Booted {
    serving_status: ServingStatus,
    overload: Option<OverloadCause>,
    height: Option<u32>,
    target: Option<u32>,
    rate: Option<f64>,
    egress: Option<Egress>,
}

impl Booted {
    fn of(sources: &Sources, grpc_snapshot: &GrpcSnapshot<DiskView>) -> Self {
        let served = grpc_snapshot.tips().served.map(|tip| tip.height);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        let now = now.map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
        Self {
            serving_status: grpc_snapshot.serving_status(now),
            overload: sources.grpc_admission.overload(),
            height: served.or(sources.progress.handed()).map(u32::from),
            target: grpc_snapshot.sync_target().map(u32::from),
            rate: *sources.block_rate.borrow(),
            egress: *sources.egress.borrow(),
        }
    }
}

/// Pure: the state rules over the hold + heartbeat + stage (held > the rest)
fn readiness(hold: Hold, live: bool, stage: Stage) -> Readiness {
    match hold {
        Hold::Off => {}
        Hold::Draining => return Readiness::Held(DaemonReason::GoawayPending),
        Hold::Drained => return Readiness::Held(DaemonReason::GoawaySent),
    }
    match (live, stage) {
        (false, Stage::Booted(_)) => Readiness::Down(DaemonReason::RuntimeStalled),
        (_, Stage::Starting { awaiting_validator: true, .. }) => {
            Readiness::Down(DaemonReason::NoValidatorReachable)
        }
        (_, Stage::Starting { bootstrap, .. }) => Readiness::Provisioning(bootstrap),
        (true, Stage::Booted(Booted { overload: Some(cause), .. })) => {
            Readiness::Down(DaemonReason::Overloaded(cause))
        }
        (true, Stage::Booted(booted)) => match booted.serving_status {
            ServingStatus::Down => Readiness::Down(DaemonReason::TipUnvouched),
            ServingStatus::Degraded => Readiness::Degraded(DaemonReason::TipUnvouched),
            ServingStatus::Serving => Readiness::Serving(booted.egress),
            ServingStatus::Syncing => Readiness::Syncing(booted.target.map(|target| Catchup {
                height: booted.height.unwrap_or(0),
                target,
                blocks_per_sec: booted.rate,
            })),
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
            bootstrap: watch::Sender::new(None),
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

    /// Operator hold (`command::hold`, from its marker): `held` above every other state, and the gRPC
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

    pub(crate) fn current_readiness(&self) -> Readiness {
        let loaded = self.sources.get().map(|sources| (sources, sources.grpc_snapshots.load()));
        self.readiness_at(
            loaded.as_ref().map(|(sources, grpc_snapshot)| (*sources, &**grpc_snapshot)),
        )
    }

    fn readiness_at(&self, booted: Option<(&Sources, &GrpcSnapshot<DiskView>)>) -> Readiness {
        let stage = match booted {
            Some((sources, grpc_snapshot)) => Stage::Booted(Booted::of(sources, grpc_snapshot)),
            None => Stage::Starting {
                bootstrap: self.current_bootstrap(),
                awaiting_validator: self.awaiting_validator.load(Ordering::Relaxed),
            },
        };
        readiness(self.retire.state(), self.live(), stage)
    }

    /// Chainview alarms (none before boot)
    pub(crate) fn alarms(&self) -> Vec<String> {
        let sources = self.sources.get();
        sources.map_or_else(Vec::new, |sources| {
            zaino_grpc_snapshot::notes(&sources.grpc_snapshots.load())
        })
    }

    /// `/readyz`: `(ready, readiness)`
    pub(crate) fn readiness_json(&self) -> (bool, String) {
        let readiness = self.current_readiness();
        (readiness.ready(), serde_json::to_string(&readiness).unwrap_or_default())
    }

    /// Public `/status` body, now
    fn public_status_json(&self) -> String {
        let loaded = self.sources.get().map(|sources| (sources, sources.grpc_snapshots.load()));
        let booted = loaded.as_ref().map(|(sources, grpc_snapshot)| (*sources, &**grpc_snapshot));
        let chain = match booted {
            Some((_, grpc_snapshot)) => Chain::of(grpc_snapshot, self.network),
            None => Chain::unbooted(self.network),
        };
        let status = public_status(self.readiness_at(booted), chain, &self.configured);
        serde_json::to_string(&status).unwrap_or_default()
    }

    /// The state + reason (`/readyz`'s) + every `GrpcSnapshot` gauge (none before boot), set just before a
    /// scrape renders
    pub(crate) fn emit_gauges(&self) {
        emit_state_gauges(&self.current_readiness());
        if let Some(sources) = self.sources.get() {
            zaino_grpc_snapshot::emit_gauges(&sources.grpc_snapshots.load(), &sources.progress);
        }
    }

    /// `None` = bootstrap over (the indexer's own status takes over)
    pub(crate) fn bootstrap(&self, progress: Option<SnapshotProgress>) {
        self.bootstrap.send_replace(progress);
    }

    pub(crate) fn current_bootstrap(&self) -> Option<SnapshotProgress> {
        self.bootstrap.borrow().clone()
    }
}

fn emit_state_gauges(readiness: &Readiness) {
    for state in Readiness::LABELS {
        let now = u8::from(state == readiness.label());
        metrics::gauge!(STATE, "state" => state).set(f64::from(now));
    }
    let reason = readiness.reason().map(DaemonReason::label);
    for label in DaemonReason::LABELS {
        let now = u8::from(Some(label) == reason);
        metrics::gauge!(REASON, "reason" => label).set(f64::from(now));
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

/// Public `/status` (gRPC listener), safe for the open internet (no validator address, peer,
/// disk, mempool or tailnet name)
///
/// - `indexes` = configured index → RPCs it gates (an RPC naming no index needs none)
#[derive(Debug, Serialize, PartialEq)]
struct PublicStatus {
    #[serde(flatten)]
    readiness: Readiness,
    chain: Chain,
    build: Build,
    indexes: BTreeMap<&'static str, Vec<&'static str>>,
}

/// `tip` = verified best, `served` = the height RPCs answer at (both `null` before boot)
#[derive(Debug, Serialize, PartialEq)]
struct Chain {
    network: &'static str,
    tip: Option<PublicBlock>,
    served: Option<u32>,
    validators: Option<ValidatorCounts>,
    alarms: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct PublicBlock {
    height: u32,
    hash: String,
}

/// `answering` = live or catching up
#[derive(Debug, Serialize, PartialEq)]
struct ValidatorCounts {
    trusted: usize,
    answering: usize,
}

#[derive(Debug, Serialize, PartialEq)]
struct Build {
    version: &'static str,
}

impl Chain {
    fn of(grpc_snapshot: &GrpcSnapshot<DiskView>, network: NetworkType) -> Self {
        let tips = grpc_snapshot.tips();
        let trusted_validators = grpc_snapshot.chain_view().trusted_validators();
        let answering = |meta: &&zaino_chainview::TrustedValidatorMeta| {
            matches!(meta.health, Health::Live | Health::CatchingUp)
        };
        Self {
            network: crate::config::network_name(network),
            tip: tips.best.map(|best| PublicBlock {
                height: u32::from(best.height),
                hash: best.hash.to_string(),
            }),
            served: tips.served.map(|served| u32::from(served.height)),
            validators: Some(ValidatorCounts {
                trusted: trusted_validators.len(),
                answering: trusted_validators.iter().filter(answering).count(),
            }),
            alarms: zaino_grpc_snapshot::notes(grpc_snapshot),
        }
    }

    fn unbooted(network: NetworkType) -> Self {
        let network = crate::config::network_name(network);
        Self { network, tip: None, served: None, validators: None, alarms: Vec::new() }
    }
}

fn public_status(readiness: Readiness, chain: Chain, configured: &[IndexKind]) -> PublicStatus {
    let gated = |kind: &IndexKind| (kind.name(), zaino_grpc::rpcs_gated_by(*kind).collect());
    PublicStatus {
        readiness,
        chain,
        build: Build { version: env!("CARGO_PKG_VERSION") },
        indexes: configured.iter().map(gated).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::index_snapshot::SnapshotPhase;

    /// Every state rule, first match wins, as `/readyz` serializes it
    ///
    /// - `state` = its [`Readiness::label`], `ready` ⇔ serving or degraded
    /// - held = the operator's intent over every condition, detail = its GOAWAYs
    #[test]
    fn the_state_is_the_first_matching_rule_with_its_own_detail() {
        const LIVE: bool = true;
        const STALLED: bool = false;
        let stage = |serving_status, overload, target: Option<u32>, rate: Option<f64>| {
            let (height, egress) =
                (Some(3_514_194), Some(Egress { start: 1_790_000_400, mbps: 12.5 }));
            Stage::Booted(Booted { serving_status, overload, height, target, rate, egress })
        };
        let booted = |serving_status, target, rate| stage(serving_status, None, target, rate);
        let saturated = Some(OverloadCause::CpuPressure);
        let overloaded =
            |serving_status| stage(serving_status, saturated, Some(3_515_406), Some(6.2));
        let starting =
            |bootstrap, awaiting_validator| Stage::Starting { bootstrap, awaiting_validator };
        let because = |reason: &str, message: &str| serde_json::json!({ "reason": reason, "message": message });
        let stalled = because("runtime_stalled", "Runtime stalled");
        let saturated = because("overloaded", "Overloaded: CPU pressure");
        let unvouched = because("tip_unvouched", "No trusted validator vouches for the tip");
        let downloading = SnapshotProgress {
            phase: SnapshotPhase::Downloading,
            bytes: 4 << 30,
            total_bytes: 9 << 30,
            bytes_per_sec: Some(50_000_000),
        };
        let cases = [
            (
                "plain startup",
                Hold::Off,
                LIVE,
                starting(None, false),
                serde_json::json!({ "state": "provisioning", "detail": null }),
            ),
            (
                "bootstrap downloading",
                Hold::Off,
                LIVE,
                starting(Some(downloading), false),
                serde_json::json!({
                    "state": "provisioning",
                    "detail": {
                        "phase": "downloading", "bytes": 4u64 << 30, "total_bytes": 9u64 << 30,
                        "bytes_per_sec": 50_000_000,
                    },
                }),
            ),
            (
                "boot waiting on a validator",
                Hold::Off,
                LIVE,
                starting(None, true),
                serde_json::json!({
                    "state": "down",
                    "detail": because("no_validator_reachable", "No trusted validator reachable"),
                }),
            ),
            (
                "runtime stalled beats serving",
                Hold::Off,
                STALLED,
                booted(ServingStatus::Serving, Some(3_514_194), None),
                serde_json::json!({ "state": "down", "detail": stalled }),
            ),
            (
                "runtime stalled beats overloaded",
                Hold::Off,
                STALLED,
                overloaded(ServingStatus::Serving),
                serde_json::json!({ "state": "down", "detail": stalled }),
            ),
            (
                "runtime stalled beats degraded",
                Hold::Off,
                STALLED,
                booted(ServingStatus::Degraded, None, None),
                serde_json::json!({ "state": "down", "detail": stalled }),
            ),
            (
                "overloaded beats syncing",
                Hold::Off,
                LIVE,
                overloaded(ServingStatus::Syncing),
                serde_json::json!({ "state": "down", "detail": saturated }),
            ),
            (
                "overloaded beats down",
                Hold::Off,
                LIVE,
                overloaded(ServingStatus::Down),
                serde_json::json!({ "state": "down", "detail": saturated }),
            ),
            (
                "overloaded beats degraded",
                Hold::Off,
                LIVE,
                overloaded(ServingStatus::Degraded),
                serde_json::json!({ "state": "down", "detail": saturated }),
            ),
            (
                "unvouched tip, fresh: peers carried it",
                Hold::Off,
                LIVE,
                booted(ServingStatus::Degraded, Some(3_514_194), None),
                serde_json::json!({ "state": "degraded", "detail": unvouched }),
            ),
            (
                "unvouched tip, stale",
                Hold::Off,
                LIVE,
                booted(ServingStatus::Down, Some(3_515_406), Some(6.2)),
                serde_json::json!({ "state": "down", "detail": unvouched }),
            ),
            (
                "syncing",
                Hold::Off,
                LIVE,
                booted(ServingStatus::Syncing, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "syncing",
                    "detail": { "height": 3_514_194, "target": 3_515_406, "blocks_per_sec": 6.2 },
                }),
            ),
            (
                "syncing, no header chain yet",
                Hold::Off,
                LIVE,
                booted(ServingStatus::Syncing, None, None),
                serde_json::json!({ "state": "syncing", "detail": null }),
            ),
            (
                "serving: its last egress window",
                Hold::Off,
                LIVE,
                booted(ServingStatus::Serving, Some(3_514_194), Some(1.0)),
                serde_json::json!({
                    "state": "serving", "detail": { "start": 1_790_000_400, "mbps": 12.5 },
                }),
            ),
            (
                "held while serving",
                Hold::Draining,
                LIVE,
                booted(ServingStatus::Serving, None, None),
                serde_json::json!({
                    "state": "held", "detail": because("goaway_pending", "GOAWAY pending"),
                }),
            ),
            (
                "held while overloaded, stalled",
                Hold::Drained,
                STALLED,
                overloaded(ServingStatus::Down),
                serde_json::json!({
                    "state": "held", "detail": because("goaway_sent", "GOAWAY sent"),
                }),
            ),
            (
                "held while booting, no validator",
                Hold::Drained,
                LIVE,
                starting(None, true),
                serde_json::json!({
                    "state": "held", "detail": because("goaway_sent", "GOAWAY sent"),
                }),
            ),
        ];
        // case, hold, heartbeat, stage, `/readyz` body
        for (case, hold, live, stage, expected) in cases {
            let readiness = readiness(hold, live, stage);
            assert_eq!(serde_json::to_value(&readiness).expect("serializes"), expected, "{case}");
            let state = expected["state"].as_str().expect("a state tag");
            assert_eq!(
                readiness.label(),
                state,
                "{case}: label (gauge, state log) = /readyz state"
            );
            let ready = ["serving", "degraded"].contains(&state);
            assert_eq!(readiness.ready(), ready, "{case}: /readyz 200 ⇔ serving or degraded");
        }
    }

    /// Public `/status`: before boot (chain facts `null`), then booted with a tip, one of two
    /// validators answering; never a validator address, peer, disk or mempool
    #[test]
    fn the_public_status_nests_state_chain_build_and_index_gated_rpcs() {
        let configured = [IndexKind::TreeState, IndexKind::UtxoSet];
        let indexes = serde_json::json!({
            "tree_state": ["GetTreeState", "GetLatestTreeState", "GetSubtreeRoots"],
            "utxo_set": [],
        });
        let build = serde_json::json!({ "version": env!("CARGO_PKG_VERSION") });

        let before = public_status(
            Readiness::Provisioning(None),
            Chain::unbooted(NetworkType::Main),
            &configured,
        );
        assert_eq!(
            serde_json::to_value(before).expect("serializes"),
            serde_json::json!({
                "state": "provisioning", "detail": null,
                "chain": {
                    "network": "mainnet", "tip": null, "served": null, "validators": null,
                    "alarms": [],
                },
                "build": build, "indexes": indexes,
            }),
            "before boot",
        );

        let chain = Chain {
            network: "mainnet",
            tip: Some(PublicBlock { height: 3_515_406, hash: "00ab".to_owned() }),
            served: Some(3_515_400),
            validators: Some(ValidatorCounts { trusted: 2, answering: 1 }),
            alarms: vec!["1 validator on a stale tip".to_owned()],
        };
        let serving = Readiness::Serving(Some(Egress { start: 1_790_000_400, mbps: 12.5 }));
        let after = public_status(serving, chain, &configured);
        assert_eq!(
            serde_json::to_value(after).expect("serializes"),
            serde_json::json!({
                "state": "serving", "detail": { "start": 1_790_000_400, "mbps": 12.5 },
                "chain": {
                    "network": "mainnet", "tip": { "height": 3_515_406, "hash": "00ab" },
                    "served": 3_515_400, "validators": { "trusted": 2, "answering": 1 },
                    "alarms": ["1 validator on a stale tip"],
                },
                "build": build, "indexes": indexes,
            }),
            "booted, serving",
        );
    }

    /// `zaino_state` + `zaino_state_reason` = `/readyz`'s state + detail reason, each one-hot
    #[test]
    fn the_state_gauges_are_readyz_one_hot() {
        let (recorder, rendered) = crate::telemetry::metrics::recorder();
        let _recording = metrics::set_default_local_recorder(&recorder);
        // the series at 1 (each family's other series checked at 0)
        let set = |family: &str, key: &str, labels: &[&str]| -> Vec<String> {
            let text = rendered.render();
            let mut ones = Vec::new();
            for label in labels {
                let series = format!("{family}{{{key}=\"{label}\"}} ");
                let value = text.lines().find_map(|line| line.strip_prefix(&series));
                match value {
                    Some("1") => ones.push((*label).to_owned()),
                    Some("0") => {}
                    other => panic!("{series}= {other:?}, want 0 or 1 in:\n{text}"),
                }
            }
            ones
        };
        let scraped = || {
            let state = set("zaino_state", "state", &Readiness::LABELS);
            (state, set("zaino_state_reason", "reason", &DaemonReason::LABELS))
        };

        let status = DaemonStatus::new(NetworkType::Regtest, []);
        status.emit_gauges();
        let (state, reason) = scraped();
        assert_eq!(state, ["provisioning"], "not booted: zaino_state");
        assert_eq!(reason, Vec::<String>::new(), "not booted: zaino_state_reason");

        let serving = Readiness::Serving(Some(Egress { start: 1_790_000_400, mbps: 12.5 }));
        let held = Readiness::Held(DaemonReason::GoawaySent);
        #[rustfmt::skip]
        let cases = [
            // case,             readiness,                                     state,       reason
            ("serving",          serving,                                       "serving",   None),
            ("degraded",         Readiness::Degraded(DaemonReason::TipUnvouched), "degraded", Some("tip_unvouched")),
            ("held, GOAWAY'd",   held,                                          "held",      Some("goaway_sent")),
        ];
        for (case, readiness, expected_state, expected_reason) in cases {
            emit_state_gauges(&readiness);
            let (state, reason) = scraped();
            assert_eq!(state, [expected_state], "{case}: zaino_state");
            assert_eq!(reason, Vec::from_iter(expected_reason), "{case}: zaino_state_reason");
        }
    }
}
