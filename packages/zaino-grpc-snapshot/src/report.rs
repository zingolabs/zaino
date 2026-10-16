//! Reporting from one snapshot (G9): gauges at scrape ([`emit_gauges`]), alarm [`notes`], edge
//! logs per publish ([`transitions`])
//!
//! - State = f(snapshot, `SyncProgress`): nothing mirrored between scrapes
//! - Gauge names = ztest's `zainod` families (a rename breaks its sync probes)

use tracing::{info, warn};
use zaino_chainview::{Agreement, Alarm, ChainViewSnapshot, MempoolState};

use zaino_persistence::View;
use zaino_primitives::metric::Metric;
use zaino_sync::SyncProgress;
use zaino_traffic::{Health, TrustedValidatorId};

use crate::grpc_snapshot::GrpcSnapshot;

mod names {
    pub(super) const BEST_TIP: &str = "zaino.best_tip";
    pub(super) const FETCH_HEIGHT: &str = "zaino.fetch_height";
    pub(super) const INDEX_SYNCED: &str = "zaino.index.synced";
    pub(super) const ENDPOINT_STATE: &str = "zaino.chainview.endpoint_state";
    pub(super) const AGREEMENT: &str = "zaino.chainview.agreement";
    pub(super) const TIP_HEIGHT: &str = "zaino.chainview.tip_height";
    pub(super) const STALE_BLOCKS: &str = "zaino.chainview.stale_blocks";
    pub(super) const PEERS: &str = "zaino.chainview.peers";
    pub(super) const PUSH_STREAM: &str = "zaino.chainview.push_stream";
    pub(super) const RELEASE: &str = "zaino.chainview.release";
    pub(super) const END_OF_SERVICE_HEIGHT: &str = "zaino.chainview.end_of_service_height";
    pub(super) const TIP_HOLDERS: &str = "zaino.chainview.tip_holders";
    pub(super) const FINALITY_PAUSED: &str = "zaino.chainview.finality_paused";
    pub(super) const SHARED_OUTBOUND_MIN: &str = "zaino.chainview.shared_outbound_min";
    pub(super) const MEMPOOL_TRANSACTIONS: &str = "zaino.chainview.mempool_transactions";
}

const STATES: [Health; 5] =
    [Health::Pending, Health::Live, Health::Degraded, Health::Down, Health::CatchingUp];

const AGREEMENTS: [Agreement; 5] = [
    Agreement::Unknown,
    Agreement::Agreed,
    Agreement::Ahead,
    Agreement::Behind,
    Agreement::Diverged,
];

const PARTITIONED: &str = "Two live validators share no outbound peer (possible partition)";
const ECLIPSED: &str = "Live validators reach few distinct outbound peers (possible eclipse)";
const FINALITY_PAUSED: &str = "Finality paused: no trusted validator holds the boundary";

/// One sentence per raised alarm (zainod's `/status` `notes`), `Alarm` order
pub fn notes<V>(grpc_snapshot: &GrpcSnapshot<V>) -> Vec<String> {
    let counts = grpc_snapshot.chain_view().alarms().counts();
    counts.filter(|(_, count)| *count > 0).map(|(alarm, count)| note(alarm, count)).collect()
}

/// `count` = raised instances (validators raising it, 1 for a view-wide alarm)
fn note(alarm: Alarm, count: usize) -> String {
    let counted = |one: &str, many: &str| match count {
        1 => format!("1 {one}"),
        count => format!("{count} {many}"),
    };
    match alarm {
        Alarm::Partitioned => PARTITIONED.to_owned(),
        Alarm::Eclipsed => ECLIPSED.to_owned(),
        Alarm::FinalityPaused => FINALITY_PAUSED.to_owned(),
        Alarm::Stale => counted("validator on a stale tip", "validators on a stale tip"),
        Alarm::Ending => counted(
            "validator release reaches end of service within a week",
            "validator releases reach end of service within a week",
        ),
    }
}

