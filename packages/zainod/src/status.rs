//! `/statusz`, `/readyz` + the `/metrics` gauges: each from one global snapshot load (G9)
//!
//! - Sources filled once by `indexer::boot` (admin thread starts first → `provisioning` until then)
//! - `/statusz` = [`Readiness`] + process fields + `zaino_snapshot::Report`; owners' live tables
//!   read at request
//! - `ready` ⇔ `state` = serving or degraded ⇔ `/readyz` 200 (degraded = a fresh peer-verified
//!   tip served, no trusted validator vouching for it)

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
use zaino_snapshot::{Down, Report, Snapshot, Snapshots, Standing};
use zaino_sync::SyncProgress;
use zaino_traffic::{Health, MemberTable};
use zcash_protocol::consensus::NetworkType;

use crate::egress::Egress;
use crate::progress::Disk;

static SOURCES: OnceLock<Sources> = OnceLock::new();

/// Config's network + enabled indexes, set before the snapshot bootstrap (stub + [`Report`] +
/// public `/status` read one set)
static CONFIGURED: OnceLock<(NetworkType, Vec<IndexKind>)> = OnceLock::new();

/// Once per process, right after config validation
pub(crate) fn configure(network: NetworkType, kinds: impl IntoIterator<Item = IndexKind>) {
    let _ = CONFIGURED.set((network, kinds.into_iter().collect()));
}

fn configured() -> &'static [IndexKind] {
    CONFIGURED.get().map_or(&[], |(_, kinds)| kinds.as_slice())
}

fn configured_network() -> Option<NetworkType> {
    CONFIGURED.get().map(|(network, _)| *network)
}

/// Set once on the shutdown signal, never cleared (the process is exiting)
static DRAINING: AtomicBool = AtomicBool::new(false);

/// From here on the state is `draining` (first match)
pub(crate) fn drain() {
    DRAINING.store(true, Ordering::Relaxed);
}

pub(crate) fn draining() -> bool {
    DRAINING.load(Ordering::Relaxed)
}

/// Operator hold (`hold.rs`, from its marker): `held` above everything but draining
static HELD: AtomicBool = AtomicBool::new(false);

pub(crate) fn hold(held: bool) {
    HELD.store(held, Ordering::Relaxed);
}

pub(crate) fn held() -> bool {
    HELD.load(Ordering::Relaxed)
}

/// Boot waiting on its first validator answer (the upgrade schedule): `down`, not `provisioning`
static AWAITING_VALIDATOR: AtomicBool = AtomicBool::new(false);

pub(crate) fn awaiting_validator(awaiting: bool) {
    AWAITING_VALIDATOR.store(awaiting, Ordering::Relaxed);
}

/// - `members` = the balancer's (latency, failures, health); `disk` = the progress task's last walk
/// - `block_rate` = blocks handed per second over its last ~60 s (`None` before its first tick)
/// - `grpc_load` = the gRPC work pool's (`/statusz` `load`, `Overloaded`)
/// - `egress` = the last closed 10-min window's (public `/status`)
pub(crate) struct Sources {
    pub(crate) network: &'static str,
    pub(crate) started: Instant,
    pub(crate) snapshots: Snapshots<DiskView>,
    pub(crate) progress: SyncProgress,
    pub(crate) members: watch::Receiver<Arc<MemberTable>>,
    pub(crate) disk: watch::Receiver<Disk>,
    pub(crate) block_rate: watch::Receiver<Option<f64>>,
    pub(crate) grpc_load: Arc<zaino_grpc::Load>,
    pub(crate) egress: watch::Receiver<Option<Egress>>,
}

/// Closed set; first match: draining, not booted, runtime stalled, the snapshot's [`Standing`]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Provisioning,
    Syncing,
    Serving,
    Degraded,
    Draining,
    Held,
    Down,
}

impl State {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Syncing => "syncing",
            Self::Serving => "serving",
            Self::Degraded => "degraded",
            Self::Draining => "draining",
            Self::Held => "held",
            Self::Down => "down",
        }
    }
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
    #[cfg(feature = "snapshot")]
    Byte,
}

/// Head of `/statusz` (`/readyz` = `state` + `detail`)
///
/// - `detail` only when `down` or `degraded`, `progress` only while provisioning or syncing,
///   `notes` = raised alarms
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Readiness {
    state: State,
    progress: Option<Progress>,
    detail: Option<&'static str>,
    notes: Vec<String>,
}

