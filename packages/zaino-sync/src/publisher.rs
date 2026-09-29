//! The [`Producer`](crate::Producer)'s side of its sink: where production starts, which blocks
//! are final, where a reset rewinds to
//!
//! - Start = after the rearmost subscriber's durable tip (every subscriber fed the same heights)
//! - Final = `ReorgDepth` below the highest tip seen, announced in-band: an `Apply` says whether
//!   it is final, and a `Finalized` follows each non-final one once the tip buries it
//! - Every height field = a last height, inclusive (`None` = none yet)

use std::sync::Arc;

use zaino_primitives::types::{Height, ReorgDepth};

use crate::{IndexerDataSink, Step, Weight};

pub(crate) struct Publisher<T> {
    sink: IndexerDataSink<T>,
    depth: ReorgDepth,
    /// Final under the highest tip seen (monotone: a lower winning tip never un-finalises)
    final_tip: Option<Height>,
    /// Delivered and announced final (`== delivered` = none pending)
    announced: Option<Height>,
    delivered: Option<Height>,
}

impl<T: Weight> Publisher<T> {
    /// `durable` = every subscriber's durable tip
    ///
    /// - Production starts after the rearmost
    /// - Final through the furthest (durable = final under some earlier tip: a lower tip after a
    ///   restart cannot un-finalise what an index holds)
    pub(crate) fn new(
        sink: IndexerDataSink<T>,
        depth: ReorgDepth,
        durable: impl IntoIterator<Item = Option<Height>>,
    ) -> Self {
        let durable: Vec<Option<Height>> = durable.into_iter().collect();
        let rearmost = *durable.iter().min().expect("sink with no subscriber");
        let furthest = *durable.iter().max().expect("sink with no subscriber");
        Self { sink, depth, final_tip: furthest, announced: rearmost, delivered: rearmost }
    }

    /// Next height `add` accepts
    pub(crate) fn next(&self) -> Height {
        self.delivered.map_or(Height::GENESIS, Height::next)
    }

    pub(crate) fn depth(&self) -> ReorgDepth {
        self.depth
    }

    /// Last final height, inclusive (`None` = nothing final yet)
    pub(crate) fn final_tip(&self) -> Option<Height> {
        self.final_tip
    }

    /// A `Finalized` for every delivered non-final height `tip` buries
    ///
    /// - A lower tip (shorter winning branch) keeps the final tip where it was
    pub(crate) async fn set_tip(&mut self, tip: Height) {
        self.final_tip = self.final_tip.max(tip.checked_sub(self.depth.get()));
        while self.announced < self.final_tip.min(self.delivered) {
            let height = self.announced.map_or(Height::GENESIS, Height::next);
            self.sink.send(Step::Finalized { height }).await;
            self.announced = Some(height);
        }
    }

    /// Block `height`'s data to every subscriber (contiguity asserted: `next()`, then +1 each)
    pub(crate) async fn add(&mut self, height: Height, data: Arc<T>) {
        assert_eq!(height, self.next(), "sink add out of order");
        let finalized = Some(height) <= self.final_tip;
        if finalized {
            let (announced, delivered) = (self.announced, self.delivered);
            assert_eq!(announced, delivered, "final block {height} above non-final ones");
        }
        self.sink.send(Step::Apply { height, finalized, data }).await;
        self.delivered = Some(height);
        if finalized {
            self.announced = self.delivered;
        }
    }

    /// A branch won: every subscriber writes what is final, drops its non-finalized state, and is
    /// replayed from the first non-final height (final is final: nothing below it is re-sent)
    ///
    /// Returns where the producer resumes
    pub(crate) async fn reset(&mut self) -> Height {
        self.sink.send(Step::Reset).await;
        self.delivered = self.announced;
        self.next()
    }

