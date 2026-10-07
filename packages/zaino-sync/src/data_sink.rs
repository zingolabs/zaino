//! [`IndexerDataSink<T>`]: one publisher, the same [`Step`]s to every subscriber, in order
//!
//! - One byte-bounded queue per subscriber (a slow one holds the publisher back, never grows
//!   memory)
//! - One `Arc<T>` per step, shared by every queue, freed at the last pop
//! - Design: `docs/design/data-sink.md`
//!
//! ```
//! use std::{num::NonZeroUsize, sync::Arc};
//! use zaino_primitives::types::Height;
//! use zaino_sync::{IndexerDataSink, Step, Weight};
//!
//! struct Blob;
//! impl Weight for Blob {
//!     fn weight(&self) -> usize { 100 }
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let budget = NonZeroUsize::new(1 << 20).unwrap();
//! let mut sink = IndexerDataSink::new("blobs");
//! let mut first = sink.subscribe("first", budget);
//! let mut second = sink.subscribe("second", budget);
//!
//! let height = Height::try_from(7).unwrap();
//! sink.send(Step::Apply { height, data: Arc::new(Blob) }).await;
//! sink.shutdown();
//!
//! for queue in [&mut first, &mut second] {
//!     assert!(matches!(queue.next().await, Step::Apply { .. }));
//!     assert!(matches!(queue.next().await, Step::Shutdown));
//! }
//! # }
//! ```

use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use zaino_primitives::types::Height;

use crate::emit::QueueBytes;

/// Bytes an item holds in memory, heap included: the unit a subscriber's queue budget counts
pub trait Weight {
    fn weight(&self) -> usize;
}

/// One step, the same for every subscriber (`Shutdown` last: commit what is held, stop)
#[derive(Debug)]
pub enum Step<T> {
    Apply { height: Height, data: Arc<T> },
    Shutdown,
}

impl<T: Weight> Weight for Step<T> {
    fn weight(&self) -> usize {
        size_of::<Self>()
            + match self {
                Self::Apply { data, .. } => data.weight(),
                Self::Shutdown => 0,
            }
    }
}

impl<T> Clone for Step<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Apply { height, data } => Self::Apply { height: *height, data: Arc::clone(data) },
            Self::Shutdown => Self::Shutdown,
        }
    }
}

/// A step and the share of its queue's budget it holds until popped (`None` = `Shutdown`: budget
/// bypassed, a full queue never holds back the stop)
pub(crate) struct Queued<T> {
    step: Step<T>,
    _held: Option<OwnedSemaphorePermit>,
}

/// One delivered block of a [`Subscription::run`]
pub type Applied<T> = (Height, Arc<T>);

/// One consumer's end: its queue, in stream order
///
/// - `ended_run` = the step that ended the last [`run`](Self::run): the next one out
pub struct Subscription<T> {
    rx: mpsc::UnboundedReceiver<Queued<T>>,
    queued: QueueBytes,
    shut_down: bool,
    ended_run: Option<Step<T>>,
}

impl<T> Subscription<T> {
    /// Next step, its bytes returned to the budget; `Shutdown` again on every call after it
    pub async fn next(&mut self) -> Step<T> {
        if let Some(step) = self.ended_run.take() {
            return step;
        }
        let queued = self.rx.recv().await;
        self.popped(queued)
    }

    /// Next step if one is already queued (never waits)
    fn try_next(&mut self) -> Option<Step<T>> {
        let queued = self.rx.try_recv().ok()?;
        Some(self.popped(Some(queued)))
    }

    /// - Closed after `Shutdown` = sink consumed by it
    /// - Closed before = sink dropped (its publisher panicked)
    fn popped(&mut self, queued: Option<Queued<T>>) -> Step<T> {
        match queued {
            Some(Queued { step, _held }) => {
                if let Some(held) = _held {
                    self.queued.popped(held.num_permits());
                }
                self.shut_down |= matches!(step, Step::Shutdown);
                step
            }
            None if self.shut_down => Step::Shutdown,
            None => panic!("sink dropped without Shutdown"),
        }
    }
}

