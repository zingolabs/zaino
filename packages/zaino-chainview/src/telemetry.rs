//! Endpoint telemetry: alarm conditions per fold, event histograms + counters
//!
//! - Observation only: never moves the tip, never gates serving or sync (false alarm = a log line)
//! - State gauges + alarm edge logs = `zaino-snapshot`'s (at scrape, per publish)

use std::collections::HashSet;

use crate::config::{ECLIPSE_OUTBOUND_MAX, END_OF_SERVICE_WARN_BLOCKS, STALE_TIP_BLOCKS};
use zaino_traffic::{Health, ValidatorId};

use crate::endpoints::{EndpointSet, ValidatorMetadata};
use crate::snapshot::Sighting;
use crate::submit::Ended;

mod names {
    pub(super) const FIRST_TRUSTED: &str = "zaino.chainview.first_trusted_seconds";
    pub(super) const ALL_TRUSTED: &str = "zaino.chainview.all_trusted_seconds";
    pub(super) const RESIDENCE: &str = "zaino.chainview.residence_seconds";
    pub(super) const SUBMISSIONS: &str = "zaino.chainview.submissions_total";
    pub(super) const SUBMISSION_ATTEMPTS: &str = "zaino.chainview.submission_attempts";
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
    use metrics::{describe_counter, describe_histogram};

    describe_counter!(
        names::SUBMISSIONS,
        "Submissions ended, by outcome (spread = seen past its entries; unconfirmed = accepted, \
         spread unobservable; rejected; unreachable)"
    );
    describe_histogram!(
        names::SUBMISSION_ATTEMPTS,
        "Entries one submission pushed to before it ended, by outcome"
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
}

/// Raised conditions as of one fold (`zaino-snapshot` logs each edge)
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

    /// Finality stalled: final tip unmoved ≥ 60 s, a block `depth` deep no trusted validator
    /// vouched for (H6)
    pub fn finality_paused(&self) -> bool {
        self.finality_paused
    }
}

/// Outbound peers per live endpoint that reported any (inbound addrs = ephemeral ports)
pub(crate) fn outbound(endpoints: &imbl::Vector<ValidatorMetadata>) -> Vec<HashSet<&str>> {
    endpoints
        .iter()
        .filter(|meta| meta.health == Health::Live)
        .map(|meta| {
            meta.peers.iter().filter(|peer| !peer.inbound).map(|peer| peer.addr.as_str()).collect()
        })
        .filter(|peers: &HashSet<&str>| !peers.is_empty())
        .collect()
}

/// Least pairwise overlap; `None` below two endpoints with outbound peers
pub(crate) fn shared_outbound_min(outbound: &[HashSet<&str>]) -> Option<usize> {
    let pairs = outbound.iter().enumerate().flat_map(|(at, left)| {
        outbound[at + 1..].iter().map(move |right| left.intersection(right).count())
    });
    pairs.min()
}

pub(crate) fn alarms(endpoints: &imbl::Vector<ValidatorMetadata>, finality_paused: bool) -> Alarms {
    let stale = endpoints
        .iter()
        .enumerate()
        .filter(|(_, meta)| meta.health == Health::Live)
        .filter(|(_, meta)| meta.stale_blocks().is_some_and(|behind| behind >= STALE_TIP_BLOCKS))
        .filter_map(|(index, _)| ValidatorId::new(index))
        .collect();
    let ending = endpoints
        .iter()
        .enumerate()
        .filter(|(_, meta)| {
            meta.blocks_to_end_of_service().is_some_and(|left| left <= END_OF_SERVICE_WARN_BLOCKS)
        })
        .filter_map(|(index, _)| ValidatorId::new(index))
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

#[cfg(test)]
mod tests {
    use zaino_primitives::types::{
        BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, Height, PeerInfo,
    };

    use super::*;

    /// - Isolated node (regtest) + inbound-only overlap → nothing raised
    #[test]
    fn alarms_rise_on_a_stale_tip_a_partition_and_a_thin_outbound_set() {
        type Endpoint = (Health, u32, u32, &'static [&'static str], &'static [&'static str]);
        type Raised = (&'static [usize], bool, bool);
        use Health::{CatchingUp, Live};
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
                    meta.health = *state;
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
