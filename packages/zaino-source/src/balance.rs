//! Which validator answers: power of two choices over peak-EWMA load (`chainview.md` §9)
//!
//! ```text
//!   cost = latency estimate × (in flight + 1)
//!   pick = cheaper of two at random, then the rest by rising cost (failover order)
//!
//!   estimate ──▶ slower sample: jump to it (peak) │ faster: EWMA toward it │ idle: decay to 0
//! ```
//!
//! - the set of validators, never one: not a [`ChainDataSource`] (one validator's RPC); its one
//!   capability = [`failover`](TrafficBalancer::failover)
//! - tower's `PeakEwma` + `p2c` rule, on ports instead of `tower::Service`s
//! - idle decay = a once-slow source is retried, never starved for good

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;
use tracing::warn;

use crate::{ChainDataSource, QueryError};

/// Time constant of the estimate's decay toward faster samples, and toward 0 while idle
const DECAY: Duration = Duration::from_secs(10);

/// Estimate before a source's first answer (optimistic: an unmeasured source gets tried)
const INITIAL: Duration = Duration::from_millis(30);

/// Tries of one read on one validator while it fails transiently (`FailureMode::is_transient`)
const ATTEMPTS_PER_VALIDATOR: u32 = 3;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(250);

/// N sources, picked per request by load
pub struct TrafficBalancer<S> {
    members: Vec<Arc<Member<S>>>,
}

impl<S> Clone for TrafficBalancer<S> {
    fn clone(&self) -> Self {
        Self { members: self.members.clone() }
    }
}

struct Member<S> {
    source: Arc<S>,
    in_flight: AtomicU32,
    estimate: Mutex<Estimate>,
}

#[derive(Debug, Clone, Copy)]
struct Estimate {
    nanos: f64,
    at: Instant,
}

impl Estimate {
    fn decayed(self, now: Instant) -> f64 {
        let idle = now.saturating_duration_since(self.at).as_secs_f64();
        self.nanos * (-idle / DECAY.as_secs_f64()).exp()
    }

    fn observe(&mut self, sample: Duration, now: Instant) {
        let sample = sample.as_nanos() as f64;
        let decay =
            (-now.saturating_duration_since(self.at).as_secs_f64() / DECAY.as_secs_f64()).exp();
        self.nanos = match sample > self.nanos {
            true => sample,
            false => self.nanos * decay + sample * (1.0 - decay),
        };
        self.at = now;
    }
}

impl<S> Member<S> {
    fn cost(&self, now: Instant) -> f64 {
        let estimate = *self.estimate.lock().expect("load estimate mutex poisoned");
        estimate.decayed(now) * f64::from(self.in_flight.load(Ordering::Relaxed) + 1)
    }
}

impl<S> TrafficBalancer<S> {
    /// One member per source (non-empty, asserted)
    pub fn new(sources: Vec<Arc<S>>) -> Self {
        assert!(!sources.is_empty(), "no source to balance over");
        let estimate = Estimate { nanos: INITIAL.as_nanos() as f64, at: Instant::now() };
        let members = sources
            .into_iter()
            .map(|source| {
                let estimate = Mutex::new(estimate);
                Arc::new(Member { source, in_flight: AtomicU32::new(0), estimate })
            })
            .collect();
        Self { members }
    }

    /// Every member once: the cheaper of two at random, then the rest by rising cost
    fn candidates(&self) -> Vec<Candidate<S>> {
        let now = Instant::now();
        let mut rest: Vec<(f64, &Arc<Member<S>>)> =
            self.members.iter().map(|member| (member.cost(now), member)).collect();
        let first = match rest.len() {
            1 => 0,
            n => {
                let a = fastrand::usize(..n);
                let b = (a + 1 + fastrand::usize(..n - 1)) % n;
                if rest[b].0 < rest[a].0 {
                    b
                } else {
                    a
                }
            }
        };
        let picked = rest.swap_remove(first).1;
        rest.sort_by(|(left, _), (right, _)| left.total_cmp(right));
        std::iter::once(picked)
            .chain(rest.into_iter().map(|(_, member)| member))
            .map(|member| Candidate(Arc::clone(member)))
            .collect()
    }
}

