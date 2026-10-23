//! Connection lifetime (gRPC A9): GOAWAY at `max_connection_age` or after `max_connection_idle`
//! with no open stream, in-flight streams then given `max_connection_age_grace`
//!
//! - wallets hold one connection for hours: without an age they never re-resolve DNS, so neither
//!   a load balancer nor a replaced node moves them
//! - age ±10% per connection (connections opened together never all cycle together)

use std::hash::BuildHasher as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::time::Instant;

/// Share of `max_connection_age` one connection's age may differ by, either way
pub(crate) const JITTER: f64 = 0.1;

/// One connection's age: uniform in `age` ± [`JITTER`]
pub(crate) fn jittered(age: Duration) -> Duration {
    // a fresh `RandomState` = fresh random keys: a random u64 without an RNG dependency
    let random = std::collections::hash_map::RandomState::new().hash_one(0u8);
    let unit = random as f64 / u64::MAX as f64;
    age.mul_f64(1.0 - JITTER + 2.0 * JITTER * unit)
}

/// Operator hold (taking the node out of rotation): while held, every connection, open or accepted
/// later, gets a GOAWAY + `max_connection_age_grace` once the hold's delay has passed
///
/// - owner-held ([`GrpcService::with_retire`](crate::GrpcService::with_retire)): a hold may start
///   before the server exists
#[derive(Clone, Debug)]
pub struct Retire(Arc<watch::Sender<Option<Instant>>>);

/// Where the hold stands (`Draining` = held, GOAWAY not due yet)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Off,
    Draining,
    Drained,
}

impl Default for Retire {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(None)))
    }
}

impl Retire {
    /// Held from now, GOAWAY due after `delay` (already held = its first deadline kept)
    pub fn hold(&self, delay: Duration) {
        let due = Instant::now() + delay;
        self.0.send_if_modified(|held| match held {
            Some(_) => false,
            None => {
                *held = Some(due);
                true
            }
        });
    }

    /// Hold lifted: nothing more retired (connections already GOAWAY'd stay so)
    pub fn resume(&self) {
        self.0.send_if_modified(|held| held.take().is_some());
    }

    pub fn state(&self) -> Hold {
        match *self.0.borrow() {
            None => Hold::Off,
            Some(due) if Instant::now() < due => Hold::Draining,
            Some(_) => Hold::Drained,
        }
    }

    /// One accepted connection's view of the hold
    pub(crate) fn subscribe(&self) -> Retirement {
        Retirement(self.0.subscribe())
    }
}

/// Taken at accept (a hold reaches the connection however far its handshake got)
#[derive(Debug)]
pub(crate) struct Retirement(watch::Receiver<Option<Instant>>);

