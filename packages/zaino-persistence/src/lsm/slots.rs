//! Engine-wide merge slots: <= `LsmConfig::merge_slots` merges working at once across every set
//! of every index, sharing `LsmConfig::merge_mib_per_sec`; next free slot → lowest waiting tier
//!
//! - cap: else a merge per tier per set = dozens of threads competing with serving reads for disk
//! - bandwidth: merges never take the whole device (commits' fsyncs + folds' reads queue behind)
//! - lowest tier first: small merges (bound a commit's stall) never queue behind a large one
//! - slot holder waits only on its own I/O + the shared bandwidth → every waiting merge gets a slot

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
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
#[derive(Debug)]
struct Bandwidth {
    per_sec: f64,
    bucket: Mutex<(f64, Instant)>,
}

impl Bandwidth {
    fn new(bytes_per_sec: u64) -> Self {
        let per_sec = bytes_per_sec as f64;
        Self { per_sec, bucket: Mutex::new((per_sec, Instant::now())) }
    }

    fn charge(&self, bytes: usize) {
        let debt = {
            let mut bucket = self.bucket.lock().expect("bandwidth never held across a panic");
            let (tokens, at) = &mut *bucket;
            let now = Instant::now();
            let refill = now.duration_since(*at).as_secs_f64() * self.per_sec;
            *tokens = (*tokens + refill).min(self.per_sec) - bytes as f64;
            *at = now;
            (*tokens < 0.0).then(|| Duration::from_secs_f64(-*tokens / self.per_sec))
        };
        if let Some(debt) = debt {
            thread::sleep(debt);
        }
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
    pub(crate) fn new(capacity: usize, bytes_per_sec: u64) -> Self {
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

    /// Waits for a slot; `None` if `cancel` is raised first
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
            state = self.freed.wait_timeout(state, CANCEL_POLL).expect("slots lock").0;
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

    /// - never past capacity at once; freed slot → lowest waiting tier
    /// - cancelled waiter leaves the queue without a slot
    #[test]
    fn slots_cap_concurrency_and_serve_the_lowest_tier_first() {
        let slots: &'static Slots = Box::leak(Box::new(Slots::new(1, u64::MAX)));
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
        assert_eq!(slots.lock().running, 0);
    }

    /// One second's worth free, the rest paced: 30 MB at 20 MB/s ≈ 0.5 s, whoever charges it
    #[test]
    fn bandwidth_lets_one_seconds_burst_through_then_paces_every_charger_to_the_rate() {
        let bandwidth = Arc::new(Bandwidth::new(20_000_000));
        let started = Instant::now();
        let chargers: Vec<_> = (0..3)
            .map(|_| {
                let bandwidth = Arc::clone(&bandwidth);
                thread::spawn(move || (0..10).for_each(|_| bandwidth.charge(1_000_000)))
            })
            .collect();
        for charger in chargers {
            charger.join().expect("charger");
        }
        let took = started.elapsed();
        assert!(
            (0.4..1.5).contains(&took.as_secs_f64()),
            "30 MB at 20 MB/s, 20 MB burst: {took:?}"
        );
    }
}