impl<T: Weight> Subscription<T> {
    /// `first` + every `Apply` already queued behind it, to `budget` bytes (one batch of work,
    /// never a wait)
    ///
    /// - step ending the run = the next [`next`](Self::next)'s
    pub fn run(&mut self, first: Applied<T>, budget: NonZeroUsize) -> Vec<Applied<T>> {
        let mut bytes = first.1.weight();
        let mut run = vec![first];
        while bytes < budget.get() {
            match self.try_next() {
                Some(Step::Apply { height, data }) => {
                    bytes = bytes.saturating_add(data.weight());
                    run.push((height, data));
                }
                other => {
                    self.ended_run = other;
                    break;
                }
            }
        }
        run
    }
}

/// Budget = semaphore permits (1 per byte) over an unbounded channel: the byte-bounded queue
/// tokio lacks
struct Subscriber<T> {
    name: &'static str,
    tx: mpsc::UnboundedSender<Queued<T>>,
    queued: QueueBytes,
    budget: Arc<Semaphore>,
    /// Largest acquire: a step heavier than the whole budget waits for an empty queue
    capacity: u32,
}

impl<T> Subscriber<T> {
    fn push(&self, queued: Queued<T>) {
        if self.tx.send(queued).is_err() {
            panic!("subscriber {} dropped its queue before Shutdown", self.name);
        }
    }
}

/// Per-block data `T`, published once and consumed by every subscriber
///
/// - Subscribed before it is handed to its publisher (taken by value: no subscriber joins
///   mid-stream)
/// - Ends with [`shutdown`](Self::shutdown) (dropped without it = subscribers panic)
pub struct IndexerDataSink<T> {
    name: &'static str,
    subscribers: Vec<Subscriber<T>>,
}

impl<T> IndexerDataSink<T> {
    /// `name` = its `sink` label on `zaino.sink.queue_bytes`
    pub fn new(name: &'static str) -> Self {
        Self { name, subscribers: Vec::new() }
    }

    /// `budget` = bytes the queue may hold ([`Weight`])
    pub fn subscribe(&mut self, name: &'static str, budget: NonZeroUsize) -> Subscription<T> {
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = QueueBytes::new(self.name, name);
        let budget = budget.get().min(Semaphore::MAX_PERMITS);
        self.subscribers.push(Subscriber {
            name,
            tx,
            queued: queued.clone(),
            budget: Arc::new(Semaphore::new(budget)),
            capacity: u32::try_from(budget).unwrap_or(u32::MAX),
        });
        Subscription { rx, queued, shut_down: false, ended_run: None }
    }

    /// `Shutdown` last in every queue; never waits (budget bypassed)
    pub fn shutdown(self) {
        for subscriber in &self.subscribers {
            subscriber.push(Queued { step: Step::Shutdown, _held: None });
        }
    }
}

