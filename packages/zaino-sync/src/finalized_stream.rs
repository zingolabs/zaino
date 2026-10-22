//! [`FinalizedStream<T>`]: one publisher, the same [`Step`]s to every subscriber, in order
//!
//! - One byte-bounded queue per subscriber (a slow one holds the publisher back, never grows
//!   memory)
//! - One `Arc<T>` per step, shared by every queue, freed at the last pop
//!
//! ```
//! use std::{num::NonZeroUsize, sync::Arc};
//! use zaino_sync::{FinalizedStream, Step, Weight};
//!
//! struct Blob;
//! impl Weight for Blob {
//!     fn weight(&self) -> usize { 100 }
//! }
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let budget = NonZeroUsize::new(1 << 20).expect("non-zero");
//! let mut sink = FinalizedStream::new();
//! let mut first = sink.subscribe("first", budget);
//! let mut second = sink.subscribe("second", budget);
//!
//! sink.send(Step::Apply(Arc::new(Blob))).await;
//! sink.shutdown();
//!
//! for queue in [&mut first, &mut second] {
//!     assert!(matches!(queue.next().await, Step::Apply(_)));
//!     assert!(matches!(queue.next().await, Step::Shutdown));
//! }
//! # }
//! ```

use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use zaino_primitives::types::{Block, BlockFees};

/// Bytes an item holds in memory, heap included: the unit a subscriber's queue budget counts
pub trait Weight {
    fn weight(&self) -> usize;
}

impl Weight for Block {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

impl Weight for BlockFees {
    fn weight(&self) -> usize {
        self.footprint()
    }
}

/// Per-block transaction fees: utxo-set → compact-block, one per finalized step
pub type FeeSink = FinalizedStream<BlockFees>;

/// One step, the same for every subscriber
///
/// - `FinalizedTip` = the block at the chain's finalized tip (apply, then commit: caught up)
/// - `Shutdown` last: commit what is held, stop
#[derive(Debug)]
pub enum Step<T> {
    Apply(Arc<T>),
    FinalizedTip(Arc<T>),
    Shutdown,
}

impl<T: Weight> Weight for Step<T> {
    fn weight(&self) -> usize {
        size_of::<Self>()
            + match self {
                Self::Apply(data) | Self::FinalizedTip(data) => data.weight(),
                Self::Shutdown => 0,
            }
    }
}

impl<T> Clone for Step<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Apply(data) => Self::Apply(Arc::clone(data)),
            Self::FinalizedTip(data) => Self::FinalizedTip(Arc::clone(data)),
            Self::Shutdown => Self::Shutdown,
        }
    }
}

/// Step + its share of the queue's budget, held until popped (`None` = `Shutdown`: budget
/// bypassed, a full queue never holds back the stop)
pub(crate) struct Queued<T> {
    step: Step<T>,
    _held: Option<OwnedSemaphorePermit>,
}

/// One batch of work off a [`Subscription`], in height order
///
/// - `ends_at_finalized_tip` = its last block = `Step::FinalizedTip` (the chain's finalized tip)
pub struct Run<T> {
    pub blocks: Vec<Arc<T>>,
    pub ends_at_finalized_tip: bool,
}

/// One consumer's end: its queue, in stream order
pub struct Subscription<T> {
    rx: mpsc::UnboundedReceiver<Queued<T>>,
    budget: NonZeroUsize,
    shut_down: bool,
}

impl<T> Subscription<T> {
    /// Next step, its bytes returned to the budget (`Shutdown` again on every call after it)
    pub async fn next(&mut self) -> Step<T> {
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
            Some(Queued { step, .. }) => {
                self.shut_down |= matches!(step, Step::Shutdown);
                step
            }
            None if self.shut_down => Step::Shutdown,
            None => panic!("sink dropped without Shutdown"),
        }
    }
}

impl<T: Weight> Subscription<T> {
    /// Next step + every `Apply` already queued behind it, to the queue's budget (one batch of
    /// work, no wait past the first step)
    ///
    /// - `None` = `Shutdown`
    /// - `FinalizedTip` block ends the run, included
    pub async fn next_run(&mut self) -> Option<Run<T>> {
        let first = self.next().await;
        let run = self.run(first, self.budget);
        (!run.blocks.is_empty()).then_some(run)
    }

    /// `first` + queued `Apply`s to `budget` bytes (`Shutdown` ends it, next `next` = it again)
    fn run(&mut self, first: Step<T>, budget: NonZeroUsize) -> Run<T> {
        let mut run = Run { blocks: Vec::new(), ends_at_finalized_tip: false };
        let mut bytes = 0usize;
        let mut step = Some(first);
        while let Some(next) = step.take() {
            match next {
                Step::Apply(data) => {
                    bytes = bytes.saturating_add(data.weight());
                    run.blocks.push(data);
                }
                Step::FinalizedTip(data) => {
                    run.blocks.push(data);
                    run.ends_at_finalized_tip = true;
                    break;
                }
                Step::Shutdown => break,
            }
            if bytes < budget.get() {
                step = self.try_next();
            }
        }
        run
    }
}

