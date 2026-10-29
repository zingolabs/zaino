//! Request classes, the lane each rides, per-member permits (`usage.md`)

use std::time::Duration;

/// Ask kind: hedge floor, member kinds, round retry, synced (lane = permits + priority only)
///
/// - no poll: the core's own, on its own permit outside every lane
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Class {
    Submit,
    Headers,
    TipBlock,
    Lookup,
    Bytes,
    BulkBlock,
}

/// Declaration order = dispatch priority (a freed permit goes to the first waiting lane)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Lane {
    Control,
    Interactive,
    Bulk,
}

pub(crate) const LANES: usize = 3;
/// Permits reserved across every lane (`as`: const items have no `From`)
pub(crate) const LANE_RESERVES: u32 = LANES as u32 * RESERVE;

/// Per member, one counter per lane
pub(crate) type PerLane<T> = [T; LANES];

/// The poll's + one reserved per lane + one shared
pub(crate) const MIN_CONNECTIONS: u32 = 1 + LANE_RESERVES + 1;

impl Class {
    pub(crate) fn lane(self) -> Lane {
        match self {
            Self::Submit | Self::Headers => Lane::Control,
            Self::TipBlock | Self::Lookup | Self::Bytes => Lane::Interactive,
            Self::BulkBlock => Lane::Bulk,
        }
    }

    /// Second member once the latest send is this old (`None` = never hedged)
    pub(crate) fn hedge_floor(self) -> Option<Duration> {
        match self {
            Self::TipBlock => Some(Duration::from_secs(2)),
            Self::Lookup => Some(Duration::from_secs(1)),
            Self::BulkBlock => Some(Duration::from_secs(15)),
            Self::Submit | Self::Headers | Self::Bytes => None,
        }
    }

    /// - `true` = headers + block bodies (checkable)
    /// - trusted-only = the "only source" rows (`docs/architecture.md` §3) + mempool bytes (peers
    ///   serve by wtxid, never a listing's txid)
    pub(crate) fn peers(self) -> bool {
        matches!(self, Self::Headers | Self::TipBlock | Self::BulkBlock)
    }

    /// Every member tried → next round after 1 s (else unanswered)
    pub(crate) fn retries_rounds(self) -> bool {
        matches!(self, Self::TipBlock | Self::BulkBlock)
    }

    /// `CatchingUp` excluded (no mempool, lagging chain)
    pub(crate) fn needs_synced(self) -> bool {
        matches!(self, Self::Lookup | Self::Bytes)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Submit => "submit",
            Self::Headers => "headers",
            Self::TipBlock => "tip_block",
            Self::Lookup => "lookup",
            Self::Bytes => "bytes",
            Self::BulkBlock => "bulk_block",
        }
    }
}

impl Lane {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Control => 0,
            Self::Interactive => 1,
            Self::Bulk => 2,
        }
    }
}

/// Permits per lane no other lane takes
const RESERVE: u32 = 1;

/// One member's lane connections: `RESERVE` per lane, the rest shared
#[derive(Debug, Clone)]
pub(crate) struct Permits {
    max: u32,
}

impl Permits {
    /// `max_connections` − the poll's own (`>= MIN_CONNECTIONS`: `Limits::new`)
    pub(crate) fn trusted(max_connections: u32) -> Self {
        Self { max: max_connections - 1 }
    }

    /// The peer set's requests at once: one reserved per lane, the rest shared (no poll)
    pub(crate) fn peers(in_flight: u32) -> Self {
        assert!(in_flight >= LANE_RESERVES, "the peer set's in flight covers every lane reserve");
        Self { max: in_flight }
    }

    pub(crate) fn admits(&self, in_flight: &PerLane<u32>, lane: Lane) -> bool {
        in_flight[lane.index()] < RESERVE || self.claimed(in_flight) < self.max
    }

    /// T1: Σ in flight ≤ max, reserves never borrowed
    #[cfg(test)]
    pub(crate) fn holds(&self, in_flight: &PerLane<u32>) -> bool {
        self.claimed(in_flight) <= self.max
    }

    /// Each lane's in flight, or its reserve if larger
    fn claimed(&self, in_flight: &PerLane<u32>) -> u32 {
        in_flight.iter().map(|n| (*n).max(RESERVE)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - of 8, the poll keeps 1: 7 lane permits = 3 reserves + 4 shared
    /// - each lane still admits its reserved one with every shared permit taken
    #[test]
    fn a_trusted_members_lanes_share_idle_permits_but_never_a_reserve() {
        let permits = Permits::trusted(8);
        let mut in_flight = [0; LANES];

        while permits.admits(&in_flight, Lane::Bulk) {
            in_flight[Lane::Bulk.index()] += 1;
        }

        assert_eq!(in_flight, [0, 0, 5], "bulk fills its reserve + the 4 shared, then stops");
        for lane in [Lane::Control, Lane::Interactive] {
            assert!(permits.admits(&in_flight, lane), "{lane:?} reserve free beside {in_flight:?}");
            in_flight[lane.index()] += 1;
            let full = !permits.admits(&in_flight, lane);
            assert!(full, "{lane:?}: its reserve taken, no shared left: {in_flight:?}");
        }
        assert!(permits.holds(&in_flight), "all 7 lane permits held: within T1: {in_flight:?}");
        let borrowed = [6, 1, 0];
        let broken = !permits.holds(&borrowed);
        assert!(broken, "control on the idle bulk reserve breaks T1: {borrowed:?}");
    }

    /// 4 in flight = 3 reserves + 1 shared, no poll's permit
    #[test]
    fn the_peer_sets_lanes_keep_a_reserve_each_and_share_the_rest() {
        let peers = Permits::peers(4);

        #[rustfmt::skip]
        let cases = [
            // case,                                         in flight, lane,              admits
            ("control's reserve beside bulk on the rest",    [0, 0, 3], Lane::Control,     true),
            ("bulk past its reserve: the shared one",        [0, 0, 1], Lane::Bulk,        true),
            ("bulk stops at the other two reserves",         [0, 0, 2], Lane::Bulk,        false),
            ("all 4 in flight",                              [1, 1, 2], Lane::Interactive, false),
        ];
        for (case, in_flight, lane, admits) in cases {
            assert_eq!(
                peers.admits(&in_flight, lane),
                admits,
                "{case}: {lane:?} beside {in_flight:?}"
            );
        }
    }
}