impl<T: Weight> IndexerDataSink<T> {
    /// `step` to every queue, serially (a full queue delays the rest: what bounds memory)
    pub async fn send(&self, step: Step<T>) {
        assert!(!matches!(step, Step::Shutdown), "Shutdown ends the sink: `shutdown`, not `send`");
        for subscriber in &self.subscribers {
            let step = step.clone();
            let permits = u32::try_from(step.weight())
                .map_or(subscriber.capacity, |weight| weight.min(subscriber.capacity));
            let held = Arc::clone(&subscriber.budget)
                .acquire_many_owned(permits)
                .await
                .expect("queue budget never closed");
            subscriber.queued.pushed(permits);
            subscriber.push(Queued { step, _held: Some(held) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    struct Blob(usize);

    impl Weight for Blob {
        fn weight(&self) -> usize {
            self.0
        }
    }

    fn apply(height: u32, weight: usize) -> Step<Blob> {
        Step::Apply { height: h(height), data: Arc::new(Blob(weight)) }
    }

    fn popped(step: Step<Blob>) -> Height {
        match step {
            Step::Apply { height, .. } => height,
            Step::Shutdown => panic!("expected an apply"),
        }
    }

    /// Two subscribers see the same steps in the same order; `Shutdown` lands last and stays popped
    #[tokio::test]
    async fn every_subscriber_sees_the_same_steps_then_shutdown_forever() {
        let mut sink = IndexerDataSink::<Blob>::new("test");
        let queue = NonZeroUsize::new(1 << 20).expect("nz");
        let (mut one, mut two) = (sink.subscribe("one", queue), sink.subscribe("two", queue));
        sink.send(apply(7, 1)).await;
        sink.send(apply(8, 1)).await;
        sink.shutdown();

        for sub in [&mut one, &mut two] {
            assert_eq!(popped(sub.next().await), h(7));
            assert_eq!(popped(sub.next().await), h(8));
            assert!(matches!(sub.next().await, Step::Shutdown));
            assert!(matches!(sub.next().await, Step::Shutdown), "closed queue past Shutdown");
        }
    }

    /// A run = the first `Apply` + those already queued, cut at its budget or at `Shutdown`; the
    /// step ending it is the next one out
    #[tokio::test]
    async fn a_run_gathers_queued_applies_to_its_budget_and_hands_back_the_step_ending_it() {
        let mut sink = IndexerDataSink::<Blob>::new("test");
        let mut sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        for height in 0..5 {
            sink.send(apply(height, 10)).await;
        }
        sink.shutdown();
        let first = |step: Step<Blob>| match step {
            Step::Apply { height, data } => (height, data),
            Step::Shutdown => panic!("expected an apply"),
        };
        let heights = |run: Vec<Applied<Blob>>| -> Vec<u32> {
            run.into_iter().map(|(height, _)| u32::from(height)).collect()
        };
        let (twenty, all) = (NonZeroUsize::new(20).expect("nz"), NonZeroUsize::MAX);

        let start = first(sub.next().await);
        assert_eq!(heights(sub.run(start, twenty)), [0, 1], "cut at its 20-byte budget");
        let start = first(sub.next().await);
        assert_eq!(heights(sub.run(start, all)), [2, 3, 4], "cut by Shutdown");
        assert!(matches!(sub.next().await, Step::Shutdown), "Shutdown ended the last run");
        assert!(matches!(sub.next().await, Step::Shutdown), "and stays");
    }

    /// A subscriber holds its queue through `Shutdown`: dropping it sooner is a bug upstream
    #[tokio::test]
    #[should_panic(expected = "subscriber one dropped its queue before Shutdown")]
    async fn a_queue_dropped_before_shutdown_panics_the_sink() {
        let mut sink = IndexerDataSink::<Blob>::new("test");
        drop(sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz")));
        sink.send(apply(0, 1)).await;
    }

    /// A sink ends with `Shutdown`: one dropped without it (its publisher panicked) is no clean stop
    #[tokio::test]
    #[should_panic(expected = "sink dropped without Shutdown")]
    async fn a_sink_dropped_without_shutdown_panics_the_subscriber() {
        let mut sink = IndexerDataSink::<Blob>::new("test");
        let mut sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        drop(sink);
        sub.next().await;
    }

    /// `zaino.sink.queue_bytes` = bytes each queue holds, per (sink, subscriber):
    /// - up on send, down on pop, back to 0 once drained
    /// - a step over the budget counts as the budget (what it holds); `Shutdown` counts nothing
    /// - same subscriber name on two sinks = two gauges (compact-block reads blocks and fees)
    #[tokio::test]
    async fn queue_bytes_tracks_what_each_queue_holds_per_sink_and_subscriber() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let step = size_of::<Step<Blob>>() + 100;
        let budget = NonZeroUsize::new(3 * step).expect("nz");
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let (mut blocks, mut fees) = (IndexerDataSink::new("blocks"), IndexerDataSink::new("fees"));
        // gauge handles bind to the recorder at subscribe
        let (mut one, mut two, mut fees_one) = metrics::with_local_recorder(&recorder, || {
            let one = blocks.subscribe("one", budget);
            (one, blocks.subscribe("two", budget), fees.subscribe("one", budget))
        });
        // snapshot swaps each gauge to 0 → running sum = current value (only ever += / −=)
        let totals = std::cell::RefCell::new(std::collections::BTreeMap::new());
        let queued = || -> Vec<(String, String, f64)> {
            let mut totals = totals.borrow_mut();
            for (key, _, _, value) in snapshotter.snapshot().into_vec() {
                let DebugValue::Gauge(bytes) = value else { continue };
                let label = |name: &str| -> String {
                    let mut labels = key.key().labels();
                    labels.find(|label| label.key() == name).expect(name).value().into()
                };
                *totals.entry((label("sink"), label("subscriber"))).or_insert(0.0) += bytes.0;
            }
            totals.iter().map(|((sink, sub), bytes)| (sink.clone(), sub.clone(), *bytes)).collect()
        };
        let row = |sink: &str, subscriber: &str, bytes: usize| {
            (sink.to_owned(), subscriber.to_owned(), bytes as f64)
        };

        blocks.send(apply(0, 100)).await;
        blocks.send(apply(1, 100)).await;
        fees.send(apply(0, 100)).await;
        assert_eq!(popped(one.next().await), h(0));
        assert_eq!(
            queued(),
            [row("blocks", "one", step), row("blocks", "two", 2 * step), row("fees", "one", step)]
        );

        // drain: one holds 1, two holds 2, fees one holds 1
        assert_eq!(popped(one.next().await), h(1));
        assert_eq!((popped(two.next().await), popped(two.next().await)), (h(0), h(1)));
        assert_eq!(popped(fees_one.next().await), h(0));
        blocks.send(apply(2, 10 * budget.get())).await;
        assert_eq!(
            queued(),
            [row("blocks", "one", 3 * step), row("blocks", "two", 3 * step), row("fees", "one", 0)],
            "an oversize step holds the whole budget"
        );

        blocks.shutdown();
        fees.shutdown();
        assert_eq!(popped(one.next().await), h(2));
        assert_eq!(popped(two.next().await), h(2));
        for sub in [&mut one, &mut two, &mut fees_one] {
            assert!(matches!(sub.next().await, Step::Shutdown), "Shutdown last");
        }
        assert_eq!(
            queued(),
            [row("blocks", "one", 0), row("blocks", "two", 0), row("fees", "one", 0)],
            "drained through Shutdown"
        );
    }

    /// Budget = three 100-byte steps: a fourth waits for a pop; a step over the whole budget
    /// passes alone once the queue drains; `Shutdown` queues behind a full budget, never waits
    #[tokio::test]
    async fn a_byte_budget_bounds_the_queue_and_never_holds_back_shutdown() {
        let step = size_of::<Step<Blob>>() + 100;
        let oversize = 10 * 3 * step;
        let mut sink = IndexerDataSink::<Blob>::new("test");
        let mut sub = sink.subscribe("one", NonZeroUsize::new(3 * step).expect("nz"));

        for height in 0..3 {
            sink.send(apply(height, 100)).await;
        }
        {
            let fourth = sink.send(apply(3, 100));
            tokio::pin!(fourth);
            assert!(futures::poll!(fourth.as_mut()).is_pending(), "budget full");
            assert_eq!(popped(sub.next().await), h(0));
            fourth.await;
        }

        for height in 1..=3 {
            assert_eq!(popped(sub.next().await), h(height));
        }
        sink.send(apply(4, oversize)).await;
        {
            let small = sink.send(apply(5, 100));
            tokio::pin!(small);
            assert!(futures::poll!(small.as_mut()).is_pending(), "oversize holds all");
            assert_eq!(popped(sub.next().await), h(4));
            small.await;
        }

        {
            let heavy = sink.send(apply(6, oversize));
            tokio::pin!(heavy);
            assert!(futures::poll!(heavy.as_mut()).is_pending(), "waits for an empty queue");
            assert_eq!(popped(sub.next().await), h(5));
            heavy.await;
        }
        sink.shutdown();
        assert_eq!(popped(sub.next().await), h(6));
        assert!(matches!(sub.next().await, Step::Shutdown), "queued past the full budget");
    }
}
