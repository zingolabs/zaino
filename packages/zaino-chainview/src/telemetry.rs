//! Endpoint telemetry: gauges each fold, a log line per alarm edge
//!
//! - Observation only: never a vote, never gates serving or sync (a false alarm costs a log line)

use std::collections::HashSet;

use tracing::{info, warn};
use zaino_primitives::types::EndOfService;

use crate::config::{ECLIPSE_OUTBOUND_MAX, END_OF_SERVICE_WARN_BLOCKS, STALE_TIP_BLOCKS};
use crate::endpoints::{Agreement, EndpointIndex, EndpointSet, EndpointState, ValidatorMetadata};
use crate::snapshot::{ChainViewSnapshot, Sighting};
use crate::submit::Ended;

mod names {
    pub(super) const ENDPOINT_STATE: &str = "zaino.chainview.endpoint_state";
    pub(super) const AGREEMENT: &str = "zaino.chainview.agreement";
    pub(super) const TIP_HEIGHT: &str = "zaino.chainview.tip_height";
    pub(super) const STALE_BLOCKS: &str = "zaino.chainview.stale_blocks";
    pub(super) const PEERS: &str = "zaino.chainview.peers";
    pub(super) const TIP_HOLDERS: &str = "zaino.chainview.tip_holders";
    pub(super) const BEST_HEIGHT: &str = "zaino.chainview.best_height";
    pub(super) const SHARED_OUTBOUND_MIN: &str = "zaino.chainview.shared_outbound_min";
    pub(super) const MEMPOOL_TRANSACTIONS: &str = "zaino.chainview.mempool_transactions";
    pub(super) const FIRST_TRUSTED: &str = "zaino.chainview.first_trusted_seconds";
    pub(super) const ALL_TRUSTED: &str = "zaino.chainview.all_trusted_seconds";
    pub(super) const RESIDENCE: &str = "zaino.chainview.residence_seconds";
    pub(super) const RELEASE: &str = "zaino.chainview.release";
    pub(super) const PUSH_STREAM: &str = "zaino.chainview.push_stream";
    pub(super) const SUBMISSIONS: &str = "zaino.chainview.submissions_total";
    pub(super) const SUBMISSION_ATTEMPTS: &str = "zaino.chainview.submission_attempts";
    pub(super) const END_OF_SERVICE_HEIGHT: &str = "zaino.chainview.end_of_service_height";
    pub(super) const FINALITY_PAUSED: &str = "zaino.chainview.finality_paused";
}

/// Sub-second relay hops to hour-long residence
const SPREAD_BUCKETS: &[f64] =
    &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0, 3600.0, 14400.0];

/// Per-histogram bucket edges (installed by the daemon's exporter)
pub const METRIC_BUCKETS: &[(&str, &[f64])] = &[
    (names::FIRST_TRUSTED, SPREAD_BUCKETS),
    (names::ALL_TRUSTED, SPREAD_BUCKETS),
    (names::RESIDENCE, SPREAD_BUCKETS),
    (names::SUBMISSION_ATTEMPTS, &[1.0, 2.0, 3.0, 4.0, 6.0, 8.0]),
];

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    describe_counter!(
        names::SUBMISSIONS,
        "Submissions ended, by outcome (spread = seen past its entries; unconfirmed = accepted, \
         spread unobservable; rejected; unreachable)"
    );
    describe_histogram!(
        names::SUBMISSION_ATTEMPTS,
        "Entries one submission pushed to before it ended, by outcome"
    );

    describe_gauge!(
        names::MEMPOOL_TRANSACTIONS,
        "Held mempool transactions, by state (verified = a trusted listing; ours_unverified = our \
         relay, unlisted)"
    );
    describe_histogram!(
        names::FIRST_TRUSTED,
        metrics::Unit::Seconds,
        "First sighting to first trusted listing, by origin (ours = our relay)"
    );
    describe_histogram!(
        names::ALL_TRUSTED,
        metrics::Unit::Seconds,
        "First trusted listing to every mempool-reading trusted validator listing it"
    );
    describe_histogram!(
        names::RESIDENCE,
        metrics::Unit::Seconds,
        "First sighting to leaving the view, by how (block = at a tip move; unlisted = evicted)"
    );

    describe_gauge!(names::ENDPOINT_STATE, "1 on the endpoint's current poller state, by endpoint");
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
    describe_gauge!(names::BEST_HEIGHT, "Height of the header chain's most-work verified block");
    describe_gauge!(
        names::FINALITY_PAUSED,
        "1 while the final boundary waits for a trusted validator to hold it"
    );
    describe_gauge!(
        names::SHARED_OUTBOUND_MIN,
        "Fewest outbound peers any two live endpoints share; 0 = possible partition"
    );
}