impl Retirement {
    /// Once a hold's deadline passes with that hold still on (resumed first = re-armed by the next)
    pub(crate) async fn fired(mut self) {
        loop {
            let held = *self.0.borrow_and_update();
            match held {
                // sender gone (service dropped) = this deadline stands
                Some(due) => tokio::select! {
                    () = tokio::time::sleep_until(due) => return,
                    Ok(()) = self.0.changed() => {}
                },
                None => {
                    if self.0.changed().await.is_err() {
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }
}

/// One connection's open streams + since when it has had none
#[derive(Debug)]
pub(crate) struct Activity {
    open: Mutex<(usize, Instant)>,
    changed: Notify,
}

impl Default for Activity {
    fn default() -> Self {
        Self { open: Mutex::new((0, Instant::now())), changed: Notify::new() }
    }
}

impl Activity {
    /// Counted open until dropped
    pub(crate) fn opened(self: &Arc<Self>) -> Open {
        self.open.lock().expect("connection activity poisoned").0 += 1;
        Open { activity: Arc::clone(self) }
    }

    /// Resolves once no stream has been open for `idle`
    pub(crate) async fn idle(&self, idle: Duration) {
        loop {
            let changed = self.changed.notified();
            let mut changed = std::pin::pin!(changed);
            // registered before the read: a close between the two still wakes this
            changed.as_mut().enable();
            let (open, quiet_since) = *self.open.lock().expect("connection activity poisoned");
            if open > 0 {
                changed.await;
                continue;
            }
            let deadline = quiet_since + idle;
            if Instant::now() >= deadline {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                () = changed => {}
            }
        }
    }
}

/// One open stream (request in, response body not yet dropped)
#[derive(Debug)]
pub(crate) struct Open {
    activity: Arc<Activity>,
}

impl Drop for Open {
    fn drop(&mut self) {
        {
            let mut open = self.activity.open.lock().expect("open-streams mutex poisoned");
            open.0 -= 1;
            if open.0 == 0 {
                open.1 = Instant::now();
            }
        }
        self.activity.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1,000 connections at a 30 min age: every one within ±10%, spread across it (not one value)
    #[test]
    fn each_connection_ages_within_ten_percent_either_way() {
        let age = Duration::from_secs(30 * 60);
        let ages: Vec<_> = (0..1_000).map(|_| jittered(age)).collect();
        let band = age.mul_f64(1.0 - JITTER)..=age.mul_f64(1.0 + JITTER);
        let outside: Vec<_> = ages.iter().filter(|at| !band.contains(*at)).collect();
        assert_eq!(outside, Vec::<&Duration>::new(), "ages outside {band:?}");
        let spread = *ages.iter().max().expect("1,000") - *ages.iter().min().expect("1,000");
        assert!(spread > age.mul_f64(0.15), "spread across the band: {spread:?}");
    }

    /// Paused clock, delay 10 s, one connection open before the first hold:
    ///
    /// - hold at 0 s, resume at 3 s, hold at 5 s: fires at 15 s (second hold's deadline), not 10 s
    /// - `state` Off → Draining → Drained, repeated `hold` keeps the first deadline
    /// - connection accepted past the deadline fires at once, after `resume` never
    #[tokio::test(start_paused = true)]
    async fn a_hold_retires_every_connection_at_its_own_deadline_and_a_resume_disarms_it() {
        let delay = Duration::from_secs(10);
        let retire = Retire::default();
        let start = Instant::now();
        let open = tokio::spawn(retire.subscribe().fired());
        assert_eq!(retire.state(), Hold::Off);

        retire.hold(delay);
        tokio::time::sleep(Duration::from_secs(3)).await;
        retire.resume();
        tokio::time::sleep(Duration::from_secs(2)).await;
        retire.hold(delay);
        tokio::time::sleep(Duration::from_secs(4)).await;
        retire.hold(Duration::ZERO);
        assert_eq!(retire.state(), Hold::Draining, "repeat hold: first deadline kept");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!open.is_finished(), "11 s: the first hold's deadline was resumed away");
        open.await.expect("fires");
        assert_eq!(start.elapsed(), Duration::from_secs(15), "the second hold's deadline");
        assert_eq!(retire.state(), Hold::Drained);

        let late = Instant::now();
        retire.subscribe().fired().await;
        assert_eq!(late.elapsed(), Duration::ZERO, "accepted while drained: GOAWAY at once");

        retire.resume();
        let after = tokio::spawn(retire.subscribe().fired());
        tokio::time::sleep(delay * 10).await;
        assert!(!after.is_finished(), "resumed: never retired");
        assert_eq!(retire.state(), Hold::Off);
    }

    /// Paused clock, idle = 60 s:
    ///
    /// - quiet from the start → idle at 60 s
    /// - stream open the whole time holds it off
    /// - idle counts from the last close, not from the first open
    #[tokio::test(start_paused = true)]
    async fn idle_counts_from_the_last_stream_closed() {
        let idle = Duration::from_secs(60);
        let activity = Arc::new(Activity::default());
        let start = Instant::now();
        activity.idle(idle).await;
        assert_eq!(start.elapsed(), idle, "never used: idle at 60 s");

        let open = activity.opened();
        let waiting = {
            let activity = Arc::clone(&activity);
            tokio::spawn(async move { activity.idle(idle).await })
        };
        tokio::time::sleep(idle * 5).await;
        assert!(!waiting.is_finished(), "a stream open: never idle");
        let closed = Instant::now();
        drop(open);
        waiting.await.expect("idle once closed");
        assert_eq!(closed.elapsed(), idle, "60 s after the last close");
    }
}
