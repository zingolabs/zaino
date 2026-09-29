//! `IndexerDataSink<T>`: one producer, N subscribers, one byte-bounded queue each, keyed by
//! block height
//!
//! - `add(height, T)` shares one `Arc<T>` across every queue (freed when the last subscriber pops)
//! - One resume point: every subscriber is fed from the rearmost durable extent (an index ahead
//!   skips what it already holds: `IndexWriter::deliver`)
//! - Backpressure = the slowest subscriber's byte budget ([`Weight`]; a budget, not a depth, so
//!   slack holds steady from 1 KB to 2 MB blocks)
//! - Finality decided here (`ReorgDepth` below the highest tip), in-band: an `Apply` says whether
//!   it is final, and a `Finalized` follows each non-final one once the tip buries it
//! - Reorg = [`Step::Reset`] in the same queue as the data (cannot overtake or trail an apply)
//! - Two phases: [`SinkBuilder`] takes subscriptions, [`SinkBuilder::seal`] fixes the start

use std::{num::NonZeroUsize, sync::Arc};

use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use zaino_primitives::types::{Extent, Height, ReorgDepth};

/// Bytes an item holds in memory, heap included: the unit a subscriber's queue budget counts
pub trait Weight {
    fn weight(&self) -> usize;
}

/// One instruction for a subscriber, in stream order
#[derive(Debug)]
pub enum Step<T> {
    /// Block `height`'s data; `finalized` = below the reorg bound (durable-bound, skips pre-commit)
    Apply { height: Height, finalized: bool, data: Arc<T> },
    /// An earlier non-final `Apply` at `height` is now below the reorg bound (oldest first)
    Finalized { height: Height },
    /// A branch won: drop **all** pre-commit state, then expect applies from the durable tip
    ///
    /// - No fork height (no reverse fold to disagree about; the reorg bound keeps every fork above
    ///   the durable tip)
    Reset,
}

impl<T: Weight> Weight for Step<T> {
    fn weight(&self) -> usize {
        size_of::<Self>()
            + match self {
                Self::Apply { data, .. } => data.weight(),
                Self::Finalized { .. } | Self::Reset => 0,
            }
    }
}

impl<T> Clone for Step<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Apply { height, finalized, data } => {
                Self::Apply { height: *height, finalized: *finalized, data: Arc::clone(data) }
            }
            Self::Finalized { height } => Self::Finalized { height: *height },
            Self::Reset => Self::Reset,
        }
    }
}

/// A subscriber's receiver went away, so the sink can no longer feed every consumer
#[derive(Debug, thiserror::Error)]
#[error("subscriber {subscriber} stopped receiving")]
pub struct SinkGone {
    pub subscriber: &'static str,
}

/// A step and the share of its queue's budget it holds until popped
pub(crate) struct Queued<T> {
    step: Step<T>,
    _held: OwnedSemaphorePermit,
}

/// One consumer's end: its queue and the tip (serving gate)
pub struct Subscription<T> {
    pub(crate) steps: mpsc::UnboundedReceiver<Queued<T>>,
    pub(crate) tip: watch::Receiver<Option<Height>>,
    pub(crate) depth: ReorgDepth,
}

impl<T> Subscription<T> {
    /// Next step, its bytes returned to the budget; `None` = producer dropped (drain + stop)
    pub async fn next(&mut self) -> Option<Step<T>> {
        self.steps.recv().await.map(|queued| queued.step)
    }

    /// Next step if one is already queued (never waits)
    pub(crate) fn try_next(&mut self) -> Option<Step<T>> {
        self.steps.try_recv().ok().map(|queued| queued.step)
    }

    /// Next step, or [`Woke::Tip`] when the tip moves with none queued (a lowered tip sends no
    /// step); `None` = producer dropped
    pub(crate) async fn next_or_tip(&mut self) -> Option<Woke<T>> {
        tokio::select! {
            biased;
            step = self.steps.recv() => step.map(|queued| Woke::Step(queued.step)),
            Ok(()) = self.tip.changed() => Some(Woke::Tip),
        }
    }
}

/// What ended a subscriber's wait
pub(crate) enum Woke<T> {
    Step(Step<T>),
    Tip,
}