/// Budget = semaphore permits (1 per byte) over an unbounded channel (byte-bounded queue tokio
/// lacks)
///
/// - `capacity` = largest acquire (a step heavier than the budget waits for empty)
struct Subscriber<T> {
    name: &'static str,
    tx: mpsc::UnboundedSender<Queued<T>>,
    budget: Arc<Semaphore>,
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
pub struct FinalizedStream<T> {
    subscribers: Vec<Subscriber<T>>,
}

impl<T> Default for FinalizedStream<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FinalizedStream<T> {
    pub fn new() -> Self {
        Self { subscribers: Vec::new() }
    }

    /// `budget` = bytes the queue may hold ([`Weight`]), and one [`Run`]'s
    pub fn subscribe(&mut self, name: &'static str, budget: NonZeroUsize) -> Subscription<T> {
        let (tx, rx) = mpsc::unbounded_channel();
        let permits = budget.get().min(Semaphore::MAX_PERMITS);
        self.subscribers.push(Subscriber {
            name,
            tx,
            budget: Arc::new(Semaphore::new(permits)),
            capacity: u32::try_from(permits).unwrap_or(u32::MAX),
        });
        Subscription { rx, budget, shut_down: false }
    }

    /// `Shutdown` last in every queue, never waits (budget bypassed)
    pub fn shutdown(self) {
        for subscriber in &self.subscribers {
            subscriber.push(Queued { step: Step::Shutdown, _held: None });
        }
    }
}

impl<T: Weight> FinalizedStream<T> {
    /// `step` to every queue or to none (a full queue delays it: what bounds memory)
    ///
    /// - every queue's share held before any push: a send cancelled mid-wait delivers nowhere
    ///   (subscribers never part ways at a cancelled publisher's last step)
    pub async fn send(&self, step: Step<T>) {
        assert!(!matches!(step, Step::Shutdown), "Shutdown ends the sink: `shutdown`, not `send`");
        let weight = u32::try_from(step.weight());
        let mut held = Vec::with_capacity(self.subscribers.len());
        for subscriber in &self.subscribers {
            let permits =
                weight.map_or(subscriber.capacity, |weight| weight.min(subscriber.capacity));
            let budget = Arc::clone(&subscriber.budget).acquire_many_owned(permits).await;
            held.push(budget.expect("queue budget never closed"));
        }
        for (subscriber, held) in self.subscribers.iter().zip(held) {
            subscriber.push(Queued { step: step.clone(), _held: Some(held) });
        }
    }
}

#[cfg(test)]
mod tests {
    use zaino_source::testing::h;
    use zcash_protocol::consensus::BlockHeight;

    use super::*;

    #[derive(Debug)]
    struct Blob {
        height: BlockHeight,
        bytes: usize,
    }

    impl Weight for Blob {
        fn weight(&self) -> usize {
            self.bytes
        }
    }

    fn blob(height: u32, bytes: usize) -> Arc<Blob> {
        Arc::new(Blob { height: h(height), bytes })
    }

    fn apply(height: u32, bytes: usize) -> Step<Blob> {
        Step::Apply(blob(height, bytes))
    }

    fn popped(step: Step<Blob>) -> BlockHeight {
        match step {
            Step::Apply(blob) => blob.height,
            other => panic!("expected an Apply, popped {other:?}"),
        }
    }

    fn assert_shutdown(step: Step<Blob>, why: &str) {
        assert!(matches!(step, Step::Shutdown), "{why}: popped {step:?}, expected Shutdown");
    }

    /// - two subscribers see the same steps in the same order
    /// - send cancelled while `two` full → reaches neither
    /// - `Shutdown` lands last and stays popped
    #[tokio::test]
    async fn every_subscriber_sees_the_same_steps_a_cancelled_send_none_then_shutdown_forever() {
        let mut sink = FinalizedStream::<Blob>::new();
        let (roomy, tight) = (1 << 20, size_of::<Step<Blob>>() + 1);
        let nz = |bytes| NonZeroUsize::new(bytes).expect("nz");
        let (mut one, mut two) =
            (sink.subscribe("one", nz(roomy)), sink.subscribe("two", nz(tight)));
        sink.send(apply(7, 1)).await;
        {
            let blocked = sink.send(apply(99, 1));
            tokio::pin!(blocked);
            assert!(futures::poll!(blocked.as_mut()).is_pending(), "two's queue full");
        }
        assert_eq!(popped(two.next().await), h(7), "the cancelled 99 never queued for two");
        sink.send(apply(8, 1)).await;
        assert_eq!(popped(two.next().await), h(8), "two: next send after the cancelled one");
        sink.shutdown();
        assert_eq!(popped(one.next().await), h(7), "one: first send");
        assert_eq!(popped(one.next().await), h(8), "one: the cancelled 99 never queued either");
        for (name, sub) in [("one", &mut one), ("two", &mut two)] {
            assert_shutdown(sub.next().await, &format!("{name}: after its last step"));
            assert_shutdown(sub.next().await, &format!("{name}: closed queue past Shutdown"));
        }
    }

