//! Reporting from one snapshot (G9): gauges at scrape ([`emit_gauges`]), alarm [`notes`], edge
//! logs per publish ([`transitions`])
//!
//! - State = f(snapshot, `SyncProgress`): nothing mirrored between scrapes
//! - Gauge names = ztest's `zainod` families (a rename breaks its sync probes)

use tracing::{info, warn};
use zaino_chainview::{Agreement, ChainViewSnapshot, Spread};

use zaino_persistence::View;
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

/// Why one held transaction is held, first match: a trusted listing, a peer quorum, our relay
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MempoolState {
    Verified,
    Quorum,
    Ours,
}

impl MempoolState {
    const fn label(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Quorum => "quorum",
            Self::Ours => "ours_unverified",
        }
    }
}

fn mempool_state(spread: &Spread) -> MempoolState {
    match (spread.trusted.seen > 0, spread.quorum) {
        (true, _) => MempoolState::Verified,
        (false, true) => MempoolState::Quorum,
        (false, false) => MempoolState::Ours,
    }
}

/// Held transactions per [`MempoolState`], one pass
fn mempool(view: &ChainViewSnapshot) -> [(MempoolState, usize); 3] {
    let mut held =
        [(MempoolState::Verified, 0), (MempoolState::Quorum, 0), (MempoolState::Ours, 0)];
    for (_, spread) in view.spreads() {
        let state = mempool_state(&spread);
        if let Some((_, count)) = held.iter_mut().find(|(each, _)| *each == state) {
            *count += 1;
        }
    }
    held
}

const PARTITIONED: &str = "Two live validators share no outbound peer (possible partition)";
const ECLIPSED: &str = "Live validators reach few distinct outbound peers (possible eclipse)";
const FINALITY_PAUSED: &str = "Finality paused: no trusted validator holds the boundary";

/// One sentence per raised alarm (zainod's `/status` `notes`): partition, eclipse, finality
/// paused, stale tips, ending releases
pub fn notes<V>(grpc_snapshot: &GrpcSnapshot<V>) -> Vec<String> {
    let alarms = grpc_snapshot.view().alarms();
    let counted = |count: usize, one: &str, many: &str| match count {
        0 => None,
        1 => Some(format!("1 {one}")),
        count => Some(format!("{count} {many}")),
    };
    let flags = [
        (alarms.partitioned(), PARTITIONED),
        (alarms.eclipsed(), ECLIPSED),
        (alarms.finality_paused(), FINALITY_PAUSED),
    ];
    let flags = flags.into_iter().filter(|(raised, _)| *raised).map(|(_, note)| note.to_owned());
    let stale =
        counted(alarms.stale().count(), "validator on a stale tip", "validators on a stale tip");
    let ending = counted(
        alarms.ending().count(),
        "validator release reaches end of service within a week",
        "validator releases reach end of service within a week",
    );
    flags.chain(stale).chain(ending).collect()
}

/// `# HELP` registrations for every gauge [`emit_gauges`] sets
pub fn describe_metrics() {
    use metrics::describe_gauge;

    describe_gauge!(names::BEST_TIP, "Height of the header chain's most-work verified block");
    describe_gauge!(
        names::FETCH_HEIGHT,
        "Last finalized block height the follower sent to the index writers"
    );
    describe_gauge!(
        names::INDEX_SYNCED,
        "1 = served at the verified tip, 0 = the served tip trails it (still syncing), by index"
    );
    describe_gauge!(
        names::ENDPOINT_STATE,
        "1 on the endpoint's health as of its last poll, by endpoint"
    );
    describe_gauge!(
        names::AGREEMENT,
        "1 on where the endpoint's chain stands against the verified best block, by endpoint"
    );
    describe_gauge!(names::TIP_HEIGHT, "Endpoint's own best-chain tip height, by endpoint");
    describe_gauge!(
        names::STALE_BLOCKS,
        "Blocks the endpoint's tip trails its own clock-based network estimate, by endpoint"
    );
    describe_gauge!(names::PEERS, "Endpoint's getpeerinfo connections, by endpoint and direction");
    describe_gauge!(names::PUSH_STREAM, "1 while the endpoint's indexer push streams are up");
    describe_gauge!(
        names::RELEASE,
        "1 on the endpoint's release, by endpoint, build and user agent"
    );
    describe_gauge!(
        names::END_OF_SERVICE_HEIGHT,
        "Height past which the endpoint's release halts (mainnet; absent = not enforced/unknown)"
    );
    describe_gauge!(
        names::TIP_HOLDERS,
        "Trusted validators holding the verified best block (0 = no tip: nothing served)"
    );
    describe_gauge!(
        names::FINALITY_PAUSED,
        "1 while the finalized boundary waits for a trusted validator to hold it"
    );
    describe_gauge!(
        names::SHARED_OUTBOUND_MIN,
        "Fewest outbound peers any two live endpoints share; 0 = possible partition"
    );
    describe_gauge!(
        names::MEMPOOL_TRANSACTIONS,
        "Held mempool transactions, by state (verified = a trusted listing; quorum = a peer \
         quorum's, no trusted listing; ours_unverified = our relay, neither)"
    );
}

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
    chain_view_gauges(grpc_snapshot.view());
}