/// One source, as picked: calls through it count toward its load
struct Candidate<S>(Arc<Member<S>>);

impl<S> Candidate<S> {
    /// `call` on this source: in flight counted for its duration, its latency folded in after
    async fn call<T, Fut>(&self, call: impl FnOnce(Arc<S>) -> Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        let member = &self.0;
        member.in_flight.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let answer = call(Arc::clone(&member.source)).await;
        let now = Instant::now();
        let mut estimate = member.estimate.lock().expect("load estimate mutex poisoned");
        estimate.observe(now.saturating_duration_since(started), now);
        drop(estimate);
        member.in_flight.fetch_sub(1, Ordering::Relaxed);
        answer
    }
}

impl<S: ChainDataSource> TrafficBalancer<S> {
    /// Every validator in turn until one answers (a lagging one lacks a just-mined block or
    /// transaction): a domain answer (every read's = absent) moves on; the result = the first
    /// answer, else a transport failure (it may have held it), else the last absence
    ///
    /// - a transient failure retried on the same validator first (`ATTEMPTS_PER_VALIDATOR`)
    pub async fn failover<T, E, Fut>(&self, ask: impl Fn(Arc<S>) -> Fut) -> Result<T, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
        Fut: Future<Output = Result<T, QueryError<E>>>,
    {
        let (mut absent, mut failed) = (None, None);
        for candidate in self.candidates() {
            let mut delay = FIRST_RETRY_DELAY;
            for attempt in 1..=ATTEMPTS_PER_VALIDATOR {
                match candidate.call(&ask).await {
                    Ok(answer) => return Ok(answer),
                    Err(QueryError::Domain(said)) => absent = Some(said),
                    Err(QueryError::NonDomain(cause))
                        if cause.mode.is_transient() && attempt < ATTEMPTS_PER_VALIDATOR =>
                    {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    Err(QueryError::NonDomain(cause)) => {
                        warn!(%cause, "Validator read failed, trying the next");
                        failed = Some(cause);
                    }
                }
                break;
            }
        }
        let unanswered = failed.map(QueryError::NonDomain).or(absent.map(QueryError::Domain));
        Err(unanswered.expect("TrafficBalancer is never empty (new asserts it)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paused clock: a source with requests piling up or a slow last answer loses the pick; idle
    /// decay brings the slow one back; failover order covers every member once
    #[tokio::test(start_paused = true)]
    async fn the_cheaper_source_wins_and_a_slow_one_is_retried_once_idle() {
        let balanced = TrafficBalancer::new(vec![Arc::new("near"), Arc::new("far")]);
        let firsts = |balanced: &TrafficBalancer<&'static str>| -> Vec<&'static str> {
            (0..64).map(|_| *balanced.candidates()[0].0.source).collect()
        };

        let far = balanced.candidates().into_iter().find(|c| *c.0.source == "far").expect("far");
        far.call(|_| tokio::time::sleep(Duration::from_secs(2))).await;
        assert!(firsts(&balanced).iter().all(|first| *first == "near"), "2 s answer = expensive");
        let order: Vec<&str> = balanced.candidates().iter().map(|c| *c.0.source).collect();
        assert_eq!(order, ["near", "far"], "every member once, cheapest first");

        tokio::time::advance(DECAY * 10).await;
        let near = balanced.candidates().into_iter().find(|c| *c.0.source == "near").expect("near");
        near.call(|_| tokio::time::sleep(Duration::from_millis(10))).await;
        let busy = near.0.in_flight.fetch_add(8, Ordering::Relaxed);
        assert_eq!(busy, 0);
        let firsts = firsts(&balanced);
        assert!(firsts.iter().all(|first| *first == "far"), "idle-decayed far beats 10 ms × 9");
    }
}