/// Raised conditions; logged on each edge, not each fold
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Alarms {
    stale: EndpointSet,
    ending: EndpointSet,
    partitioned: bool,
    eclipsed: bool,
    finality_paused: bool,
}

impl Alarms {
    /// Live endpoints whose tip trails their own clock estimate by >= `STALE_TIP_BLOCKS`
    pub fn stale(&self) -> EndpointSet {
        self.stale
    }

    /// Endpoints whose release halts within `END_OF_SERVICE_WARN_BLOCKS` of their tip
    pub fn ending(&self) -> EndpointSet {
        self.ending
    }

    /// Two live endpoints share no outbound peer
    pub fn partitioned(&self) -> bool {
        self.partitioned
    }

    /// Live endpoints together reach at most `ECLIPSE_OUTBOUND_MAX` outbound peers
    pub fn eclipsed(&self) -> bool {
        self.eclipsed
    }

    /// A boundary block `depth` deep, not final: no trusted validator holds it (H6)
    pub fn finality_paused(&self) -> bool {
        self.finality_paused
    }
}

/// Outbound peers per live endpoint that reported any (inbound addrs = ephemeral ports)
fn outbound(endpoints: &imbl::Vector<ValidatorMetadata>) -> Vec<HashSet<&str>> {
    endpoints
        .iter()
        .filter(|meta| meta.state == EndpointState::Live)
        .map(|meta| {
            meta.peers.iter().filter(|peer| !peer.inbound).map(|peer| peer.addr.as_str()).collect()
        })
        .filter(|peers: &HashSet<&str>| !peers.is_empty())
        .collect()
}

/// Least pairwise overlap; `None` below two endpoints with outbound peers
fn shared_outbound_min(outbound: &[HashSet<&str>]) -> Option<usize> {
    let pairs = outbound.iter().enumerate().flat_map(|(at, left)| {
        outbound[at + 1..].iter().map(move |right| left.intersection(right).count())
    });
    pairs.min()
}

pub(crate) fn alarms(endpoints: &imbl::Vector<ValidatorMetadata>, finality_paused: bool) -> Alarms {
    let stale = endpoints
        .iter()
        .enumerate()
        .filter(|(_, meta)| meta.state == EndpointState::Live)
        .filter(|(_, meta)| meta.stale_blocks().is_some_and(|behind| behind >= STALE_TIP_BLOCKS))
        .filter_map(|(index, _)| EndpointIndex::new(index))
        .collect();
    let ending = endpoints
        .iter()
        .enumerate()
        .filter(|(_, meta)| {
            meta.blocks_to_end_of_service().is_some_and(|left| left <= END_OF_SERVICE_WARN_BLOCKS)
        })
        .filter_map(|(index, _)| EndpointIndex::new(index))
        .collect();
    let outbound = outbound(endpoints);
    let union: HashSet<&str> = outbound.iter().flatten().copied().collect();
    Alarms {
        stale,
        ending,
        partitioned: shared_outbound_min(&outbound) == Some(0),
        eclipsed: (1..=ECLIPSE_OUTBOUND_MAX).contains(&union.len()),
        finality_paused,
    }
}

const STATES: [EndpointState; 5] = [
    EndpointState::Pending,
    EndpointState::Live,
    EndpointState::Degraded,
    EndpointState::Down,
    EndpointState::CatchingUp,
];

const AGREEMENTS: [Agreement; 5] = [
    Agreement::Unknown,
    Agreement::Agreed,
    Agreement::Ahead,
    Agreement::Behind,
    Agreement::Diverged,
];

fn origin(sighting: &Sighting) -> &'static str {
    match sighting.ours() {
        true => "ours",
        false => "network",
    }
}

/// First trusted listing just landed
pub(crate) fn first_trusted(sighting: &Sighting) {
    let timeline = sighting.timeline();
    if let Some(at) = timeline.first_trusted {
        let took = at.duration_since(timeline.first_seen).as_secs_f64();
        metrics::histogram!(names::FIRST_TRUSTED, "origin" => origin(sighting)).record(took);
    }
}

