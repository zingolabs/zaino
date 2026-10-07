//! Admission arithmetic: per-member permits and rate, one retry budget (pure, time as input)

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use crate::class::{Class, PerClass, CLASSES};

const RESERVED: u32 = {
    let mut sum = 0;
    let mut class = 0;
    while class < CLASSES {
        sum += Class::ALL[class].reserve();
        class += 1;
    }
    sum
};

/// Σ reserves + one shared (below it a class with no reserve never runs)
pub(crate) const MIN_CONNECTIONS: u32 = RESERVED + 1;

/// One member's connections, shared by class
///
/// - admitted while under its ceiling and either under its reserve or no other class's unused
///   reserve taken (lanes = ceiling equal to reserve)
#[derive(Debug, Clone)]
pub(crate) struct Permits {
    max: u32,
    reserve: PerClass<u32>,
    ceiling: PerClass<u32>,
}

impl Permits {
    pub(crate) fn trusted(max: u32) -> Self {
        assert!(max >= MIN_CONNECTIONS, "max_connections covers every reserve + one shared");
        let reserve = Class::ALL.map(Class::reserve);
        let ceiling = Class::ALL.map(|class| {
            let rest = max - (RESERVED - class.reserve());
            let scaled = u64::from(class.ceiling_per_32()) * u64::from(max);
            let scaled = u32::try_from(scaled.div_ceil(32)).unwrap_or(u32::MAX);
            scaled.max(class.reserve()).min(rest)
        });
        Self { max, reserve, ceiling }
    }

    /// One request at a time (a zebra peer connection serves one)
    pub(crate) fn peer() -> Self {
        let ceiling = Class::ALL.map(|class| u32::from(class.peers()));
        Self { max: 1, reserve: [0; CLASSES], ceiling }
    }

    pub(crate) fn admits(&self, in_flight: &PerClass<u32>, class: Class) -> bool {
        let used = in_flight[class.index()];
        used < self.ceiling[class.index()]
            && (used < self.reserve[class.index()] || self.claimed(in_flight) < self.max)
    }

    /// T1: Σ in flight ≤ max, per class ≤ ceiling, reserves never borrowed
    pub(crate) fn holds(&self, in_flight: &PerClass<u32>) -> bool {
        let under_ceilings = (0..CLASSES).all(|class| in_flight[class] <= self.ceiling[class]);
        under_ceilings && self.claimed(in_flight) <= self.max
    }

    /// Each class's in flight, or its reserve if larger
    fn claimed(&self, in_flight: &PerClass<u32>) -> u32 {
        (0..CLASSES).map(|class| in_flight[class].max(self.reserve[class])).sum()
    }
}

/// Requests one rate limit admits at once
const BURST: Duration = Duration::from_secs(1);

/// GCRA over `max_requests_per_sec` (`tat` = when every charged request has drained)
///
/// - hand-rolled: `governor` has no non-consuming check (a pick compares members first)
/// - a batch larger than the burst admitted once drained (never starved)
#[derive(Debug, Clone)]
pub(crate) struct Gcra {
    per_request: Duration,
    tat: Option<Instant>,
}

impl Gcra {
    pub(crate) fn new(per_sec: NonZeroU32) -> Self {
        Self { per_request: Duration::from_secs(1) / per_sec.get(), tat: None }
    }

    pub(crate) fn headroom(&self, now: Instant) -> bool {
        self.tat.is_none_or(|tat| tat <= now + BURST)
    }

    pub(crate) fn ready_at(&self, now: Instant) -> Instant {
        now + self.tat.map_or(Duration::ZERO, |tat| tat.saturating_duration_since(now + BURST))
    }

    pub(crate) fn charge(&mut self, now: Instant, requests: u32) {
        let from = self.tat.map_or(now, |tat| tat.max(now));
        self.tat = Some(from + self.per_request * requests);
    }
}

/// Retries + hedges, every member at once: tower `TpsBudget`'s rule as a token bucket
///
/// - milli-tokens: first attempt deposits 10 %, time 1 per second, one retry costs 1, cap 10
#[derive(Debug, Clone)]
pub(crate) struct RetryBudget {
    milli: i64,
    at: Instant,
}

const DEPOSIT: i64 = 100;
const RETRY_COST: i64 = 1_000;
const BUDGET_CAP: i64 = 10_000;

impl RetryBudget {
    pub(crate) fn new(now: Instant) -> Self {
        Self { milli: BUDGET_CAP, at: now }
    }

    fn balance(&self, now: Instant) -> i64 {
        let elapsed = now.saturating_duration_since(self.at).as_millis();
        let accrued = i64::try_from(elapsed).unwrap_or(BUDGET_CAP);
        self.milli.saturating_add(accrued).min(BUDGET_CAP)
    }

