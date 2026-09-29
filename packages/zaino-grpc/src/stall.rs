//! Streams whose client stopped reading
//!
//! - flow control stalls hyper on `poll_capacity`: it stops polling the body, so a stream cannot
//!   notice from inside, and a live peer that never reads keeps every permit it holds
//! - owing = a body handed hyper a data frame and has not been polled since (a `Pending` body =
//!   idle or waiting on a read, never owing)
//! - the connection's watchdog closes it once any stream has owed for the stall timeout

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// One connection's owing streams, by ticket id → since when
#[derive(Debug, Default)]
pub(crate) struct Watch {
    owing: Mutex<HashMap<u64, Instant>>,
    issued: AtomicU64,
}

impl Watch {
    pub(crate) fn ticket(self: &Arc<Self>) -> Ticket {
        let id = self.issued.fetch_add(1, Ordering::Relaxed);
        Ticket { watch: Arc::clone(self), id, owing: false }
    }

    fn oldest(&self) -> Option<Instant> {
        self.owing.lock().expect("stall watch poisoned").values().min().copied()
    }

    /// Resolves once some stream has owed for `timeout` (sleeps to the oldest deadline; with
    /// nothing owed, rechecks every `timeout`: detection within 2 × `timeout` at worst)
    pub(crate) async fn stalled(&self, timeout: Duration) {
        loop {
            let deadline = match self.oldest() {
                Some(since) if since.elapsed() >= timeout => return,
                Some(since) => since + timeout,
                None => Instant::now() + timeout,
            };
            tokio::time::sleep_until(deadline).await;
        }
    }
}

/// One stream's standing with its connection's [`Watch`] (locks only on a transition)
#[derive(Debug)]
pub(crate) struct Ticket {
    watch: Arc<Watch>,
    id: u64,
    owing: bool,
}

impl Ticket {
    /// Handed hyper a data frame: owing until the next poll
    pub(crate) fn handed(&mut self) {
        if !self.owing {
            self.watch.owing.lock().expect("stall watch poisoned").insert(self.id, Instant::now());
            self.owing = true;
        }
    }

    /// Polled again: hyper had room for more
    pub(crate) fn pulled(&mut self) {
        if self.owing {
            self.watch.owing.lock().expect("stall watch poisoned").remove(&self.id);
            self.owing = false;
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.pulled();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame handed and never pulled trips the watch at the timeout, not before; a pull or a
    /// dropped stream clears it; a quiet connection never trips
    #[tokio::test(start_paused = true)]
    async fn only_data_left_unpulled_for_the_timeout_trips_the_watch() {
        let timeout = Duration::from_secs(300);
        let watch = Arc::new(Watch::default());
        let tripped = |watch: &Arc<Watch>| {
            let watch = Arc::clone(watch);
            tokio::spawn(async move { watch.stalled(timeout).await })
        };

        let quiet = tripped(&watch);
        tokio::time::sleep(timeout * 3).await;
        assert!(!quiet.is_finished(), "no stream owes: never stalled");
        quiet.abort();

        let (mut reading, mut stuck) = (watch.ticket(), watch.ticket());
        reading.handed();
        stuck.handed();
        let watching = tripped(&watch);
        tokio::time::sleep(timeout / 2).await;
        reading.pulled();
        reading.handed();
        tokio::time::sleep(timeout / 2 - Duration::from_secs(1)).await;
        assert!(!watching.is_finished(), "1 s short of the timeout");
        tokio::time::sleep(Duration::from_secs(1)).await;
        watching.await.expect("the stuck stream trips it at the timeout");

        drop(stuck);
        reading.pulled();
        assert_eq!(watch.oldest(), None, "a dropped or pulled stream owes nothing");
    }
}
