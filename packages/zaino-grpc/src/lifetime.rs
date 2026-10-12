//! Connection lifetime (gRPC A9): GOAWAY at `max_connection_age` or after `max_connection_idle`
//! with no open stream, in-flight streams then given `max_connection_age_grace`
//!
//! - wallets hold one connection for hours: without an age they never re-resolve DNS, so neither
//!   a load balancer nor a replaced node moves them
//! - age ±10% per connection (connections opened together never all cycle together)

use std::hash::BuildHasher as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// Share of `max_connection_age` one connection's age may differ by, either way
const JITTER: f64 = 0.1;

/// One connection's age: uniform in `age` ± [`JITTER`]
pub(crate) fn jittered(age: Duration) -> Duration {
    // a fresh `RandomState` = fresh random keys: a random u64 without an RNG dependency
    let random = std::collections::hash_map::RandomState::new().hash_one(0u8);
    let unit = random as f64 / u64::MAX as f64;
    age.mul_f64(1.0 - JITTER + 2.0 * JITTER * unit)
}

/// Every connection open now gets a GOAWAY + `max_connection_age_grace` (an operator taking the
/// node out of rotation); connections opened later are untouched
#[derive(Clone, Debug)]
pub struct Retire(Arc<tokio::sync::watch::Sender<u64>>);

impl Default for Retire {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(0)))
    }
}

impl Retire {
    pub fn retire_all(&self) {
        self.0.send_modify(|generation| *generation += 1);
    }

    /// One accepted connection's subscription: [`Retirement::fired`] at the next
    /// [`Self::retire_all`] after this call
    pub(crate) fn subscribe(&self) -> Retirement {
        Retirement(self.0.subscribe())
    }
}

/// Taken at accept (a `retire_all` after it reaches the connection, however far its handshake got)
#[derive(Debug)]
pub(crate) struct Retirement(tokio::sync::watch::Receiver<u64>);

impl Retirement {
    pub(crate) async fn fired(mut self) {
        // the sender lives as long as the service: an error = never
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
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
        if let Ok(mut open) = self.activity.open.lock() {
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
        let (low, high) = (age.mul_f64(0.9), age.mul_f64(1.1));
        assert!(ages.iter().all(|at| (low..=high).contains(at)), "within ±10%");
        let spread = *ages.iter().max().expect("1,000") - *ages.iter().min().expect("1,000");
        assert!(spread > age.mul_f64(0.15), "spread across the band: {spread:?}");
    }

    /// Paused clock, idle = 60 s: quiet from the start → idle at 60 s; a stream open the whole
    /// time holds it off; idle counts from the last close, not from the first open
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