/// Every mempool reader lists it, for the first time
pub(crate) fn all_trusted(sighting: &Sighting) {
    let timeline = sighting.timeline();
    if let (Some(first), Some(all)) = (timeline.first_trusted, timeline.all_trusted) {
        metrics::histogram!(names::ALL_TRUSTED).record(all.duration_since(first).as_secs_f64());
    }
}

/// Left the view; `at_block` = dropped as the tip moved (mined, most likely)
pub(crate) fn left(sighting: &Sighting, at_block: bool) {
    let stayed = sighting.timeline().first_seen.elapsed().as_secs_f64();
    let how = if at_block { "block" } else { "unlisted" };
    metrics::histogram!(names::RESIDENCE, "left" => how, "origin" => origin(sighting))
        .record(stayed);
}

/// One submission ended: its outcome and how many entries it took
pub(crate) fn submitted(ended: &Ended, attempts: usize) {
    let outcome = match ended {
        Ended::Spread => "spread",
        Ended::Unconfirmed => "unconfirmed",
        Ended::Rejected(_) => "rejected",
        Ended::Unreachable(_) => "unreachable",
    };
    metrics::counter!(names::SUBMISSIONS, "outcome" => outcome).increment(1);
    metrics::histogram!(names::SUBMISSION_ATTEMPTS, "outcome" => outcome).record(attempts as f64);
}