/// Every gauge [`emit_gauges`] sets
pub const METRICS: &[Metric] = {
    use names::*;

    &[
        Metric::gauge(BEST_TIP, "Height of the header chain's most-work verified block"),
        Metric::gauge(
            FETCH_HEIGHT,
            "Last finalized block height the follower sent to the index writers",
        ),
        Metric::gauge(
            INDEX_SYNCED,
            "1 = served at the verified tip, 0 = the served tip trails it (still syncing), by index",
        ),
        Metric::gauge(
            ENDPOINT_STATE,
            "1 on the endpoint's health as of its last poll, by endpoint",
        ),
        Metric::gauge(
            AGREEMENT,
            "1 on where the endpoint's chain stands against the verified best block, by endpoint",
        ),
        Metric::gauge(TIP_HEIGHT, "Endpoint's own best-chain tip height, by endpoint"),
        Metric::gauge(
            STALE_BLOCKS,
            "Blocks the endpoint's tip trails its own clock-based network estimate, by endpoint",
        ),
        Metric::gauge(PEERS, "Endpoint's getpeerinfo connections, by endpoint and direction"),
        Metric::gauge(PUSH_STREAM, "1 while the endpoint's indexer push streams are up"),
        Metric::gauge(RELEASE, "1 on the endpoint's release, by endpoint, build and user agent"),
        Metric::gauge(
            END_OF_SERVICE_HEIGHT,
            "Height past which the endpoint's release halts (mainnet; absent = not enforced/unknown)",
        ),
        Metric::gauge(
            TIP_HOLDERS,
            "Trusted validators holding the verified best block (0 = no tip: nothing served)",
        ),
        Metric::gauge(
            names::FINALITY_PAUSED,
            "1 while the finalized boundary waits for a trusted validator to hold it",
        ),
        Metric::gauge(
            SHARED_OUTBOUND_MIN,
            "Fewest outbound peers any two live endpoints share; 0 = possible partition",
        ),
        Metric::gauge(
            MEMPOOL_TRANSACTIONS,
            "Held mempool transactions, by state (verified = a trusted listing; quorum = a peer \
             quorum's, no trusted listing; ours_unverified = our relay, neither)",
        ),
    ]
};

/// Every gauge from `grpc_snapshot` + `progress`, set at scrape (fresh, no mirror tasks)
pub fn emit_gauges<V: View>(grpc_snapshot: &GrpcSnapshot<V>, progress: &SyncProgress) {
    let tips = grpc_snapshot.tips();
    if let Some(best) = tips.best {
        metrics::gauge!(names::BEST_TIP).set(f64::from(u32::from(best.height)));
    }
    if let Some(handed) = progress.handed() {
        metrics::gauge!(names::FETCH_HEIGHT).set(f64::from(u32::from(handed)));
    }
    for (kind, _) in grpc_snapshot.indexed().into_iter().flat_map(|indexed| indexed.durable()) {
        metrics::gauge!(names::INDEX_SYNCED, "index" => kind.name()).set(f64::from(tips.synced));
    }
    chain_view_gauges(grpc_snapshot.chain_view());
}

/// `zaino.chainview.*`: per endpoint, then the view's
fn chain_view_gauges(view: &ChainViewSnapshot) {
    for state in [MempoolState::Verified, MempoolState::Quorum, MempoolState::Ours] {
        let held = view.mempool_states().filter(|each| *each == state).count();
        metrics::gauge!(names::MEMPOOL_TRANSACTIONS, "state" => state.label()).set(held as f64);
    }

    for meta in view.trusted_validators().iter() {
        let endpoint = meta.address.clone();
        for state in STATES {
            let labels = [("endpoint", endpoint.clone()), ("state", state.label().to_owned())];
            metrics::gauge!(names::ENDPOINT_STATE, &labels).set(f64::from(meta.health == state));
        }
        for agreement in AGREEMENTS {
            let label = agreement.label().to_owned();
            let labels = [("endpoint", endpoint.clone()), ("agreement", label)];
            metrics::gauge!(names::AGREEMENT, &labels).set(f64::from(meta.agreement == agreement));
        }
        if let Some(tip) = meta.tip() {
            metrics::gauge!(names::TIP_HEIGHT, "endpoint" => endpoint.clone())
                .set(f64::from(u32::from(tip.height)));
        }
        if let Some(behind) = meta.stale_blocks() {
            metrics::gauge!(names::STALE_BLOCKS, "endpoint" => endpoint.clone())
                .set(f64::from(behind));
        }
        let peers = meta.telemetry.peers.as_deref().unwrap_or_default();
        let inbound = peers.iter().filter(|peer| peer.inbound).count();
        for (direction, count) in [("inbound", inbound), ("outbound", peers.len() - inbound)] {
            metrics::gauge!(names::PEERS, "endpoint" => endpoint.clone(), "direction" => direction)
                .set(count as f64);
        }
        metrics::gauge!(names::PUSH_STREAM, "endpoint" => endpoint.clone())
            .set(f64::from(meta.streaming));
        if let Some(release) = &meta.telemetry.release {
            let labels = [
                ("endpoint", endpoint.clone()),
                ("build", release.build.clone()),
                ("user_agent", release.user_agent.clone()),
            ];
            metrics::gauge!(names::RELEASE, &labels).set(1.0);
            if let zaino_source::EndOfService::At { height, .. } = release.end_of_service {
                metrics::gauge!(names::END_OF_SERVICE_HEIGHT, "endpoint" => endpoint)
                    .set(f64::from(u32::from(height)));
            }
        }
    }
    metrics::gauge!(names::TIP_HOLDERS).set(view.held_by().count() as f64);
    if let Some(shared) = view.shared_outbound_min() {
        metrics::gauge!(names::SHARED_OUTBOUND_MIN).set(shared as f64);
    }
    let paused = view.alarms().raised(Alarm::FinalityPaused);
    metrics::gauge!(names::FINALITY_PAUSED).set(f64::from(paused));
}