/// Budget = semaphore permits (1 per byte) over an unbounded channel: the byte-bounded queue
/// tokio lacks
///
/// - dropped receiver → channel drops its queue → permits freed → `send` fails, never hangs
struct Subscriber<T> {
    name: &'static str,
    tx: mpsc::UnboundedSender<Queued<T>>,
    budget: Arc<Semaphore>,
    /// Largest acquire: a step heavier than the whole budget waits for an empty queue
    capacity: u32,
}

impl<T: Weight> Subscriber<T> {
    async fn send(&self, step: Step<T>) -> Result<(), SinkGone> {
        let permits = u32::try_from(step.weight()).map_or(self.capacity, |w| w.min(self.capacity));
        let held = Arc::clone(&self.budget)
            .acquire_many_owned(permits)
            .await
            .expect("queue budget never closed");
        self.tx.send(Queued { step, _held: held }).map_err(|_| SinkGone { subscriber: self.name })
    }
}

/// Subscriptions before the first block (a subscriber joining mid-stream would see a gap)
pub struct SinkBuilder<T> {
    subscribers: Vec<Subscriber<T>>,
    tip: watch::Sender<Option<Height>>,
    depth: ReorgDepth,
    /// Rearmost durable extent subscribed so far
    start: Option<Extent>,
    furthest_durable: Extent,
}

impl<T> SinkBuilder<T> {
    pub fn new(depth: ReorgDepth) -> Self {
        Self {
            subscribers: Vec::new(),
            tip: watch::Sender::new(None),
            depth,
            start: None,
            furthest_durable: Extent::ZERO,
        }
    }

    /// `durable` = the subscriber's durable extent (production starts at the rearmost one);
    /// `budget` = bytes its queue may hold ([`Weight`])
    pub fn subscribe(
        &mut self,
        name: &'static str,
        budget: NonZeroUsize,
        durable: Extent,
    ) -> Subscription<T> {
        let (tx, steps) = mpsc::unbounded_channel();
        let budget = budget.get().min(Semaphore::MAX_PERMITS);
        self.subscribers.push(Subscriber {
            name,
            tx,
            budget: Arc::new(Semaphore::new(budget)),
            capacity: u32::try_from(budget).unwrap_or(u32::MAX),
        });
        self.start = Some(self.start.map_or(durable, |start| start.min(durable)));
        self.furthest_durable = self.furthest_durable.max(durable);
        Subscription { steps, tip: self.tip.subscribe(), depth: self.depth }
    }

    /// Final from the start through the furthest durable extent (durable = was final under some
    /// earlier tip, so a lower tip after a restart cannot un-finalise what an index holds)
    pub fn seal(self) -> IndexerDataSink<T> {
        let start = self.start.expect("sink sealed with no subscriber");
        IndexerDataSink {
            subscribers: self.subscribers,
            tip: self.tip,
            depth: self.depth,
            final_extent: self.furthest_durable,
            announced: start,
            next: start,
        }
    }
}

/// Per-block data `T`, produced once and consumed by every subscriber
///
/// - Dropping it closes every queue (how subscribers are told to drain and stop)
pub struct IndexerDataSink<T> {
    subscribers: Vec<Subscriber<T>>,
    tip: watch::Sender<Option<Height>>,
    depth: ReorgDepth,
    /// Final under the highest tip seen (monotone: a lower winning tip never un-finalises)
    final_extent: Extent,
    /// Delivered and announced final (`== next` = none pending)
    announced: Extent,
    /// Delivered so far: the next `add` is `next.next()`
    next: Extent,
}

impl<T: Weight> IndexerDataSink<T> {
    /// Next height `add` accepts
    pub fn next(&self) -> Height {
        self.next.next()
    }

    pub fn depth(&self) -> ReorgDepth {
        self.depth
    }

    pub fn final_extent(&self) -> Extent {
        self.final_extent
    }

    /// New tip → a `Finalized` for every delivered non-final height it buries
    ///
    /// - A lower tip (shorter winning branch) keeps the boundary where it was
    pub async fn set_tip(&mut self, tip: Height) -> Result<(), SinkGone> {
        self.tip.send_replace(Some(tip));
        self.finalize_through(self.depth.final_extent(tip)).await
    }

    /// Everything inside `extent` final, without a tip (a producer that learns finality from its
    /// own upstream); monotone like [`set_tip`](Self::set_tip)
    pub async fn finalize_through(&mut self, extent: Extent) -> Result<(), SinkGone> {
        self.final_extent = self.final_extent.max(extent);
        while self.announced < self.final_extent.min(self.next) {
            let height = self.announced.next();
            self.broadcast(Step::Finalized { height }).await?;
            self.announced = Extent::through(height);
        }
        Ok(())
    }