impl Readiness {
    /// Serving, or degraded (serving a peer-verified tip no trusted validator vouches for)
    pub(crate) fn ready(&self) -> bool {
        matches!(self.state, State::Serving | State::Degraded)
    }

    /// systemd `STATUS=`: the state, `: detail` when down or degraded
    pub(crate) fn summary(&self) -> String {
        match self.detail {
            Some(detail) => format!("{}: {detail}", self.state.label()),
            None => self.state.label().to_owned(),
        }
    }
}

const RUNTIME_STALLED: &str = "Runtime stalled";

/// ≥ 10 % of gRPC work refused for 5 min (`zaino_grpc::LoadReport::overloaded`)
const OVERLOADED: &str = "Overloaded";

/// Where the process is: before boot (snapshot bootstrap, first validator answer) or booted
enum Stage {
    Starting { bootstrap: Option<Progress>, awaiting_validator: bool },
    Booted(Booted),
}

/// One snapshot's facts: `done` = served height (else handed), `target` = [`Snapshot::sync_target`]
struct Booted {
    standing: Standing,
    overloaded: bool,
    done: Option<u32>,
    target: Option<u32>,
    rate: Option<f64>,
    notes: Vec<String>,
}

impl Booted {
    fn of(sources: &Sources, snap: &Snapshot<DiskView>) -> Self {
        let served = snap.tips().served.map(|tip| tip.height);
        let (progress, rate) = (&sources.progress, *sources.block_rate.borrow());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        let now = now.map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
        Self {
            standing: snap.standing(now),
            overloaded: sources.grpc_load.report().overloaded,
            done: served.or(progress.handed()).map(u32::from),
            target: snap.sync_target().map(u32::from),
            rate,
            notes: zaino_snapshot::notes(snap),
        }
    }
}

/// Pure: the state rules over the process flags + stage (draining > held > the rest)
fn readiness(draining: bool, held: bool, live: bool, stage: Stage) -> Readiness {
    let at = |state, progress, detail, notes| Readiness { state, progress, detail, notes };
    if held && !draining {
        let notes = match stage {
            Stage::Booted(booted) => booted.notes,
            Stage::Starting { .. } => Vec::new(),
        };
        return at(State::Held, None, None, notes);
    }
    match (draining, live, stage) {
        (true, _, Stage::Booted(booted)) => at(State::Draining, None, None, booted.notes),
        (true, _, Stage::Starting { .. }) => at(State::Draining, None, None, Vec::new()),
        (false, false, Stage::Booted(booted)) => {
            at(State::Down, None, Some(RUNTIME_STALLED), booted.notes)
        }
        (false, _, Stage::Starting { awaiting_validator: true, .. }) => {
            at(State::Down, None, Some(Down::NoValidatorReachable.detail()), Vec::new())
        }
        (false, _, Stage::Starting { bootstrap, .. }) => {
            at(State::Provisioning, bootstrap, None, Vec::new())
        }
        (false, true, Stage::Booted(booted)) if booted.overloaded => {
            at(State::Down, None, Some(OVERLOADED), booted.notes)
        }
        (false, true, Stage::Booted(booted)) => match booted.standing {
            Standing::Down(down) => at(State::Down, None, Some(down.detail()), booted.notes),
            Standing::Degraded(why) => at(State::Degraded, None, Some(why.detail()), booted.notes),
            Standing::Serving => at(State::Serving, None, None, booted.notes),
            Standing::Syncing => {
                let progress = booted.target.map(|total| Progress {
                    done: u64::from(booted.done.unwrap_or(0)),
                    total: u64::from(total),
                    unit: Unit::Block,
                    rate: booted.rate,
                });
                at(State::Syncing, progress, None, booted.notes)
            }
        },
    }
}