    pub(crate) fn deposit(&mut self, now: Instant) {
        self.milli = (self.balance(now) + DEPOSIT).min(BUDGET_CAP);
        self.at = self.at.max(now);
    }

    pub(crate) fn available(&self, now: Instant) -> bool {
        self.balance(now) >= RETRY_COST
    }

    pub(crate) fn withdraw(&mut self, now: Instant) {
        assert!(self.available(now), "a retry withdrawn from a budget that holds one");
        self.milli = self.balance(now) - RETRY_COST;
        self.at = self.at.max(now);
    }

    pub(crate) fn ready_at(&self, now: Instant) -> Instant {
        let short = u64::try_from(RETRY_COST - self.balance(now)).unwrap_or(0);
        now + Duration::from_millis(short)
    }

    /// T7
    pub(crate) fn holds(&self) -> bool {
        (0..=BUDGET_CAP).contains(&self.milli)
    }

    #[cfg(test)]
    pub(crate) fn overdrawn(now: Instant) -> Self {
        Self { milli: -RETRY_COST, at: now }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// - 32 connections: reserves (Σ 5, + 1 shared = MIN_CONNECTIONS), ceilings scaled, bulk =
    ///   the rest
    /// - a burst of lookups stops at its ceiling; bulk fills to the rest, never into reserves
    /// - each class still admits its reserved one with everything else full
    /// - at MIN_CONNECTIONS every class runs (no-reserve classes on the shared one)
    #[test]
    fn classes_borrow_idle_permits_but_never_a_reserve() {
        let permits = Permits::trusted(32);
        assert_eq!(MIN_CONNECTIONS, 6);
        assert_eq!(permits.reserve, [1, 1, 1, 0, 1, 0, 1]);
        assert_eq!(permits.ceiling, [1, 2, 4, 4, 8, 2, 28]);
        let mut in_flight = [0; CLASSES];
        let fill = |in_flight: &mut PerClass<u32>, class: Class| {
            while permits.admits(in_flight, class) {
                in_flight[class.index()] += 1;
            }
        };
        fill(&mut in_flight, Class::Lookup);
        fill(&mut in_flight, Class::BulkBlock);
        assert_eq!(in_flight, [0, 0, 0, 0, 8, 0, 21], "bulk stops where reserves begin");
        assert!(permits.holds(&in_flight));
        for class in [Class::Poll, Class::Submit, Class::TipBlock] {
            fill(&mut in_flight, class);
        }
        assert_eq!(in_flight, [1, 1, 1, 0, 8, 0, 21]);
        assert!(!permits.admits(&in_flight, Class::Headers), "no reserve, nothing free");
        in_flight[Class::Headers.index()] = 1;
        assert!(!permits.holds(&in_flight), "a borrowed reserve breaks T1");

        let small = Permits::trusted(MIN_CONNECTIONS);
        assert_eq!(small.ceiling, [1, 1, 1, 1, 2, 1, 2], "every class runs");
        assert_eq!(Permits::peer().ceiling, [0, 0, 1, 1, 0, 1, 1], "peers: checkable only");
    }

    /// - 2/s: a 1 s burst passes, then one per 500 ms; a batch bigger than the burst passes once
    ///   drained and holds the next back for its full cost
    /// - budget: 10 retries from full, refilled by first attempts (10 %) and time (1/s)
    #[test]
    fn rate_and_budget_admit_their_burst_then_refill_at_their_rate() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut rate = Gcra::new(NonZeroU32::new(2).expect("nonzero"));
        let admitted: Vec<bool> = (0..4)
            .map(|_| {
                let ok = rate.headroom(t0);
                if ok {
                    rate.charge(t0, 1);
                }
                ok
            })
            .collect();
        assert_eq!(admitted, [true, true, true, false], "burst = 1 s of rate, plus one");
        assert_eq!(rate.ready_at(t0), ms(500));
        assert!(rate.headroom(ms(500)));
        let mut batch = Gcra::new(NonZeroU32::new(2).expect("nonzero"));
        batch.charge(t0, 10);
        assert_eq!((batch.headroom(ms(3_999)), batch.ready_at(t0)), (false, ms(4_000)));

        let mut budget = RetryBudget::new(t0);
        for _ in 0..10 {
            budget.withdraw(t0);
        }
        assert!(!budget.available(t0) && budget.holds());
        assert_eq!(budget.ready_at(t0), ms(1_000), "time refills 1/s");
        (0..10).for_each(|_| budget.deposit(t0));
        assert!(budget.available(t0), "10 first attempts = one retry");
        budget.withdraw(t0);
        budget.deposit(ms(60_000));
        assert_eq!(budget.balance(ms(60_000)), BUDGET_CAP, "capped at 10");
    }
}
