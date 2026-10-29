//! Engine-wide merge slots: <= `LsmConfig::merge_slots` merges at once across every set of every
//! index, sharing `LsmConfig::merge_mib_per_sec`
//!
//! - next free slot → lowest waiting tier
//! - cap: else a merge per tier per set = dozens of threads competing with serving reads for disk
//! - bandwidth: merges never take the whole device (commits' fsyncs + folds' reads queue behind)
//! - lowest tier first: small merges (bound a commit's stall) never queue behind a large one
//! - slot holder waits only on its own I/O + the shared bandwidth → every waiting merge gets a slot

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    num::NonZeroU64,
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

/// How often a waiting merge rechecks its cancel flag
const CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub(crate) struct Slots {
    state: Mutex<State>,
    freed: Condvar,
    capacity: usize,
    bandwidth: Bandwidth,
}

/// Merge bytes per second across every slot holder: a debt bucket, one second's worth of burst
///
/// - each charge sleeps off the debt it finds (concurrent merges share the rate, never exceed it)
/// - `bucket.0` = bytes available (< 0 = owed)
#[derive(Debug)]
struct Bandwidth {
    per_sec: NonZeroU64,
    bucket: Mutex<(i128, Instant)>,
}

impl Bandwidth {
    const NANOS_PER_SEC: i128 = 1_000_000_000;

    fn new(per_sec: NonZeroU64) -> Self {
        Self { per_sec, bucket: Mutex::new((i128::from(per_sec.get()), Instant::now())) }
    }

    fn charge(&self, bytes: usize) {
        if let Some(debt) = self.debt(bytes, Instant::now()) {
            thread::sleep(debt);
        }
    }

    /// `bytes` charged at `now`: the wait it owes (`None` = within the burst)
    fn debt(&self, bytes: usize, now: Instant) -> Option<Duration> {
        let mut bucket = self.bucket.lock().expect("bandwidth never held across a panic");
        let (tokens, at) = &mut *bucket;
        let per_sec = i128::from(self.per_sec.get());
        let idle =
            i128::try_from(now.saturating_duration_since(*at).as_nanos()).unwrap_or(i128::MAX);
        let refill = idle.saturating_mul(per_sec) / Self::NANOS_PER_SEC;
        let charged = i128::try_from(bytes).expect("usize fits i128");
        *tokens = tokens.saturating_add(refill).min(per_sec) - charged;
        *at = now.max(*at);
        (*tokens < 0).then(|| {
            let owed = -*tokens * Self::NANOS_PER_SEC / per_sec;
            Duration::from_nanos(u64::try_from(owed).unwrap_or(u64::MAX))
        })
    }
}

/// `waiting` = `(tier, arrival)`: lowest waiting tier first, ties in arrival order
#[derive(Debug)]
struct State {
    running: usize,
    waiting: BinaryHeap<Reverse<(u32, u64)>>,
    arrivals: u64,
}

/// Held slot, returned on drop
pub(super) struct Slot<'a> {
    slots: &'a Slots,
}

impl Slots {
    pub(crate) fn new(capacity: usize, bytes_per_sec: NonZeroU64) -> Self {
        Self {
            state: Mutex::new(State { running: 0, waiting: BinaryHeap::new(), arrivals: 0 }),
            freed: Condvar::new(),
            capacity,
            bandwidth: Bandwidth::new(bytes_per_sec),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("merge slots never held across a panic")
    }

    /// Waits for a slot (`None` if `cancel` raised first)
    pub(super) fn acquire(&self, tier: u32, cancel: &AtomicBool) -> Option<Slot<'_>> {
        let mut state = self.lock();
        let ticket = Reverse((tier, state.arrivals));
        state.arrivals += 1;
        state.waiting.push(ticket);
        loop {
            if cancel.load(Ordering::Relaxed) {
                state.waiting.retain(|waiting| *waiting != ticket);
                self.freed.notify_all();
                return None;
            }
            if state.running < self.capacity && state.waiting.peek() == Some(&ticket) {
                state.waiting.pop();
                state.running += 1;
                return Some(Slot { slots: self });
            }
            state = self.freed.wait_timeout(state, CANCEL_POLL).expect("slots mutex poisoned").0;
        }
    }
}

impl Slot<'_> {
    /// `bytes` of merge I/O done: waits while the engine's merges run ahead of their bandwidth
    pub(super) fn charge(&self, bytes: usize) {
        self.slots.bandwidth.charge(bytes);
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.slots.lock().running -= 1;
        self.slots.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{sync::Arc, thread};

    /// - never past capacity at once
    /// - freed slot → lowest waiting tier
    /// - cancelled waiter leaves the queue without a slot
    #[test]
    fn slots_cap_concurrency_and_serve_the_lowest_tier_first() {
        let slots: &'static Slots = Box::leak(Box::new(Slots::new(1, NonZeroU64::MAX)));
        let running = AtomicBool::new(false);
        let held = slots.acquire(5, &running).expect("free slot");

        let order = Arc::new(Mutex::new(Vec::new()));
        let waiters: Vec<_> = [7, 2, 4]
            .into_iter()
            .enumerate()
            .map(|(queued, tier)| {
                let order = Arc::clone(&order);
                let handle = thread::spawn(move || {
                    let _slot = slots.acquire(tier, &AtomicBool::new(false)).expect("slot");
                    order.lock().expect("order").push(tier);
                });
                // every waiter queued before the slot frees
                while slots.lock().waiting.len() <= queued {
                    thread::yield_now();
                }
                handle
            })
            .collect();

        let cancel = AtomicBool::new(true);
        assert!(slots.acquire(0, &cancel).is_none(), "cancelled before a slot frees");
        assert_eq!(slots.lock().waiting.len(), 3, "the cancelled waiter left the queue");

        drop(held);
        for waiter in waiters {
            waiter.join().expect("waiter");
        }
        assert_eq!(*order.lock().expect("order"), [2, 4, 7], "lowest tier first");
        assert_eq!(slots.lock().running, 0, "slots running once every waiter finished");
    }

    /// 20 MB/s: one second's worth (20 MB) free, every byte past it owed at the rate, idle time
    /// refilling at most one second's worth
    #[test]
    fn bandwidth_lets_one_second_s_burst_through_then_paces_to_the_rate() {
        const MB: usize = 1_000_000;
        let bandwidth = Bandwidth::new(NonZeroU64::new(20_000_000).expect("20 MB/s"));
        let created = bandwidth.bucket.lock().expect("bucket").1;
        let at = |secs: f64| created + Duration::from_secs_f64(secs);
        #[rustfmt::skip]
        let charges = [
            ("the whole burst",              at(0.0),  20 * MB, None),
            ("1 MB past it",                 at(0.0),  MB,      Some(50)),
            ("1 MB as the 50 ms debt clears", at(0.05), MB,     Some(50)),
            ("30 MB after 10 s idle",        at(10.0), 30 * MB, Some(500)),
        ];
        for (case, now, bytes, owed_ms) in charges {
            let owed = bandwidth.debt(bytes, now);

            let owed = owed.map(|owed| owed.as_millis());
            assert_eq!(owed, owed_ms, "{case}: ms owed");
        }
    }
}
