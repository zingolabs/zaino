//! One member: identity, health, bench, permits, poll cadence (`traffic-balancer.md` §3, §4)

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use crate::class::{Lane, PerLane, Permits, LANES, MIN_CONNECTIONS};

/// Configured order, `< MAX`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValidatorId(u8);

impl ValidatorId {
    /// chainview's `EndpointSet::MAX`
    pub const MAX: usize = 64;

    pub fn new(index: usize) -> Option<Self> {
        u8::try_from(index).ok().filter(|_| index < Self::MAX).map(Self)
    }

    pub fn get(self) -> usize {
        usize::from(self.0)
    }
}

/// WorkPool connection (never reused)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberId {
    Trusted(ValidatorId),
    Peer(PeerId),
}

/// One trusted validator's connection budget
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    max_connections: u32,
}

impl Limits {
    /// The poll's own + one reserved per lane (control, interactive, bulk) + one shared
    pub const MIN_CONNECTIONS: u32 = MIN_CONNECTIONS;

    /// `None` below [`MIN_CONNECTIONS`](Self::MIN_CONNECTIONS)
    ///
    /// - 2nd arg ignored (request-rate limit deleted; dropped once chainview's tests pass `(n)`)
    pub fn new(max_connections: u32, _: Option<NonZeroU32>) -> Option<Self> {
        (max_connections >= MIN_CONNECTIONS).then_some(Self { max_connections })
    }
}

/// Poll = the active check, every reply the passive one (chainview's `EndpointState`, moved)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Pending,
    Live,
    CatchingUp,
    Degraded,
    Down,
}

impl Health {
    /// Metric label + status text
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Live => "live",
            Self::CatchingUp => "catching_up",
            Self::Degraded => "degraded",
            Self::Down => "down",
        }
    }
}

/// Every member as the balancer sees it (/statusz, metrics)
#[derive(Debug, Clone, PartialEq)]
pub struct MemberTable {
    pub rows: Vec<MemberRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemberRow {
    pub id: MemberId,
    pub health: Health,
    pub failures: u32,
    pub benched_until: Option<Instant>,
    pub latency: Duration,
    pub in_flight: u32,
}

/// Last answered poll (peers: `Live` from joining)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Synced {
    Live,
    CatchingUp,
}

/// Consecutive failures → `Down` (its poll probe alone, at the ladder's ceiling)
pub(crate) const DOWN_AFTER: u32 = 10;
const BENCH: Duration = Duration::from_secs(60);
const BENCH_MAX: Duration = Duration::from_secs(3600);
const PEER_TIER: u16 = u8::MAX as u16 + 1;

/// `last_sent` / `down_since` / `Bench::stamp` = core event order (T3); `latency` = last reply's
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub(crate) tier: u16,
    pub(crate) synced: Option<Synced>,
    pub(crate) failures: u32,
    pub(crate) down_since: Option<u64>,
    pub(crate) bench: Option<Bench>,
    pub(crate) latency: Duration,
    pub(crate) in_flight: PerLane<u32>,
    pub(crate) permits: Permits,
    pub(crate) last_sent: Option<u64>,
    pub(crate) poller: Option<Poller>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Bench {
    pub(crate) until: Instant,
    pub(crate) times: u32,
    pub(crate) stamp: u64,
}

impl Member {
    pub(crate) fn trusted(priority: u8, limits: Limits) -> Self {
        let mut member = Self::new(u16::from(priority), Permits::trusted(limits.max_connections));
        member.poller = Some(Poller::default());
        member
    }

    pub(crate) fn peer() -> Self {
        let mut member = Self::new(PEER_TIER, Permits::peer());
        member.synced = Some(Synced::Live);
        member
    }

    fn new(tier: u16, permits: Permits) -> Self {
        Self {
            tier,
            synced: None,
            failures: 0,
            down_since: None,
            bench: None,
            latency: Duration::ZERO,
            in_flight: [0; LANES],
            permits,
            last_sent: None,
            poller: None,
        }
    }

