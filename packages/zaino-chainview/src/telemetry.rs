//! Trusted validator telemetry: alarm conditions per merge, event histograms + counters
//!
//! - Observation only: never moves the tip, never gates serving or sync (false alarm = a log line)
//! - State gauges + alarm edge logs = `zaino-grpc-snapshot`'s (at scrape, per publish)

use std::collections::{BTreeSet, HashSet};

use strum::VariantArray as _;

use metrics::Unit;
use zaino_primitives::metric::Metric;

use crate::alarm_thresholds::{ECLIPSE_OUTBOUND_MAX, END_OF_SERVICE_WARN_BLOCKS, STALE_TIP_BLOCKS};
use zaino_traffic::{Health, TrustedValidatorId};

use crate::chain_view_snapshot::Sighting;
use crate::submit::Ended;
use crate::trusted_validators::{TrustedValidatorMeta, TrustedValidatorSet};

mod names {
    pub(super) const FIRST_TRUSTED: &str = "zaino.chainview.first_trusted_seconds";
    pub(super) const ALL_TRUSTED: &str = "zaino.chainview.all_trusted_seconds";
    pub(super) const RESIDENCE: &str = "zaino.chainview.residence_seconds";
    pub(super) const SUBMISSIONS: &str = "zaino.chainview.submissions_total";
    pub(super) const SUBMISSION_ATTEMPTS: &str = "zaino.chainview.submission_attempts";
}

/// Every metric this crate emits
pub const METRICS: &[Metric] = &[
    Metric::counter(
        names::SUBMISSIONS,
        "Submissions ended, by outcome (spread = seen past its entries; unconfirmed = accepted, \
         spread unobservable; rejected; unreachable)",
    ),
    Metric::histogram(
        names::SUBMISSION_ATTEMPTS,
        "Entries one submission pushed to before it ended, by outcome",
    ),
    Metric::histogram(
        names::FIRST_TRUSTED,
        "First sighting to first trusted listing, by origin (ours = our relay)",
    )
    .unit(Unit::Seconds),
    Metric::histogram(
        names::ALL_TRUSTED,
        "First trusted listing to every mempool-reading trusted validator listing it",
    )
    .unit(Unit::Seconds),
    Metric::histogram(
        names::RESIDENCE,
        "First sighting to leaving the view, by how (block = at a tip move; unlisted = evicted)",
    )
    .unit(Unit::Seconds),
];

/// One alarm condition: raised in `telemetry::alarms`, noted + logged by `zaino-grpc-snapshot`'s report
///
/// - new alarm = a variant, then the compiler names its arms (`alarms`, `note`, `edge`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::VariantArray)]
pub enum Alarm {
    Partitioned,
    Eclipsed,
    FinalityPaused,
    Stale,
    Ending,
}

/// Raised alarms as of one merge: `(alarm, by)`, `by` = the trusted validator raising it
/// (`None` = view-wide)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Alarms(BTreeSet<(Alarm, Option<TrustedValidatorId>)>);

impl Alarms {
    pub fn raised(&self, alarm: Alarm) -> bool {
        self.0.iter().any(|(raised, _)| *raised == alarm)
    }

    /// Trusted validators raising `alarm` (empty for a view-wide one)
    pub fn by(&self, alarm: Alarm) -> TrustedValidatorSet {
        self.0.iter().filter(|(raised, _)| *raised == alarm).filter_map(|(_, by)| *by).collect()
    }

    /// Raised here, not in `earlier` (`earlier.since(self)` = cleared)
    pub fn since<'a>(
        &'a self,
        earlier: &'a Alarms,
    ) -> impl Iterator<Item = (Alarm, Option<TrustedValidatorId>)> + 'a {
        self.0.difference(&earlier.0).copied()
    }

    /// Raised instances per alarm, `Alarm` order (0 = clear)
    pub fn counts(&self) -> impl Iterator<Item = (Alarm, usize)> + '_ {
        let count = |alarm: Alarm| self.0.iter().filter(|(raised, _)| *raised == alarm).count();
        Alarm::VARIANTS.iter().map(move |alarm| (*alarm, count(*alarm)))
    }
}

/// Outbound peers per live trusted validator that reported any (inbound addrs = ephemeral ports)
pub(crate) fn outbound(
    trusted_validators: &imbl::Vector<TrustedValidatorMeta>,
) -> Vec<HashSet<&str>> {
    trusted_validators
        .iter()
        .filter(|meta| meta.health == Health::Live)
        .map(|meta| {
            let peers = meta.telemetry.peers.iter().flatten();
            peers.filter(|peer| !peer.inbound).map(|peer| peer.addr.as_str()).collect()
        })
        .filter(|peers: &HashSet<&str>| !peers.is_empty())
        .collect()
}