/// Every gauge from `snapshot`, then a log line per alarm that rose or cleared since `previous`
pub(crate) fn emit(snapshot: &ChainViewSnapshot, previous: Alarms) {
    let verified = snapshot.mempool.values().filter(|s| !s.trusted().is_empty()).count();
    let held = snapshot.mempool.len();
    metrics::gauge!(names::MEMPOOL_TRANSACTIONS, "state" => "verified").set(verified as f64);
    metrics::gauge!(names::MEMPOOL_TRANSACTIONS, "state" => "ours_unverified")
        .set((held - verified) as f64);

    for meta in snapshot.endpoints.iter() {
        let endpoint = meta.address.clone();
        for state in STATES {
            let labels = [("endpoint", endpoint.clone()), ("state", state.label().to_owned())];
            metrics::gauge!(names::ENDPOINT_STATE, &labels).set(f64::from(meta.state == state));
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
            if let EndOfService::At { height, .. } = release.end_of_service {
                metrics::gauge!(names::END_OF_SERVICE_HEIGHT, "endpoint" => endpoint.clone())
                    .set(f64::from(u32::from(height)));
            }
        }
    }
    let holders = snapshot.tip.map_or(0, |tip| tip.held_by.count());
    metrics::gauge!(names::TIP_HOLDERS).set(holders as f64);
    if let Some(best) = snapshot.best() {
        metrics::gauge!(names::BEST_HEIGHT).set(f64::from(u32::from(best.height)));
    }
    if let Some(shared) = shared_outbound_min(&outbound(&snapshot.endpoints)) {
        metrics::gauge!(names::SHARED_OUTBOUND_MIN).set(shared as f64);
    }
    let now = snapshot.alarms;
    metrics::gauge!(names::FINALITY_PAUSED).set(f64::from(now.finality_paused));

    for index in (0..snapshot.endpoints.len()).filter_map(EndpointIndex::new) {
        let Some(meta) = snapshot.endpoints.get(index.get()) else { continue };
        let tip = meta.tip().map(|tip| u32::from(tip.height));
        let estimated = meta.info.as_ref().map(|info| u32::from(info.estimated_height));
        match (previous.stale.contains(index), now.stale.contains(index)) {
            (false, true) => warn!(
                endpoint = %meta.address, ?tip, ?estimated,
                "Validator tip stale against its own clock (stalled or eclipsed)"
            ),
            (true, false) => info!(endpoint = %meta.address, ?tip, "Validator tip fresh again"),
            _ => {}
        }
        let left = meta.blocks_to_end_of_service();
        let build = meta.release.as_ref().map(|release| release.build.as_str());
        match (previous.ending.contains(index), now.ending.contains(index)) {
            (false, true) => warn!(
                endpoint = %meta.address, ?build, ?left,
                "Validator release reaches end of service soon (it halts there): upgrade it"
            ),
            (true, false) => info!(endpoint = %meta.address, ?build, "Validator release upgraded"),
            _ => {}
        }
    }
    match (previous.partitioned, now.partitioned) {
        (false, true) => warn!("Two live validators share no outbound peer (possible partition)"),
        (true, false) => info!("Live validators share outbound peers again"),
        _ => {}
    }
    match (previous.eclipsed, now.eclipsed) {
        (false, true) => warn!(
            max = ECLIPSE_OUTBOUND_MAX,
            "Live validators reach few distinct outbound peers (possible eclipse)"
        ),
        (true, false) => info!("Live validators reach enough distinct outbound peers again"),
        _ => {}
    }
    let best = snapshot.best().map(|best| u32::from(best.height));
    match (previous.finality_paused, now.finality_paused) {
        (false, true) => warn!(?best, "Finality paused: no trusted validator holds the boundary"),
        (true, false) => info!(?best, "Finality resumed"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::{
        BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, Height, PeerInfo,
    };

    use super::*;

    /// - Isolated node (regtest) + inbound-only overlap → nothing raised
    #[test]
    fn alarms_rise_on_a_stale_tip_a_partition_and_a_thin_outbound_set() {
        type Endpoint = (EndpointState, u32, u32, &'static [&'static str], &'static [&'static str]);
        type Raised = (&'static [usize], bool, bool);
        use EndpointState::{CatchingUp, Live};
        #[rustfmt::skip]
        let cases: [(&str, &[Endpoint], Raised); 8] = [
            ("isolated regtest node",     &[(Live, 10, 10, &[], &[])],                                   (&[], false, false)),
            ("one block short of stale",  &[(Live, 10, 33, &["p1", "p2", "p3"], &[])],                   (&[], false, false)),
            ("stale live tip",            &[(Live, 10, 34, &["p1", "p2", "p3"], &[])],                   (&[0], false, false)),
            ("catching up: not an alarm", &[(CatchingUp, 10, 500, &["p1", "p2", "p3"], &[])],            (&[], false, false)),
            ("no shared outbound peer",   &[(Live, 10, 10, &["p1", "p2", "p3"], &[]),
                                            (Live, 10, 10, &["p4", "p5", "p6"], &[])],                   (&[], true, false)),
            ("one shared outbound peer",  &[(Live, 10, 10, &["p1", "p2", "p3"], &[]),
                                            (Live, 10, 10, &["p3", "p4", "p5"], &[])],                   (&[], false, false)),
            ("inbound overlap ignored",   &[(Live, 10, 10, &["p1", "p2", "p3"], &["x"]),
                                            (Live, 10, 10, &["p4", "p5", "p6"], &["x"])],                (&[], true, false)),
            ("one outbound peer for all", &[(Live, 10, 10, &["p1"], &[]), (Live, 10, 10, &["p1"], &[])], (&[], false, true)),
        ];
        for (case, endpoints, (stale, partitioned, eclipsed)) in cases {
            let endpoints: imbl::Vector<ValidatorMetadata> = endpoints
                .iter()
                .enumerate()
                .map(|(index, (state, tip, estimated, outbound, inbound))| {
                    let height = |h: u32| Height::try_from(h).expect("h");
                    let peer = |inbound: bool| {
                        move |addr: &&str| PeerInfo { addr: (*addr).to_owned(), inbound }
                    };
                    let mut meta = ValidatorMetadata::new(format!("v{index}:8232"));
                    meta.state = *state;
                    let branch = ConsensusBranchId::new(0);
                    meta.info = Some(BlockchainInfo {
                        blocks: height(*tip),
                        estimated_height: height(*estimated),
                        best_block_hash: BlockHash::ZERO,
                        sapling_activation: Height::GENESIS,
                        upgrades: Vec::new(),
                        consensus: ConsensusBranchIds { chain_tip: branch, next_block: branch },
                    });
                    meta.peers = outbound
                        .iter()
                        .map(peer(false))
                        .chain(inbound.iter().map(peer(true)))
                        .collect();
                    meta
                })
                .collect();
            let stale = EndpointSet::at(stale.iter().copied());
            let ending = EndpointSet::default();
            let expected = Alarms { stale, ending, partitioned, eclipsed, finality_paused: false };
            assert_eq!(alarms(&endpoints, false), expected, "{case}");
        }
    }
}
