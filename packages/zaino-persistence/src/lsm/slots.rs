//! Process-wide merge slots: at most [`MERGE_SLOTS`] merges do work at once across every set of
//! every index, and a waiting merge gets the next free slot lowest tier first.
//!
//! Without a cap, each set could run a merge per tier at once, dozens of threads competing with
//! serving reads for the disk. Lowest tier first keeps the small merges that bound a commit's
//! stall from queueing behind a large one. A slot holder never waits on anything but its own
//! I/O, so a waiting merge always gets a slot eventually.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Condvar, Mutex, MutexGuard,
    },
    time::Duration,
};

/// Merges doing work at once, process-wide
pub(super) const MERGE_SLOTS: usize = 4;

/// How often a waiting merge rechecks its cancel flag
const CANCEL_POLL: Duration = Duration::from_millis(50);

pub(super) struct Slots {
    state: Mutex<State>,
    freed: Condvar,
    capacity: usize,
}

struct State {
    running: usize,
    /// `(tier, arrival)`: the lowest tier waiting goes first, ties in arrival order
    waiting: BinaryHeap<Reverse<(u32, u64)>>,
    arrivals: u64,
}

/// One held slot, returned on drop
pub(super) struct Slot<'a> {
    slots: &'a Slots,
}

pub(super) static SLOTS: Slots = Slots::new(MERGE_SLOTS);

impl Slots {
    const fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(State { running: 0, waiting: BinaryHeap::new(), arrivals: 0 }),
            freed: Condvar::new(),
            capacity,
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

    /// Never more than the capacity at once; a freed slot goes to the lowest tier waiting; a
    /// cancelled waiter leaves the queue without a slot
    #[test]
    fn slots_cap_concurrency_and_serve_the_lowest_tier_first() {
        let slots: &'static Slots = Box::leak(Box::new(Slots::new(1)));
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
}
