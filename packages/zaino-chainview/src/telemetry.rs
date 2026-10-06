//! Endpoint telemetry: gauges each fold, a log line per alarm edge
//!
//! - Observation only: never a vote, never gates serving or sync (a false alarm costs a log line)

use std::collections::HashSet;

use tracing::{info, warn};

use crate::config::{ECLIPSE_OUTBOUND_MAX, STALE_TIP_BLOCKS};
use crate::endpoints::{Agreement, EndpointIndex, EndpointSet, EndpointState, ValidatorMetadata};
use crate::snapshot::ChainViewSnapshot;

mod names {
    pub(super) const ENDPOINT_STATE: &str = "zaino.chainview.endpoint_state";
    pub(super) const AGREEMENT: &str = "zaino.chainview.agreement";
    pub(super) const TIP_HEIGHT: &str = "zaino.chainview.tip_height";
    pub(super) const STALE_BLOCKS: &str = "zaino.chainview.stale_blocks";
    pub(super) const PEERS: &str = "zaino.chainview.peers";
    pub(super) const AGREEING: &str = "zaino.chainview.agreeing";
    pub(super) const SHARED_OUTBOUND_MIN: &str = "zaino.chainview.shared_outbound_min";
}

/// `# HELP` registrations for every metric this crate emits
pub fn describe_metrics() {
    use metrics::describe_gauge;

    describe_gauge!(names::ENDPOINT_STATE, "1 on the endpoint's current poller state, by endpoint");
    describe_gauge!(
        names::AGREEMENT,
        "1 on where the endpoint's chain stands against the quorum tip, by endpoint"
    );
    describe_gauge!(names::TIP_HEIGHT, "Endpoint's own best-chain tip height, by endpoint");
    describe_gauge!(
        names::STALE_BLOCKS,
        "Blocks the endpoint's tip trails its own clock-based network estimate, by endpoint"
    );
    describe_gauge!(names::PEERS, "Endpoint's getpeerinfo connections, by endpoint and direction");
    describe_gauge!(
        names::AGREEING,
        "Largest group of voting endpoints holding one common block (quorum tip's agreers)"
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
    partitioned: bool,
    eclipsed: bool,
}

impl Alarms {
    /// Live endpoints whose tip trails their own clock estimate by >= `STALE_TIP_BLOCKS`
    pub fn stale(&self) -> EndpointSet {
        self.stale
    }

    /// Two live endpoints share no outbound peer
    pub fn partitioned(&self) -> bool {
        self.partitioned
    }

    /// Live endpoints together reach at most `ECLIPSE_OUTBOUND_MAX` outbound peers
    pub fn eclipsed(&self) -> bool {
        self.eclipsed
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

pub(crate) fn alarms(endpoints: &imbl::Vector<ValidatorMetadata>) -> Alarms {
    let stale = endpoints
        .iter()
        .enumerate()
        .filter(|(_, meta)| meta.state == EndpointState::Live)
        .filter(|(_, meta)| meta.stale_blocks().is_some_and(|behind| behind >= STALE_TIP_BLOCKS))
        .filter_map(|(index, _)| EndpointIndex::new(index))
        .collect();
    let outbound = outbound(endpoints);
    let union: HashSet<&str> = outbound.iter().flatten().copied().collect();
    Alarms {
        stale,
        partitioned: shared_outbound_min(&outbound) == Some(0),
        eclipsed: (1..=ECLIPSE_OUTBOUND_MAX).contains(&union.len()),
    }
}

const STATES: [EndpointState; 6] = [
    EndpointState::Pending,
    EndpointState::Live,
    EndpointState::Degraded,
    EndpointState::Down,
    EndpointState::Syncing,
    EndpointState::CatchingUp,
];

const AGREEMENTS: [Agreement; 5] = [
    Agreement::Unknown,
    Agreement::Agreed,
    Agreement::Ahead,
    Agreement::Behind,
    Agreement::Diverged,
];

/// Every gauge from `snapshot`, then a log line per alarm that rose or cleared since `previous`
pub(crate) fn emit(snapshot: &ChainViewSnapshot, previous: Alarms) {
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
    }
    metrics::gauge!(names::AGREEING).set(snapshot.agreeing.count() as f64);
    if let Some(shared) = shared_outbound_min(&outbound(&snapshot.endpoints)) {
        metrics::gauge!(names::SHARED_OUTBOUND_MIN).set(shared as f64);
    }

    let now = snapshot.alarms;
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
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::{
        BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, Height, PeerInfo,
    };

    use super::*;
    use crate::chain::EndpointChain;

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
                    meta.chain = Some(EndpointChain::of(height(*tip), [BlockHash::ZERO]));
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
            let expected =
                Alarms { stale: EndpointSet::at(stale.iter().copied()), partitioned, eclipsed };
            assert_eq!(alarms(&endpoints), expected, "{case}");
        }
    }
}