/// Edge logs between two consecutive publishes: `synced`, then each alarm that rose or cleared
pub(crate) fn transitions<V>(prev: &GrpcSnapshot<V>, next: &GrpcSnapshot<V>) {
    let height = next.tips().served.map_or(0, |tip| u32::from(tip.height));
    match (prev.tips().synced, next.tips().synced) {
        (false, true) => info!(height, "Serving the verified tip"),
        (true, false) => info!(height, "Behind the verified tip, syncing"),
        _ => {}
    }
    alarm_edges(prev.chain_view(), next.chain_view());
}

fn alarm_edges(prev: &ChainViewSnapshot, next: &ChainViewSnapshot) {
    let (was, now) = (prev.alarms(), next.alarms());
    for (alarm, by) in now.since(was) {
        edge(next, alarm, by, true);
    }
    for (alarm, by) in was.since(now) {
        edge(next, alarm, by, false);
    }
}

/// One alarm rising (`raised`) or clearing, logged against `view` (`by` = its validator)
fn edge(view: &ChainViewSnapshot, alarm: Alarm, by: Option<TrustedValidatorId>, raised: bool) {
    let meta = by.and_then(|by| view.trusted_validators().get(by.get()));
    let validator = meta.map(|meta| meta.address.as_str());
    let tip = meta.and_then(|meta| meta.tip()).map(|tip| u32::from(tip.height));
    let best = view.best().map(|best| u32::from(best.height));
    match (alarm, raised) {
        (Alarm::Partitioned, true) => warn!("{PARTITIONED}"),
        (Alarm::Partitioned, false) => info!("Live validators share outbound peers again"),
        (Alarm::Eclipsed, true) => warn!("{ECLIPSED}"),
        (Alarm::Eclipsed, false) => {
            info!("Live validators reach enough distinct outbound peers again")
        }
        (Alarm::FinalityPaused, true) => warn!(?best, "{FINALITY_PAUSED}"),
        (Alarm::FinalityPaused, false) => info!(?best, "Finality resumed"),
        (Alarm::Stale, true) => {
            let behind = meta.and_then(|meta| meta.stale_blocks());
            warn!(
                validator,
                ?tip,
                ?behind,
                "Validator tip stale against its own clock (stalled or eclipsed)"
            )
        }
        (Alarm::Stale, false) => info!(validator, ?tip, "Validator tip fresh again"),
        (Alarm::Ending, raised) => {
            let release = meta.and_then(|meta| meta.telemetry.release.as_ref());
            let build = release.map(|release| release.build.as_str());
            if raised {
                let left = meta.and_then(|meta| meta.blocks_to_end_of_service());
                warn!(
                    validator,
                    ?build,
                    ?left,
                    "Validator release reaches end of service soon (it halts there): upgrade it"
                )
            } else {
                info!(validator, ?build, "Validator release upgraded")
            }
        }
    }
}