    pub(crate) fn health(&self) -> Health {
        match (self.failures, self.synced) {
            (failures, _) if failures >= DOWN_AFTER => Health::Down,
            (failures, _) if failures > 0 => Health::Degraded,
            (_, None) => Health::Pending,
            (_, Some(Synced::Live)) => Health::Live,
            (_, Some(Synced::CatchingUp)) => Health::CatchingUp,
        }
    }

    pub(crate) fn benched(&self, now: Instant) -> bool {
        self.bench.is_some_and(|bench| bench.until > now)
    }

    /// 60 s × 2^(times − 1), ≤ 1 h
    pub(crate) fn bench(&mut self, now: Instant, stamp: u64) {
        let times = self.bench.map_or(1, |bench| bench.times + 1);
        let span = BENCH.saturating_mul(1 << (times - 1).min(6)).min(BENCH_MAX);
        self.bench = Some(Bench { until: now + span, times, stamp });
    }

    /// Passive (any reply) or active (poll) check
    pub(crate) fn outcome(&mut self, answered: bool, stamp: u64) {
        match answered {
            true => (self.failures, self.down_since) = (0, None),
            false => {
                self.failures += 1;
                if self.failures == DOWN_AFTER {
                    self.down_since = Some(stamp);
                }
            }
        }
    }

    pub(crate) fn room(&self, lane: Lane) -> bool {
        self.permits.admits(&self.in_flight, lane)
    }

    /// Lane sends + its poll
    pub(crate) fn load(&self) -> u32 {
        let polling = self.poller.as_ref().is_some_and(|poller| poller.in_flight);
        self.in_flight.iter().sum::<u32>() + u32::from(polling)
    }
}

const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Push stream up: events wake it sooner, the poll stays the truth
const STREAMED_POLL_INTERVAL: Duration = Duration::from_secs(15);
pub(crate) const MIN_POLL_SPACING: Duration = Duration::from_millis(200);
const LADDER_START: Duration = Duration::from_millis(500);
pub(crate) const LADDER_CEILING: Duration = Duration::from_secs(30);
const METADATA_REFRESH: Duration = Duration::from_secs(60);

/// One trusted member's poll cadence (`last` = latest start, `previous` = the one before)
#[derive(Debug, Clone, Default)]
pub(crate) struct Poller {
    pub(crate) last: Option<Instant>,
    pub(crate) previous: Option<Instant>,
    pub(crate) in_flight: bool,
    pub(crate) wake: bool,
    pub(crate) streaming: bool,
    metadata_at: Option<Instant>,
    metadata_asked: bool,
}

impl Poller {
    /// Interval, a wake (≥ 200 ms after the last), or the ladder while failing
    pub(crate) fn due(&self, failures: u32, now: Instant) -> Instant {
        let Some(last) = self.last else { return now };
        let wait = match failures {
            0 if self.wake => MIN_POLL_SPACING,
            0 if self.streaming => STREAMED_POLL_INTERVAL,
            0 => POLL_INTERVAL,
            failing if self.wake && failing < DOWN_AFTER => MIN_POLL_SPACING,
            failing => LADDER_START.saturating_mul(1 << (failing - 1).min(16)).min(LADDER_CEILING),
        };
        last + wait
    }

    /// `true` = metadata due with this poll
    pub(crate) fn start(&mut self, now: Instant) -> bool {
        (self.previous, self.last, self.in_flight, self.wake) = (self.last, Some(now), true, false);
        self.metadata_asked = self.metadata_at.is_none_or(|at| now >= at + METADATA_REFRESH);
        self.metadata_asked
    }

    /// Answered = its metadata read (a failed poll leaves it due: the next answer carries it)
    pub(crate) fn finished(&mut self, answered: bool) {
        self.in_flight = false;
        if std::mem::take(&mut self.metadata_asked) && answered {
            self.metadata_at = self.last;
        }
    }
}
