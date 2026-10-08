//! Request classes, the lane each rides, per-member permits (`traffic-balancer.md` §3)

use std::time::Duration;

/// Ask kind: hedge floor, member kinds, round retry, synced (lane = permits + priority only)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Class {
    Poll,
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

/// Per member, one counter per lane
pub(crate) type PerLane<T> = [T; LANES];

/// One reserved per lane + one shared
pub(crate) const MIN_CONNECTIONS: u32 = LANES as u32 + 1;

impl Class {
    pub(crate) fn lane(self) -> Lane {
        match self {
            Self::Poll | Self::Submit | Self::Headers => Lane::Control,
            Self::TipBlock | Self::Lookup | Self::Bytes => Lane::Interactive,
            Self::BulkBlock => Lane::Bulk,
        }
    }

    /// `None` = never hedged; else a second member once the latest send is this old
    pub(crate) fn hedge_floor(self) -> Option<Duration> {
        match self {
            Self::TipBlock => Some(Duration::from_secs(2)),
            Self::Lookup => Some(Duration::from_secs(1)),
            Self::BulkBlock => Some(Duration::from_secs(15)),
            Self::Poll | Self::Submit | Self::Headers | Self::Bytes => None,
        }
    }

    /// Checkable answers only (trusted-only = the "only source" rows, `chainview.md` §1)
    pub(crate) fn peers(self) -> bool {
        !matches!(self, Self::Poll | Self::Submit | Self::Lookup)
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
            Self::Poll => "poll",
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
        self as usize
    }
}

/// One member's connections: `reserve` per lane no other lane takes, the rest shared
#[derive(Debug, Clone)]
pub(crate) struct Permits {
    max: u32,
    reserve: u32,
}

impl Permits {
    pub(crate) fn trusted(max: u32) -> Self {
        assert!(max >= MIN_CONNECTIONS, "max_connections covers every lane reserve + one shared");
        Self { max, reserve: 1 }
    }

    /// One request at a time (a zebra peer connection serves one)
    pub(crate) fn peer() -> Self {
        Self { max: 1, reserve: 0 }
    }

    pub(crate) fn admits(&self, in_flight: &PerLane<u32>, lane: Lane) -> bool {
        in_flight[lane.index()] < self.reserve || self.claimed(in_flight) < self.max
    }

    /// T1: Σ in flight ≤ max, reserves never borrowed
    pub(crate) fn holds(&self, in_flight: &PerLane<u32>) -> bool {
        self.claimed(in_flight) <= self.max
    }

    /// Each lane's in flight, or its reserve if larger
    fn claimed(&self, in_flight: &PerLane<u32>) -> u32 {
        in_flight.iter().map(|n| (*n).max(self.reserve)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - MIN_CONNECTIONS = 4: a lane fills the shared permits, never another lane's reserve
    /// - each lane still admits its reserved one with every shared permit taken
    /// - peers: one in flight, any lane
    #[test]
    fn lanes_share_idle_permits_but_never_a_reserve() {
        let permits = Permits::trusted(8);
        assert_eq!(MIN_CONNECTIONS, 4);
        let mut in_flight = [0; LANES];
        while permits.admits(&in_flight, Lane::Bulk) {
            in_flight[Lane::Bulk.index()] += 1;
        }
        assert_eq!(in_flight, [0, 0, 6], "bulk stops where the other reserves begin");
        for lane in [Lane::Control, Lane::Interactive] {
            assert!(permits.admits(&in_flight, lane), "{lane:?} reserve held");
            in_flight[lane.index()] += 1;
            assert!(!permits.admits(&in_flight, lane), "{lane:?}: one reserved, none shared");
        }
        assert!(permits.holds(&in_flight));
        assert!(!permits.holds(&[7, 1, 0]), "control on the idle bulk reserve breaks T1");

        let peer = Permits::peer();
        assert!(peer.admits(&[0, 0, 0], Lane::Control) && !peer.admits(&[0, 1, 0], Lane::Bulk));
    }
}