/// Least pairwise overlap (`None` below two trusted validators with outbound peers)
pub(crate) fn shared_outbound_min(outbound: &[HashSet<&str>]) -> Option<usize> {
    let pairs = outbound.iter().enumerate().flat_map(|(at, left)| {
        outbound[at + 1..].iter().map(move |right| left.intersection(right).count())
    });
    pairs.min()
}

/// `finality_paused` = finalized tip unmoved ≥ 60 s, a block `depth` deep no trusted validator
/// vouched for (H6)
pub(crate) fn alarms(
    trusted_validators: &imbl::Vector<TrustedValidatorMeta>,
    finality_paused: bool,
) -> Alarms {
    let outbound = outbound(trusted_validators);
    let union: HashSet<&str> = outbound.iter().flatten().copied().collect();
    let view_wide = |raised: bool| if raised { vec![None] } else { Vec::new() };
    let raising = |raises: &dyn Fn(&TrustedValidatorMeta) -> bool| {
        let validators = trusted_validators.iter().enumerate().filter(|(_, meta)| raises(meta));
        validators.filter_map(|(index, _)| TrustedValidatorId::new(index)).map(Some).collect()
    };
    let raised_by = |alarm: Alarm| -> Vec<Option<TrustedValidatorId>> {
        match alarm {
            Alarm::Partitioned => view_wide(shared_outbound_min(&outbound) == Some(0)),
            Alarm::Eclipsed => view_wide((1..=ECLIPSE_OUTBOUND_MAX).contains(&union.len())),
            Alarm::FinalityPaused => view_wide(finality_paused),
            Alarm::Stale => raising(&|meta| {
                let stale = meta.stale_blocks().is_some_and(|behind| behind >= STALE_TIP_BLOCKS);
                meta.health == Health::Live && stale
            }),
            Alarm::Ending => raising(&|meta| {
                let left = meta.blocks_to_end_of_service();
                left.is_some_and(|left| left <= END_OF_SERVICE_WARN_BLOCKS)
            }),
        }
    };
    let raised = Alarm::VARIANTS
        .iter()
        .flat_map(|alarm| raised_by(*alarm).into_iter().map(move |by| (*alarm, by)));
    Alarms(raised.collect())
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

/// Left the view (`at_block` = dropped as the tip moved: mined, most likely)
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
    use zaino_primitives::types::BlockRef;
    use zaino_source::{BlockchainInfo, PeerInfo};
    use zcash_protocol::BlockHash;

    use zaino_source::testing::h;

    use super::*;

    #[test]
    fn alarms_rise_on_a_stale_tip_a_partition_and_a_thin_outbound_set() {
        type Validator = (Health, u32, u32, &'static [&'static str], &'static [&'static str]);
        type Raised = (&'static [usize], bool, bool);
        use Health::{CatchingUp, Live};
        #[rustfmt::skip]
        let cases: [(&str, &[Validator], Raised); 8] = [
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
        for (case, trusted_validators, (stale, partitioned, eclipsed)) in cases {
            let trusted_validators: imbl::Vector<TrustedValidatorMeta> = trusted_validators
                .iter()
                .enumerate()
                .map(|(index, (state, tip, estimated, outbound, inbound))| {
                    let peer = |inbound: bool| {
                        move |addr: &&str| PeerInfo { addr: (*addr).to_owned(), inbound }
                    };
                    let mut meta = TrustedValidatorMeta::new(format!("v{index}:8232"));
                    meta.health = *state;
                    meta.info = Some(BlockchainInfo {
                        tip: BlockRef { hash: BlockHash([0; 32]), height: h(*tip) },
                        estimated_height: h(*estimated),
                        activations: Vec::new(),
                    });
                    let peers =
                        outbound.iter().map(peer(false)).chain(inbound.iter().map(peer(true)));
                    meta.telemetry.peers = Some(peers.collect());
                    meta
                })
                .collect();
            let raised = alarms(&trusted_validators, false);
            let expected = (TrustedValidatorSet::at(stale.iter().copied()), partitioned, eclipsed);
            let answered = (
                raised.by(Alarm::Stale),
                raised.raised(Alarm::Partitioned),
                raised.raised(Alarm::Eclipsed),
            );
            assert_eq!(answered, expected, "{case}");
            assert!(
                !raised.raised(Alarm::Ending) && !raised.raised(Alarm::FinalityPaused),
                "{case}"
            );
        }
    }
}