    /// Hands block `height`'s data to every subscriber
    ///
    /// - Serial: a full queue delays the rest (what bounds memory)
    /// - Producer contiguity asserted: `next()`, then +1 each, rewound by reset
    pub async fn add(&mut self, height: Height, data: Arc<T>) -> Result<(), SinkGone> {
        assert_eq!(height, self.next(), "sink add out of order");
        let finalized = self.final_extent.contains(height);
        if finalized {
            assert_eq!(self.announced, self.next, "final block {height} above non-final ones");
        }
        self.broadcast(Step::Apply { height, finalized, data }).await?;
        self.next = Extent::through(height);
        if finalized {
            self.announced = self.next;
        }
        Ok(())
    }

    /// A branch won: every subscriber writes what is final, drops pre-commit, and is replayed
    /// from the first non-final height (final is final: nothing below it is re-sent)
    ///
    /// Returns where the producer resumes
    pub async fn reset(&mut self) -> Result<Height, SinkGone> {
        self.broadcast(Step::Reset).await?;
        self.next = self.announced;
        Ok(self.next())
    }

    async fn broadcast(&self, step: Step<T>) -> Result<(), SinkGone> {
        for subscriber in &self.subscribers {
            subscriber.send(step.clone()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Weight for Height {
        fn weight(&self) -> usize {
            0
        }
    }

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn depth(n: u32) -> ReorgDepth {
        ReorgDepth::new(std::num::NonZeroU32::new(n).expect("nz"))
    }

    /// `A(h, final)` / `F(h)` / `R` per step, height checked against its data
    fn drain(sub: &mut Subscription<Height>) -> Vec<String> {
        std::iter::from_fn(|| sub.steps.try_recv().ok())
            .map(|queued| match queued.step {
                Step::Apply { height, finalized, data } => {
                    assert_eq!(height, *data, "height travels with its data");
                    format!("A{height}{}", if finalized { "f" } else { "" })
                }
                Step::Finalized { height } => format!("F{height}"),
                Step::Reset => "R".to_owned(),
            })
            .collect()
    }

    fn steps(spec: &str) -> Vec<String> {
        spec.split_whitespace().map(str::to_owned).collect()
    }

    /// Depth 2, tip 12: 10 is final on arrival, 11 and 12 are pre-commit; each tip advance then
    /// finalises exactly the height it buries, never one not yet delivered
    #[tokio::test]
    async fn finality_is_marked_on_arrival_and_announced_as_the_tip_buries_each_block() {
        let mut builder = SinkBuilder::<Height>::new(depth(2));
        let mut sub = builder.subscribe(
            "one",
            NonZeroUsize::new(1 << 20).expect("nz"),
            Extent::before(h(10)),
        );
        let mut sink = builder.seal();

        sink.set_tip(h(12)).await.expect("tip");
        for height in 10..=12 {
            sink.add(h(height), Arc::new(h(height))).await.expect("add");
        }
        assert_eq!(drain(&mut sub), steps("A10f A11 A12"));

        sink.set_tip(h(13)).await.expect("tip");
        sink.add(h(13), Arc::new(h(13))).await.expect("add");
        assert_eq!(drain(&mut sub), steps("F11 A13"), "tip 13 buries 11 only");

        sink.set_tip(h(20)).await.expect("tip jump");
        assert_eq!(drain(&mut sub), steps("F12 F13"), "jump finalises buried deliveries only");
        for height in 14..=20 {
            sink.add(h(height), Arc::new(h(height))).await.expect("add");
        }
        assert_eq!(drain(&mut sub), steps("A14f A15f A16f A17f A18f A19 A20"));

        sink.finalize_through(Extent::through(h(19))).await.expect("tipless finality");
        sink.finalize_through(Extent::through(h(15))).await.expect("lower extent");
        assert_eq!(drain(&mut sub), steps("F19"), "tipless: same announcement, never back");
    }

    /// Depth 2, subscribers durable at 9 and 11: both fed from the rearmost (10) and both see the
    /// same stream; tip 12 first leaves 11 final all the same (the ahead index holds it durable);
    /// tip 15 buries 12 and 13; a reset rewinds to the first non-final height (14), final is
    /// final, and the producer resumes there
    #[tokio::test]
    async fn every_subscriber_is_fed_from_the_rearmost_resume_point_and_replays_the_same_window() {
        let mut builder = SinkBuilder::<Height>::new(depth(2));
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let mut behind = builder.subscribe("behind", queue, Extent::before(h(10)));
        let mut ahead = builder.subscribe("ahead", queue, Extent::before(h(12)));
        let mut sink = builder.seal();
        assert_eq!(sink.next(), h(10), "production starts at the rearmost");
        assert_eq!(sink.final_extent(), Extent::before(h(12)), "final through the furthest");

        sink.set_tip(h(12)).await.expect("tip");
        for height in 10..=12 {
            sink.add(h(height), Arc::new(h(height))).await.expect("add");
        }
        sink.set_tip(h(15)).await.expect("tip");
        for height in 13..=15 {
            sink.add(h(height), Arc::new(h(height))).await.expect("add");
        }
        assert_eq!(sink.reset().await.expect("reset"), h(14));
        for height in 14..=15 {
            sink.add(h(height), Arc::new(h(height))).await.expect("replay");
        }

        let expected = steps("A10f A11f A12 F12 A13f A14 A15 R A14 A15");
        assert_eq!(drain(&mut behind), expected);
        assert_eq!(drain(&mut ahead), expected, "an index ahead sees what it holds");

        drop(behind);
        let gone = sink.add(h(16), Arc::new(h(16))).await;
        assert!(matches!(gone, Err(SinkGone { subscriber: "behind" })));
    }

    /// The producer's own order is asserted: a skipped height is a bug, not a gap to tolerate
    #[tokio::test]
    #[should_panic(expected = "sink add out of order")]
    async fn a_skipped_height_panics() {
        let mut builder = SinkBuilder::<Height>::new(depth(1));
        let _sub = builder.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"), Extent::ZERO);
        let mut sink = builder.seal();
        sink.add(h(0), Arc::new(h(0))).await.expect("first");
        let _ = sink.add(h(2), Arc::new(h(2))).await;
    }

    struct Blob(usize);

    impl Weight for Blob {
        fn weight(&self) -> usize {
            self.0
        }
    }

    /// Budget = three 100-byte steps: a fourth waits for a pop; a step over the whole budget
    /// passes alone once the queue drains; a subscriber dropped under a full queue fails the
    /// waiting add instead of hanging it
    #[tokio::test]
    async fn a_byte_budget_bounds_the_queue_and_never_deadlocks() {
        let step = size_of::<Step<Blob>>() + 100;
        let oversize = 10 * 3 * step;
        let mut builder = SinkBuilder::<Blob>::new(depth(1));
        let budget = NonZeroUsize::new(3 * step).expect("nz");
        let mut sub = builder.subscribe("one", budget, Extent::ZERO);
        let mut sink = builder.seal();
        sink.set_tip(h(100)).await.expect("tip");
        let popped = |step: Option<Step<Blob>>| match step {
            Some(Step::Apply { height, .. }) => height,
            _ => panic!("expected an apply"),
        };

        for height in 0..3 {
            sink.add(h(height), Arc::new(Blob(100))).await.expect("fits");
        }
        {
            let fourth = sink.add(h(3), Arc::new(Blob(100)));
            tokio::pin!(fourth);
            assert!(futures::poll!(fourth.as_mut()).is_pending(), "budget full");
            assert_eq!(popped(sub.next().await), h(0));
            fourth.await.expect("one pop frees one step");
        }

        for height in 1..=3 {
            assert_eq!(popped(sub.next().await), h(height));
        }
        sink.add(h(4), Arc::new(Blob(oversize))).await.expect("oversize passes an empty queue");
        {
            let small = sink.add(h(5), Arc::new(Blob(100)));
            tokio::pin!(small);
            assert!(futures::poll!(small.as_mut()).is_pending(), "oversize holds all");
            assert_eq!(popped(sub.next().await), h(4));
            small.await.expect("fits once the oversize step pops");
        }

        let heavy = sink.add(h(6), Arc::new(Blob(oversize)));
        tokio::pin!(heavy);
        assert!(futures::poll!(heavy.as_mut()).is_pending(), "waits for an empty queue");
        drop(sub);
        assert!(matches!(heavy.await, Err(SinkGone { subscriber: "one" })));
    }
}