/// `zaino.chainview.*`: per endpoint, then the view's
fn chain_view_gauges(view: &ChainViewSnapshot) {
    for (state, held) in mempool(view) {
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
        let inbound = meta.peers.iter().filter(|peer| peer.inbound).count();
        for (direction, count) in [("inbound", inbound), ("outbound", meta.peers.len() - inbound)] {
            metrics::gauge!(names::PEERS, "endpoint" => endpoint.clone(), "direction" => direction)
                .set(count as f64);
        }
        metrics::gauge!(names::PUSH_STREAM, "endpoint" => endpoint.clone())
            .set(f64::from(meta.streaming));
        if let Some(release) = &meta.release {
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
    metrics::gauge!(names::FINALITY_PAUSED).set(f64::from(view.alarms().finality_paused()));
}

/// Edge logs between two consecutive publishes: `synced`, then each alarm that rose or cleared
pub(crate) fn transitions<V>(prev: &GrpcSnapshot<V>, next: &GrpcSnapshot<V>) {
    let height = next.tips().served.map_or(0, |tip| u32::from(tip.height));
    match (prev.tips().synced, next.tips().synced) {
        (false, true) => info!(height, "Serving the verified tip"),
        (true, false) => info!(height, "Behind the verified tip, syncing"),
        _ => {}
    }
    alarm_edges(prev.view(), next.view());
}

fn alarm_edges(prev: &ChainViewSnapshot, next: &ChainViewSnapshot) {
    let (was, now) = (prev.alarms(), next.alarms());
    for (position, meta) in next.trusted_validators().iter().enumerate() {
        let Some(index) = TrustedValidatorId::new(position) else { continue };
        let tip = meta.tip().map(|tip| u32::from(tip.height));
        let behind = meta.stale_blocks();
        match (was.stale().contains(index), now.stale().contains(index)) {
            (false, true) => warn!(
                validator = %meta.address, ?tip, ?behind,
                "Validator tip stale against its own clock (stalled or eclipsed)"
            ),
            (true, false) => info!(validator = %meta.address, ?tip, "Validator tip fresh again"),
            _ => {}
        }
        let left = meta.blocks_to_end_of_service();
        let build = meta.release.as_ref().map(|release| release.build.as_str());
        match (was.ending().contains(index), now.ending().contains(index)) {
            (false, true) => warn!(
                validator = %meta.address, ?build, ?left,
                "Validator release reaches end of service soon (it halts there): upgrade it"
            ),
            (true, false) => info!(validator = %meta.address, ?build, "Validator release upgraded"),
            _ => {}
        }
    }
    match (was.partitioned(), now.partitioned()) {
        (false, true) => warn!("{PARTITIONED}"),
        (true, false) => info!("Live validators share outbound peers again"),
        _ => {}
    }
    match (was.eclipsed(), now.eclipsed()) {
        (false, true) => warn!("{ECLIPSED}"),
        (true, false) => info!("Live validators reach enough distinct outbound peers again"),
        _ => {}
    }
    let best = next.best().map(|best| u32::from(best.height));
    match (was.finality_paused(), now.finality_paused()) {
        (false, true) => warn!(?best, "{FINALITY_PAUSED}"),
        (true, false) => info!(?best, "Finality resumed"),
        _ => {}
    }
}
