//! Trusted validator telemetry: alarm conditions per merge, submission counters
//!
//! - Observation only: never moves the tip, never gates serving or sync (false alarm = a log line)
//! - State gauges + alarm edge logs = `zaino-grpc-snapshot`'s (at scrape, per publish)

use std::collections::BTreeSet;

use strum::VariantArray as _;

use zaino_primitives::metric::Metric;

use crate::alarm_thresholds::{END_OF_SERVICE_WARN_BLOCKS, STALE_TIP_BLOCKS};
use zaino_traffic::{Health, TrustedValidatorId};

use crate::submit::Ended;
use crate::trusted_validators::TrustedValidatorMeta;

mod names {
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
];

/// One alarm condition: raised in `telemetry::alarms`, noted + logged by `zaino-grpc-snapshot`'s report
///
/// - new alarm = a variant, then the compiler names its arms (`alarms`, `note`, `edge`)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::VariantArray)]
pub enum Alarm {
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
    #[cfg(test)]
    pub(crate) fn by(&self, alarm: Alarm) -> crate::TrustedValidatorSet {
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

/// `finality_paused` = finalized tip unmoved ≥ 60 s, a block `depth` deep no trusted validator
/// vouched for (H6)
pub(crate) fn alarms(
    trusted_validators: &imbl::Vector<TrustedValidatorMeta>,
    finality_paused: bool,
) -> Alarms {
    let raising = |raises: &dyn Fn(&TrustedValidatorMeta) -> bool| {
        let validators = trusted_validators.iter().enumerate().filter(|(_, meta)| raises(meta));
        validators.filter_map(|(index, _)| TrustedValidatorId::new(index)).map(Some).collect()
    };
    let raised_by = |alarm: Alarm| -> Vec<Option<TrustedValidatorId>> {
        match alarm {
            Alarm::FinalityPaused => {
                if finality_paused {
                    vec![None]
                } else {
                    Vec::new()
                }
            }
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
    use zaino_source::{BlockchainInfo, EndOfService, NodeRelease};
    use zcash_protocol::BlockHash;

    use zaino_source::testing::h;

    use super::*;
    use crate::TrustedValidatorSet;

    #[test]
    fn stale_rises_only_on_a_live_tip_24_blocks_behind_its_own_clock() {
        use Health::{CatchingUp, Live};
        const TIP: u32 = 10;
        #[rustfmt::skip]
        let cases: [(&str, Health, u32, &[usize]); 4] = [
            // case,                      health,     clock estimate,                stale
            ("isolated regtest node",     Live,       TIP,                           &[]),
            ("one block short of stale",  Live,       TIP + STALE_TIP_BLOCKS - 1,    &[]),
            ("stale live tip",            Live,       TIP + STALE_TIP_BLOCKS,        &[0]),
            ("catching up: not an alarm", CatchingUp, TIP + 20 * STALE_TIP_BLOCKS,   &[]),
        ];
        for (case, health, estimated, stale) in cases {
            let mut meta = TrustedValidatorMeta::new("v0:8232".to_owned());
            meta.health = health;
            meta.info = Some(BlockchainInfo {
                tip: BlockRef { hash: BlockHash([0; 32]), height: h(TIP) },
                estimated_height: h(estimated),
                activations: Vec::new(),
            });
            let raised = alarms(&imbl::vector![meta], false);
            let expected = TrustedValidatorSet::at(stale.iter().copied());
            assert_eq!(raised.by(Alarm::Stale), expected, "{case}: stale validators");
            let others = raised.raised(Alarm::Ending) || raised.raised(Alarm::FinalityPaused);
            assert!(!others, "{case}: only Stale may rise: {raised:?}");
        }
    }

    #[test]
    fn ending_rises_on_a_release_halting_within_a_week_of_its_tip() {
        const TIP: u32 = 10;
        let halts = |height: u32| EndOfService::At { height: h(height) };
        #[rustfmt::skip]
        let cases: [(&str, EndOfService, &[usize]); 4] = [
            // case,                      release halts,                                     ending
            ("halts a week out",          halts(TIP + END_OF_SERVICE_WARN_BLOCKS),           &[0]),
            ("halts a block past a week", halts(TIP + END_OF_SERVICE_WARN_BLOCKS + 1),       &[]),
            ("no halt enforced",          EndOfService::NotEnforced,                         &[]),
            ("halt unknown",              EndOfService::Unknown,                             &[]),
        ];
        for (case, end_of_service, ending) in cases {
            let mut meta = TrustedValidatorMeta::new("v0:8232".to_owned());
            meta.health = Health::Live;
            meta.info = Some(BlockchainInfo {
                tip: BlockRef { hash: BlockHash([0; 32]), height: h(TIP) },
                estimated_height: h(TIP),
                activations: Vec::new(),
            });
            let (build, user_agent) = ("v6.4.2".to_owned(), "/Zebra:6.4.2/".to_owned());
            meta.telemetry.release = Some(NodeRelease { build, user_agent, end_of_service });
            let raised = alarms(&imbl::vector![meta], false);
            let expected = TrustedValidatorSet::at(ending.iter().copied());
            assert_eq!(raised.by(Alarm::Ending), expected, "{case}: ending validators");
        }
    }
}