/// Now, from the globals
fn current_readiness(live: bool) -> Readiness {
    let stage = match SOURCES.get() {
        Some(sources) => Stage::Booted(Booted::of(sources, &sources.snapshots.load())),
        None => Stage::Starting {
            bootstrap: bootstrap_bytes(),
            awaiting_validator: AWAITING_VALIDATOR.load(Ordering::Relaxed),
        },
    };
    readiness(draining(), held(), live, stage)
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

/// While a bootstrap runs: its bytes (`rate` downloading only)
fn bootstrap_bytes() -> Option<Progress> {
    #[cfg(feature = "snapshot")]
    if let Some(bootstrap) =
        BOOTSTRAP.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
    {
        return Some(Progress {
            done: bootstrap.done,
            total: bootstrap.total,
            unit: Unit::Byte,
            rate: bootstrap.rate.map(|rate| rate as f64),
        });
    }
    None
}

/// While a bootstrap runs: its phase, source, height and archive count, verbatim
pub(crate) fn bootstrap_report() -> Option<serde_json::Value> {
    #[cfg(feature = "snapshot")]
    if let Some(bootstrap) =
        BOOTSTRAP.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    {
        return serde_json::to_value(bootstrap).ok();
    }
    None
}

/// `/statusz` body: readiness + process fields + the snapshot's [`Report`] (field meanings:
/// docs/running.md)
#[derive(Debug, Serialize)]
pub(crate) struct Status {
    version: &'static str,
    network: &'static str,
    uptime_s: u64,
    ready: bool,
    #[serde(flatten)]
    readiness: Readiness,
    load: Load,
    grpc: Grpc,
    disk: Disk,
    #[serde(flatten)]
    report: Report,
}

/// The gRPC work pool: `limit` = admitted at once now (adaptive), `refused_per_s` over ~10 s
#[derive(Debug, Serialize, PartialEq)]
struct Load {
    limit: usize,
    in_flight: usize,
    refused_per_s: f64,
    overloaded: bool,
}

impl From<zaino_grpc::LoadReport> for Load {
    fn from(report: zaino_grpc::LoadReport) -> Self {
        let zaino_grpc::LoadReport { limit, in_flight, refused_per_s, overloaded } = report;
        Self { limit, in_flight, refused_per_s, overloaded }
    }
}

#[derive(Debug, Serialize)]
struct Grpc {
    sent_bytes: u64,
}

/// `None` = not booted
fn current(live: bool) -> Option<Status> {
    let sources = SOURCES.get()?;
    let snap = sources.snapshots.load();
    let readiness = readiness(draining(), held(), live, Stage::Booted(Booted::of(sources, &snap)));
    let members = sources.members.borrow().clone();
    Some(Status {
        version: env!("CARGO_PKG_VERSION"),
        network: sources.network,
        uptime_s: sources.started.elapsed().as_secs(),
        ready: readiness.ready(),
        readiness,
        load: sources.grpc_load.report().into(),
        grpc: Grpc { sent_bytes: zaino_grpc::sent_bytes_total() },
        disk: sources.disk.borrow().clone(),
        report: Report::of(&snap, &sources.progress, &members, configured()),
    })
}

/// `/readyz`: `(ready, {"state", "detail"})`
pub(crate) fn readiness_json(live: bool) -> (bool, String) {
    let readiness = current_readiness(live);
    let body = serde_json::json!({ "state": readiness.state, "detail": readiness.detail });
    (readiness.ready(), body.to_string())
}

/// Public `/status` (gRPC listener): readiness + chain position + counts, safe for the open
/// internet (no validator address, peer, disk, mempool or tailnet name)
#[derive(Debug, Serialize, PartialEq)]
struct PublicStatus {
    #[serde(flatten)]
    readiness: Readiness,
    version: &'static str,
    network: Option<&'static str>,
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
    fn of(sources: &Sources, snap: &Snapshot<DiskView>) -> Self {
        let tips = snap.tips();
        let endpoints = snap.view().endpoints();
        let answering = |meta: &&zaino_chainview::ValidatorMetadata| {
            matches!(meta.health, Health::Live | Health::CatchingUp)
        };
        let info = configured_network().and_then(|network| zaino_grpc::lightd_info(snap, network));
        Self {
            tip: tips.best.map(|best| PublicBlock {
                height: u32::from(best.height),
                hash: best.hash.to_string(),
            }),
            served: tips.served.map(|served| u32::from(served.height)),
            validators: ValidatorCounts {
                configured: endpoints.len(),
                connected: endpoints.iter().filter(answering).count(),
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

/// Public `/status` body, now
fn public_status_json(live: bool) -> String {
    let chain = SOURCES.get().map(|sources| Chain::of(sources, &sources.snapshots.load()));
    let status = public_status(current_readiness(live), configured_network(), chain, configured());
    serde_json::to_string(&status).unwrap_or_default()
}

/// `GET /readyz` + `GET /status` on the gRPC listener (`zaino_grpc::Health`)
pub(crate) struct PublicHealth;

impl zaino_grpc::Health for PublicHealth {
    fn readyz(&self) -> (bool, String) {
        readiness_json(crate::admin::live())
    }

    fn status(&self) -> String {
        public_status_json(crate::admin::live())
    }
}

fn public_status(
    readiness: Readiness,
    network: Option<NetworkType>,
    chain: Option<Chain>,
    configured: &[IndexKind],
) -> PublicStatus {
    let all = zaino_nfs::INDEXES;
    let enabled = all.into_iter().filter(|kind| configured.contains(kind)).map(IndexKind::name);
    let (tip, served, validators, egress, info) = match chain {
        Some(chain) => (chain.tip, chain.served, Some(chain.validators), chain.egress, chain.info),
        None => (None, None, None, None, None),
    };
    PublicStatus {
        readiness,
        version: env!("CARGO_PKG_VERSION"),
        network: network.map(zaino_primitives::network::network_name),
        tip,
        served,
        validators,
        indexes: IndexCounts { enabled: enabled.collect(), total: all.len() },
        egress,
        info,
    }
}

/// Every snapshot gauge, set just before a scrape renders (nothing before boot)
pub(crate) fn emit_gauges() {
    if let Some(sources) = SOURCES.get() {
        zaino_snapshot::emit_gauges(&sources.snapshots.load(), &sources.progress);
    }
}

/// Readiness + a progress fingerprint for [`crate::notify`] (fingerprint moved = startup advanced)
///
/// - `started` = serving or held (a node started held must not fail its start job waiting on a
///   readiness the operator withholds)
/// - `status` = [`Readiness::summary`]
pub(crate) struct Startup {
    pub(crate) started: bool,
    pub(crate) status: String,
    pub(crate) progress: Vec<Option<u64>>,
}

/// Booted: handed + served heights + every index's durable one; before: bootstrap phase + bytes
pub(crate) fn startup(live: bool) -> Startup {
    let readiness = current_readiness(live);
    let started = readiness.ready() || readiness.state == State::Held;
    let status = readiness.summary();
    let Some(sources) = SOURCES.get() else {
        return Startup { started, status, progress: bootstrap_progress() };
    };
    let snap = sources.snapshots.load();
    let durable = snap.indexed().into_iter().flat_map(|indexed| indexed.durable());
    let durable = durable.map(|(_, tip)| tip.map(|tip| tip.height));
    let heights = [sources.progress.handed(), snap.tips().served.map(|tip| tip.height)];
    let progress = heights.into_iter().chain(durable).map(|h| h.map(|h| u64::from(u32::from(h))));
    Startup { started, status, progress: progress.collect() }
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

/// `/statusz`: everything (pre-boot stub + configured `indexes` until boot publishes)
pub(crate) fn status_json(live: bool) -> String {
    match current(live) {
        Some(status) => serde_json::to_string(&status).unwrap_or_default(),
        None => stub(current_readiness(live), bootstrap_report(), configured()).to_string(),
    }
}

/// Before boot: readiness, the configured `indexes` (no `durable` yet), the bootstrap if one runs
fn stub(
    readiness: Readiness,
    bootstrap: Option<serde_json::Value>,
    configured: &[IndexKind],
) -> serde_json::Value {
    let indexes = zaino_snapshot::indexes(configured, &[]);
    let mut stub = serde_json::to_value(&readiness).unwrap_or_default();
    stub["ready"] = readiness.ready().into();
    stub["indexes"] = serde_json::to_value(indexes).unwrap_or_default();
    if let Some(bootstrap) = bootstrap {
        stub["snapshot"] = bootstrap;
    }
    stub
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every state rule, first match wins, as `/statusz` serializes it; across all of them
    /// `ready` ⇔ serving or degraded, `detail` only when down or degraded, `progress` only
    /// provisioning or syncing
    #[test]
    fn the_state_is_the_first_matching_rule_with_its_progress_detail_and_notes() {
        let stage = |standing, overloaded, target: Option<u32>, rate: Option<f64>| {
            let notes = vec!["1 validator on a stale tip".to_owned()];
            let done = Some(3_514_194);
            Stage::Booted(Booted { standing, overloaded, done, target, rate, notes })
        };
        let booted = |standing, target, rate| stage(standing, false, target, rate);
        let overloaded = |standing| stage(standing, true, Some(3_515_406), Some(6.2));
        let starting =
            |bootstrap, awaiting_validator| Stage::Starting { bootstrap, awaiting_validator };
        let stale = serde_json::json!(["1 validator on a stale tip"]);
        let no_holder = Standing::Down(Down::TipNotHeld);
        let unreachable = Standing::Down(Down::NoValidatorReachable);
        let cases = [
            (
                "draining beats serving",
                true,
                true,
                booted(Standing::Serving, Some(3_514_194), None),
                serde_json::json!({ "state": "draining", "progress": null, "detail": null, "notes": stale }),
            ),
            (
                "draining before boot",
                true,
                true,
                starting(None, false),
                serde_json::json!({ "state": "draining", "progress": null, "detail": null, "notes": [] }),
            ),
            (
                "plain startup",
                false,
                true,
                starting(None, false),
                serde_json::json!({ "state": "provisioning", "progress": null, "detail": null, "notes": [] }),
            ),
            (
                "boot waiting on a validator",
                false,
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
                false,
                booted(Standing::Serving, Some(3_514_194), None),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "runtime stalled beats overloaded",
                false,
                false,
                overloaded(Standing::Serving),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "overloaded beats syncing",
                false,
                true,
                overloaded(Standing::Syncing),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded", "notes": stale }),
            ),
            (
                "overloaded beats unreachable validators",
                false,
                true,
                overloaded(unreachable),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded", "notes": stale }),
            ),
            (
                "overloaded beats degraded",
                false,
                true,
                overloaded(Standing::Degraded(Down::NoValidatorReachable)),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Overloaded", "notes": stale }),
            ),
            (
                "runtime stalled beats degraded",
                false,
                false,
                booted(Standing::Degraded(Down::TipNotHeld), None, None),
                serde_json::json!({ "state": "down", "progress": null, "detail": "Runtime stalled", "notes": stale }),
            ),
            (
                "every validator down, a fresh peer-verified tip served",
                false,
                true,
                booted(Standing::Degraded(Down::NoValidatorReachable), Some(3_514_194), None),
                serde_json::json!({
                    "state": "degraded", "progress": null,
                    "detail": "No trusted validator reachable", "notes": stale,
                }),
            ),
            (
                "best unheld, a fresh peer-verified tip served",
                false,
                true,
                booted(Standing::Degraded(Down::TipNotHeld), Some(3_514_194), None),
                serde_json::json!({
                    "state": "degraded", "progress": null,
                    "detail": "No trusted validator holds the verified tip", "notes": stale,
                }),
            ),
            (
                "every validator down",
                false,
                true,
                booted(unreachable, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "down", "progress": null, "detail": "No trusted validator reachable",
                    "notes": stale,
                }),
            ),
            (
                "best unheld",
                false,
                true,
                booted(no_holder, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "down", "progress": null,
                    "detail": "No trusted validator holds the verified tip", "notes": stale,
                }),
            ),
            (
                "syncing",
                false,
                true,
                booted(Standing::Syncing, Some(3_515_406), Some(6.2)),
                serde_json::json!({
                    "state": "syncing", "detail": null, "notes": stale,
                    "progress": { "done": 3_514_194, "total": 3_515_406, "unit": "block", "rate": 6.2 },
                }),
            ),
            (
                "syncing, no header chain yet",
                false,
                true,
                booted(Standing::Syncing, None, None),
                serde_json::json!({ "state": "syncing", "progress": null, "detail": null, "notes": stale }),
            ),
            (
                "serving",
                false,
                true,
                booted(Standing::Serving, Some(3_514_194), Some(1.0)),
                serde_json::json!({ "state": "serving", "progress": null, "detail": null, "notes": stale }),
            ),
        ];
        #[cfg(feature = "snapshot")]
        let cases = cases.into_iter().chain([(
            "bootstrap downloading",
            false,
            true,
            starting(
                Some(Progress { done: 4 << 30, total: 9 << 30, unit: Unit::Byte, rate: Some(5e7) }),
                false,
            ),
            serde_json::json!({
                "state": "provisioning", "detail": null, "notes": [],
                "progress": { "done": 4u64 << 30, "total": 9u64 << 30, "unit": "byte", "rate": 5e7 },
            }),
        )]);
        for (case, draining, live, stage, expected) in cases {
            let readiness = readiness(draining, false, live, stage);
            let state = readiness.state;
            assert_eq!(serde_json::to_value(&readiness).expect("serializes"), expected, "{case}");
            let serving = matches!(state, State::Serving | State::Degraded);
            assert_eq!(readiness.ready(), serving, "{case}: ready ⇔ serving or degraded");
            let detailed = matches!(state, State::Down | State::Degraded);
            assert_eq!(readiness.detail.is_some(), detailed, "{case}: detail ⇔ down or degraded");
            let progressing = matches!(state, State::Provisioning | State::Syncing);
            assert!(readiness.progress.is_none() || progressing, "{case}: progress off-state");
        }

        // held: the operator's intent over every condition but draining, notes kept
        let held = |notes| serde_json::json!({ "state": "held", "progress": null, "detail": null, "notes": notes });
        let held_cases = [
            (
                "held while serving",
                false,
                true,
                booted(Standing::Serving, None, None),
                held(stale.clone()),
            ),
            ("held while overloaded", false, true, overloaded(unreachable), held(stale.clone())),
            (
                "held while degraded",
                false,
                true,
                booted(Standing::Degraded(Down::NoValidatorReachable), None, None),
                held(stale.clone()),
            ),
            (
                "held with a stalled runtime",
                false,
                false,
                booted(Standing::Syncing, None, None),
                held(stale.clone()),
            ),
            (
                "held while booting, no validator",
                false,
                true,
                starting(None, true),
                held(serde_json::json!([])),
            ),
            (
                "draining beats held",
                true,
                true,
                booted(Standing::Serving, None, None),
                serde_json::json!({ "state": "draining", "progress": null, "detail": null, "notes": stale }),
            ),
        ];
        for (case, draining, live, stage, expected) in held_cases {
            let readiness = readiness(draining, true, live, stage);
            assert_eq!(serde_json::to_value(&readiness).expect("serializes"), expected, "{case}");
            assert!(!readiness.ready(), "{case}: never ready");
        }
    }

    /// Before boot `/statusz` already lists every foldable index as configured (compact-block
    /// pair on, the rest off), then the bootstrap verbatim when one runs
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
        let starting = readiness(
            false,
            false,
            true,
            Stage::Starting { bootstrap: None, awaiting_validator: false },
        );
        assert_eq!(
            stub(starting.clone(), None, &configured),
            serde_json::json!({
                "ready": false, "state": "provisioning", "progress": null, "detail": null,
                "notes": [], "indexes": indexes,
            }),
        );
        let bootstrap = serde_json::json!({ "phase": "downloading", "indexes": 2, "total": 9 });
        assert_eq!(
            stub(starting, Some(bootstrap.clone()), &configured),
            serde_json::json!({
                "ready": false, "state": "provisioning", "progress": null, "detail": null,
                "notes": [], "indexes": indexes, "snapshot": bootstrap,
            }),
        );
    }

    /// Public `/status`, field by field: before boot (chain facts `null`), then booted with a
    /// tip, one of two validators answering, an egress window and `GetLightdInfo`; never a
    /// validator address, peer, disk or mempool
    #[test]
    fn the_public_status_projects_readiness_and_counts_only() {
        let configured = [IndexKind::CompactBlock, IndexKind::ValueBalance];
        let starting = Stage::Starting { bootstrap: None, awaiting_validator: false };
        let readiness = |stage| readiness(false, false, true, stage);
        let indexes =
            serde_json::json!({ "enabled": ["value_balance", "compact_block"], "total": 5 });
        let version = env!("CARGO_PKG_VERSION");

        let before = public_status(readiness(starting), Some(NetworkType::Main), None, &configured);
        assert_eq!(
            serde_json::to_value(before).expect("serializes"),
            serde_json::json!({
                "state": "provisioning", "progress": null, "detail": null, "notes": [],
                "version": version, "network": "mainnet", "tip": null, "served": null,
                "validators": null, "indexes": indexes, "egress": null, "info": null,
            }),
        );

        let serving = Stage::Booted(Booted {
            standing: Standing::Serving,
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
        let after =
            public_status(readiness(serving), Some(NetworkType::Main), Some(chain), &configured);
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