    pub(crate) fn shutdown(self) {
        self.sink.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use super::*;
    use crate::Subscription;

    impl Weight for Height {
        fn weight(&self) -> usize {
            0
        }
    }

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    fn depth(n: u32) -> ReorgDepth {
        ReorgDepth::new(NonZeroU32::new(n).expect("nz"))
    }

    /// `A(h, final)` / `F(h)` / `R` / `S` per step, height checked against its data
    fn drain(sub: &mut Subscription<Height>) -> Vec<String> {
        std::iter::from_fn(|| sub.try_next())
            .map(|step| match step {
                Step::Apply { height, finalized, data } => {
                    assert_eq!(height, *data, "height travels with its data");
                    format!("A{height}{}", if finalized { "f" } else { "" })
                }
                Step::Finalized { height } => format!("F{height}"),
                Step::Reset => "R".to_owned(),
                Step::Shutdown => "S".to_owned(),
            })
            .collect()
    }

    fn steps(spec: &str) -> Vec<String> {
        spec.split_whitespace().map(str::to_owned).collect()
    }

    /// Depth 2, tip 12: 10 is final on arrival, 11 and 12 are non-finalized; each tip advance then
    /// finalises exactly the height it buries, never one not yet delivered; a lower tip never
    /// moves finality back
    #[tokio::test]
    async fn finality_is_marked_on_arrival_and_announced_as_the_tip_buries_each_block() {
        let mut sink = IndexerDataSink::<Height>::new("test");
        let mut sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        let mut publisher = Publisher::new(sink, depth(2), [Some(h(9))]);

        publisher.set_tip(h(12)).await;
        for height in 10..=12 {
            publisher.add(h(height), Arc::new(h(height))).await;
        }
        assert_eq!(drain(&mut sub), steps("A10f A11 A12"));

        publisher.set_tip(h(13)).await;
        publisher.add(h(13), Arc::new(h(13))).await;
        assert_eq!(drain(&mut sub), steps("F11 A13"), "tip 13 buries 11 only");

        publisher.set_tip(h(20)).await;
        assert_eq!(drain(&mut sub), steps("F12 F13"), "jump finalises deliveries only");
        for height in 14..=20 {
            publisher.add(h(height), Arc::new(h(height))).await;
        }
        assert_eq!(drain(&mut sub), steps("A14f A15f A16f A17f A18f A19 A20"));

        publisher.set_tip(h(15)).await;
        assert_eq!(drain(&mut sub), steps(""), "a lower tip never moves finality back");
    }

    /// Depth 2, subscribers durable through 9 and 11: production starts at 10 (after the
    /// rearmost), 11 final all the same (the ahead index holds it durable); tip 15 buries 12 and
    /// 13; a reset rewinds to the first non-final height (14), final is final, and the producer
    /// resumes there
    #[tokio::test]
    async fn production_starts_after_the_rearmost_durable_tip_and_a_reset_replays_the_window() {
        let mut sink = IndexerDataSink::<Height>::new("test");
        let mut sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        let mut publisher = Publisher::new(sink, depth(2), [Some(h(11)), Some(h(9))]);
        assert_eq!(publisher.next(), h(10), "production starts after the rearmost");
        assert_eq!(publisher.final_tip(), Some(h(11)), "final through the furthest");

        publisher.set_tip(h(12)).await;
        for height in 10..=12 {
            publisher.add(h(height), Arc::new(h(height))).await;
        }
        publisher.set_tip(h(15)).await;
        for height in 13..=15 {
            publisher.add(h(height), Arc::new(h(height))).await;
        }
        assert_eq!(publisher.reset().await, h(14));
        for height in 14..=15 {
            publisher.add(h(height), Arc::new(h(height))).await;
        }
        publisher.shutdown();

        let expected = steps("A10f A11f A12 F12 A13f A14 A15 R A14 A15 S");
        assert_eq!(drain(&mut sub), expected);
    }

    /// The producer's own order is asserted: a skipped height is a bug, not a gap to tolerate
    #[tokio::test]
    #[should_panic(expected = "sink add out of order")]
    async fn a_skipped_height_panics() {
        let mut sink = IndexerDataSink::<Height>::new("test");
        let _sub = sink.subscribe("one", NonZeroUsize::new(1 << 20).expect("nz"));
        let mut publisher = Publisher::new(sink, depth(1), [None]);
        publisher.add(h(0), Arc::new(h(0))).await;
        publisher.add(h(2), Arc::new(h(2))).await;
    }
}