    /// Run = first block + `Apply`s already queued, cut at its budget, by a `FinalizedTip` block
    /// (included, `true`) or by `Shutdown` (the next one out)
    #[tokio::test]
    async fn a_run_gathers_queued_applies_to_its_budget_ends_at_a_finalized_block_or_shutdown() {
        let mut sink = FinalizedStream::<Blob>::new();
        let mut sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        for height in 0..5 {
            sink.send(apply(height, 10)).await;
        }
        sink.send(Step::FinalizedTip(blob(5, 10))).await;
        sink.send(Step::FinalizedTip(blob(6, 10))).await;
        sink.send(apply(7, 10)).await;
        sink.shutdown();
        #[derive(Debug, PartialEq)]
        struct Gathered {
            heights: Vec<u32>,
            ends_at_finalized_tip: bool,
        }
        let gathered = |run: Run<Blob>| Gathered {
            heights: run.blocks.into_iter().map(|blob| u32::from(blob.height)).collect(),
            ends_at_finalized_tip: run.ends_at_finalized_tip,
        };
        let (twenty, all) = (NonZeroUsize::new(20).expect("nz"), NonZeroUsize::MAX);

        let start = sub.next().await;
        let expected = Gathered { heights: vec![0, 1], ends_at_finalized_tip: false };
        assert_eq!(gathered(sub.run(start, twenty)), expected, "cut at 20 bytes");
        let start = sub.next().await;
        let expected = Gathered { heights: vec![2, 3, 4, 5], ends_at_finalized_tip: true };
        assert_eq!(gathered(sub.run(start, all)), expected, "ends at FinalizedTip 5");
        let start = sub.next().await;
        let expected = Gathered { heights: vec![6], ends_at_finalized_tip: true };
        assert_eq!(gathered(sub.run(start, all)), expected, "a FinalizedTip first = alone");
        let start = sub.next().await;
        let expected = Gathered { heights: vec![7], ends_at_finalized_tip: false };
        assert_eq!(gathered(sub.run(start, all)), expected, "cut by Shutdown");
        assert_shutdown(sub.next().await, "Shutdown ended the last run");
        assert_shutdown(sub.next().await, "closed queue past Shutdown");
    }

    /// Queue held through `Shutdown` (dropped sooner = bug upstream)
    #[tokio::test]
    #[should_panic(expected = "subscriber one dropped its queue before Shutdown")]
    async fn a_queue_dropped_before_shutdown_panics_the_sink() {
        let mut sink = FinalizedStream::<Blob>::new();
        drop(sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz")));
        sink.send(apply(0, 1)).await;
    }

    /// Sink dropped without `Shutdown` (its publisher panicked) = no clean stop
    #[tokio::test]
    #[should_panic(expected = "sink dropped without Shutdown")]
    async fn a_sink_dropped_without_shutdown_panics_the_subscriber() {
        let mut sink = FinalizedStream::<Blob>::new();
        let mut sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        drop(sink);
        sub.next().await;
    }

    /// Budget = three 100-byte steps
    ///
    /// - fourth waits for a pop
    /// - step over the whole budget passes alone once the queue drains
    /// - `Shutdown` queues behind a full budget, never waits
    #[tokio::test]
    async fn a_byte_budget_bounds_the_queue_and_never_holds_back_shutdown() {
        let step = size_of::<Step<Blob>>() + 100;
        let oversize = 10 * 3 * step;
        let mut sink = FinalizedStream::<Blob>::new();
        let mut sub = sink.subscribe("one", NonZeroUsize::new(3 * step).expect("nz"));

        for height in 0..3 {
            sink.send(apply(height, 100)).await;
        }
        {
            let fourth = sink.send(apply(3, 100));
            tokio::pin!(fourth);
            assert!(futures::poll!(fourth.as_mut()).is_pending(), "budget full: 3 waits");
            assert_eq!(popped(sub.next().await), h(0), "first of the full budget");
            fourth.await;
        }

        for height in 1..=3 {
            assert_eq!(popped(sub.next().await), h(height), "in send order");
        }
        sink.send(apply(4, oversize)).await;
        {
            let small = sink.send(apply(5, 100));
            tokio::pin!(small);
            assert!(futures::poll!(small.as_mut()).is_pending(), "oversize 4 holds all: 5 waits");
            assert_eq!(popped(sub.next().await), h(4), "oversize 4 passed alone");
            small.await;
        }

        {
            let heavy = sink.send(apply(6, oversize));
            tokio::pin!(heavy);
            assert!(futures::poll!(heavy.as_mut()).is_pending(), "oversize 6 waits for 5 to pop");
            assert_eq!(popped(sub.next().await), h(5), "5 before oversize 6");
            heavy.await;
        }
        sink.shutdown();
        assert_eq!(popped(sub.next().await), h(6), "oversize 6 once the queue drained");
        assert_shutdown(sub.next().await, "queued past the full budget");
    }
}
