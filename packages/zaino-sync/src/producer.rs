//! The one task feeding the [`BlockSink`]: the [`VerifiedChain`] followed through a
//! [`ProducerCore`], every block fetched from any source and checked before it is sent
//!
//! ```text
//!   watch<VerifiedChain> ──▶ ┌──────────────┐ ──▶ Apply / Finalized / Reorg ──▶ BlockSink
//!   answer (checked)     ──▶ │ ProducerCore │ ──▶ Fetch (source, height, record)
//!   tick (1 s)           ──▶ └──────────────┘                   │
//!          ▲                                                     ▼
//!          └──────────────────── task: getblock <hash> 0 + check_block
//! ```
//!
//! - Driver = select over chain / answers / tick, outputs in order
//! - Sink sends await backpressure (fetch tasks run on meanwhile)
//! - Block-sink contract: `docs/design/data-sink.md` §"How the producer publishes"

mod checked;
mod core;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn, Span};
use zaino_header_chain::{Record, VerifiedChain};
use zaino_primitives::types::{BlockHash, BlockRef, Height};
use zaino_source::ChainDataSource;

use self::checked::check_block;
use self::core::{Answer, Input, Output, ProducerCore};
use crate::{
    emit,
    report::{self, Progress},
    BlockSink, Step,
};

/// Core re-asked at this pace (retries, hedges)
const TICK: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum ProduceError {
    #[error(
        "block {height:?} is {got} on the verified chain, but an index committed {expected} \
         there (resync required)"
    )]
    Diverged { height: Height, expected: BlockHash, got: BlockHash },
    #[error("header sync stopped publishing the verified chain")]
    ChainGone,
}

/// One fetch's outcome, back from its task
struct Answered {
    source: usize,
    height: Height,
    hash: BlockHash,
    answer: Answer,
}

pub struct Producer<S> {
    sink: BlockSink,
    sources: Vec<Arc<S>>,
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    core: ProducerCore,
    progress: Arc<Progress>,
    fetched: watch::Sender<Option<Height>>,
    live: Span,
}

impl<S: ChainDataSource> Producer<S> {
    /// - `sources` = everything that serves blocks by hash, in a fixed order (any may serve any
    ///   block: each is checked)
    /// - `concurrency` = blocks in flight ahead of the next one sent
    /// - `durable` = every subscriber's durable tip (production starts after the rearmost)
    pub fn new(
        sink: BlockSink,
        sources: Vec<Arc<S>>,
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        concurrency: NonZeroUsize,
        durable: impl IntoIterator<Item = Option<BlockRef>>,
    ) -> Self {
        let core = ProducerCore::new(sources.len(), concurrency.get(), durable);
        let fetched = watch::channel(None).0;
        Self { sink, sources, chain, core, progress: Arc::default(), fetched, live: Span::none() }
    }

    pub fn subscribe_fetched(&self) -> watch::Receiver<Option<Height>> {
        self.fetched.subscribe()
    }

    /// Reorgs and new tips log under `span`
    pub fn with_live_span(mut self, span: Span) -> Self {
        self.live = span;
        self
    }

    /// - Cancel → `Ok`
    /// - Either way: sink ends with `Shutdown` (every index loop persists, stops)
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), ProduceError> {
        let report = report::run(Arc::clone(&self.progress));
        let produce = async {
            tokio::select! {
                produced = self.produce() => produced,
                () = report => unreachable!("the reporter loops until dropped"),
            }
        };
        let produced = cancel.run_until_cancelled(produce).await;
        self.sink.shutdown();
        produced.unwrap_or(Ok(()))
    }

    async fn produce(&mut self) -> Result<(), ProduceError> {
        let mut fetches: JoinSet<Answered> = JoinSet::new();
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut logged: Option<BlockRef> = None;
        let current = self.chain.borrow_and_update().clone();
        let mut input = current.map(Input::Chain).unwrap_or(Input::Tick);
        loop {
            if let Input::Chain(chain) = &input {
                self.progress.target(chain.best().height);
                emit::tip(chain.best().height);
            }
            let now = tokio::time::Instant::now().into_std();
            for output in self.core.step(input, now)? {
                self.output(output, &mut fetches).await;
            }
            if cfg!(debug_assertions) {
                self.core.check();
            }
            self.log_tip(&mut logged);
            input = tokio::select! {
                changed = self.chain.changed() => {
                    changed.map_err(|_| ProduceError::ChainGone)?;
                    match self.chain.borrow_and_update().clone() {
                        Some(chain) => Input::Chain(chain),
                        None => Input::Tick,
                    }
                }
                Some(joined) = fetches.join_next() => {
                    let Answered { source, height, hash, answer } = match joined {
                        Ok(answered) => answered,
                        Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
                        Err(join) => panic!("block fetch task cancelled: {join}"),
                    };
                    Input::Answer { source, height, hash, answer }
                }
                _ = ticks.tick() => Input::Tick,
            };
        }
    }

    async fn output(&mut self, output: Output, fetches: &mut JoinSet<Answered>) {
        match output {
            Output::Apply { block, finalized } => {
                let height = block.header().height;
                emit::added(&block);
                self.progress.added(&block);
                self.fetched.send_replace(Some(height));
                self.sink.send(Step::Apply { height, finalized, data: block }).await;
            }
            Output::Finalized(height) => self.sink.send(Step::Finalized { height }).await,
            Output::Reorg { fork, dropped } => {
                emit::reorg();
                let fork = u32::from(fork);
                self.live.in_scope(|| warn!(fork, dropped, "Chain reorg detected"));
                self.sink.send(Step::Reorg).await;
            }
            Output::Fetch { source, height, record } => {
                fetches.spawn(fetch(Arc::clone(&self.sources[source]), source, height, record));
            }
            Output::Misanswered { source, height, why } => {
                let height = u32::from(height);
                warn!(source, height, %why, "Source misanswered a block, asking another");
            }
            Output::Unserved { height } => {
                debug!(height = u32::from(height), "No source served the block, retrying");
            }
        }
    }

    /// One line per best tip reached (~75 s apart at the tip), with where finality stands
    fn log_tip(&self, logged: &mut Option<BlockRef>) {
        let Some(chain) = self.chain.borrow().clone() else { return };
        let best = chain.best();
        if self.core.delivered() != Some(best.height) || *logged == Some(best) {
            return;
        }
        *logged = Some(best);
        let time = chain.header_at(best.height).map_or(0, |record| record.time);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let age = Duration::from_secs(now.saturating_sub(u64::from(time)));
        let finalized = chain.final_tip().map_or(0, |tip| u32::from(tip.height));
        let (height, hash) = (u32::from(best.height), best.hash);
        let age = report::Human(age);
        self.live.in_scope(|| info!(height, %hash, %age, finalized, "Chain tip advanced"));
    }
}

/// `getblock <hash> 0` off `source`, checked against `record` (on a task: decode + merkle spread
/// across the runtime's threads)
async fn fetch<S: ChainDataSource>(
    source: Arc<S>,
    index: usize,
    height: Height,
    record: Record,
) -> Answered {
    let answer = match source.get_block_by_hash(record.hash).await {
        Ok(block) => match check_block(block, height, &record) {
            Ok(checked) => Answer::Checked(checked),
            Err(why) => Answer::Misanswered(why),
        },
        Err(error) => {
            debug!(source = index, height = u32::from(height), %error, "Block fetch failed");
            Answer::Failed
        }
    };
    Answered { source: index, height, hash: record.hash, answer }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zaino_header_chain::HeaderChain;
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{Block, ReorgDepth};
    use zaino_source::mock::MockChain;

    use super::*;

    /// Header chain depth 3, A 0..=8, B7 (heavier) forking after A6; validators on A and B: steps
    /// traced through a moving final tip, a retreat onto B7, a poisoned b8 (refused, served by the
    /// other), finality across the fork
    #[tokio::test(start_paused = true)]
    async fn follows_the_verified_chain_through_finality_reorgs_and_a_poisoned_source() {
        let mut chain = Chain::new();
        let a8 = chain.extend(chain.genesis().hash, 8);
        let a: Vec<Block> = chain.path(a8.hash);
        let b7 = chain.mine_bits(a[6].header().hash, a[7].header().time, 0x1f0f_0f0f);
        let b10 = chain.extend(b7.hash, 3);
        let b: Vec<Block> = chain.path(b10.hash)[7..].to_vec();
        let b8 = b[1].header().hash;
        let names: HashMap<BlockHash, String> = (a.iter().zip(0..))
            .map(|(block, h)| (block.header().hash, format!("a{h}")))
            .chain(b.iter().zip(7..).map(|(block, h)| (block.header().hash, format!("b{h}"))))
            .collect();
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz"));
        let mut headers = HeaderChain::regtest_in_memory(chain.genesis().hash, depth);
        let on_a = Arc::new(MockChain::serving(a.clone()));
        let on_b = Arc::new(MockChain::serving(chain.path(b8)));
        let (verified, verified_rx) = watch::channel(None);
        let publish = |headers: &mut HeaderChain, blocks: &[Block], finalize: bool| {
            headers.insert_blocks(blocks).expect("valid headers");
            if let Some(boundary) = headers.finalizable().filter(|_| finalize) {
                headers.finalize(boundary).expect("in-memory store");
            }
            verified.send_replace(headers.verified().map(Arc::new));
        };
        let mut block_sink = BlockSink::new("blocks");
        let mut index = block_sink.subscribe("index", NonZeroUsize::new(1 << 20).expect("nz"));
        let sources = vec![Arc::clone(&on_a), Arc::clone(&on_b)];
        let lookahead = NonZeroUsize::new(4).expect("nz");
        let producer = Producer::new(block_sink, sources, verified_rx, lookahead, [None]);
        let cancel = CancellationToken::new();
        let producer = tokio::spawn(producer.run(cancel.clone()));
        let mut drain = async || {
            let mut steps = Vec::new();
            while let Ok(step) = tokio::time::timeout(Duration::from_secs(5), index.next()).await {
                steps.push(match step {
                    Step::Apply { data, finalized, .. } => {
                        let name = &names[&data.header().hash];
                        format!("{name}{}", if finalized { "f" } else { "" })
                    }
                    Step::Finalized { height } => format!("F{height}"),
                    Step::Reorg => "R".to_owned(),
                    Step::Shutdown => "S".to_owned(),
                });
            }
            steps.join(" ")
        };

        publish(&mut headers, &a[..=6], true);
        assert_eq!(drain().await, "a0f a1f a2f a3f a4 a5 a6", "final through 6 − 3, the rest held");
        publish(&mut headers, &a[7..], true);
        assert_eq!(drain().await, "F4 F5 a7 a8", "8 − 3 = 5 final");
        // B7 alone outweighs A7 + A8 (one heavier block): the best retreats onto B7
        publish(&mut headers, &b[..1], false);
        assert_eq!(drain().await, "R a6 b7", "a6 replayed from the window, b7 fetched");
        // a poisoned b8 on A's node (it serves B now): refused, B's node serves the real one
        let txs = [b[1].transactions().to_vec(), a[1].transactions().to_vec()].concat();
        let poisoned = Block::new(b[1].header().clone(), txs);
        on_a.extend_best([b[0].clone(), poisoned]);
        on_b.set_reachable(false);
        publish(&mut headers, &b[1..2], false);
        assert_eq!(drain().await, "", "only a poisoned b8 on offer: nothing sent");
        on_b.set_reachable(true);
        assert_eq!(drain().await, "b8", "the honest source, once back (the poisoner benched)");
        on_b.extend_best(b[2..].to_vec());
        publish(&mut headers, &b[2..], true);
        assert_eq!(drain().await, "F6 F7 b9 b10", "10 − 3: final across the fork");

        cancel.cancel();
        producer.await.expect("join").expect("cancel = clean stop");
        assert!(matches!(index.next().await, Step::Shutdown), "cancel → Shutdown");
    }

    /// Indexes durable at A2 and A4 (header store reset: nothing final): nothing sent until a
    /// final tip covers A4, then the rearmost's next heights (4 skipped by the index ahead); a
    /// durable tip off the final chain stops production before any step
    #[tokio::test(start_paused = true)]
    async fn durable_tips_wait_for_a_final_chain_covering_them_and_a_foreign_one_stops_production()
    {
        let mut chain = Chain::new();
        let a8 = chain.extend(chain.genesis().hash, 8);
        let a: Vec<Block> = chain.path(a8.hash);
        let foreign_4 = chain.extend(a[2].header().hash, 2);
        let at = |h: usize| BlockRef {
            hash: a[h].header().hash,
            height: Height::try_from(h as u32).expect("h"),
        };
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(3).expect("nz"));
        let lookahead = NonZeroUsize::new(4).expect("nz");
        let source = || vec![Arc::new(MockChain::serving(a.clone()))];
        let verified = |tip: usize, finalize: bool| {
            let mut headers = HeaderChain::regtest_in_memory(chain.genesis().hash, depth);
            headers.insert_blocks(&a[..=tip]).expect("valid headers");
            if let Some(boundary) = headers.finalizable().filter(|_| finalize) {
                headers.finalize(boundary).expect("in-memory store");
            }
            headers.verified().map(Arc::new)
        };
        let trace = |step: Step<Block>| match step {
            Step::Apply { height, finalized, .. } => {
                format!("{height}{}", if finalized { "f" } else { "" })
            }
            Step::Finalized { height } => format!("F{height}"),
            Step::Reorg => "R".to_owned(),
            Step::Shutdown => "S".to_owned(),
        };

        let (chain_tx, chain_rx) = watch::channel(verified(8, false));
        let mut block_sink = BlockSink::new("blocks");
        let mut index = block_sink.subscribe("index", NonZeroUsize::new(1 << 20).expect("nz"));
        let durable = [Some(at(2)), Some(at(4))];
        let producer = Producer::new(block_sink, source(), chain_rx, lookahead, durable);
        let producer = tokio::spawn(producer.run(CancellationToken::new()));
        let idle = tokio::time::timeout(Duration::from_secs(30), index.next()).await;
        assert!(idle.is_err(), "nothing final yet: 4 unchecked, nothing sent");
        chain_tx.send_replace(verified(8, true));
        let mut steps = Vec::new();
        while let Ok(step) = tokio::time::timeout(Duration::from_secs(5), index.next()).await {
            steps.push(trace(step));
        }
        assert_eq!(steps.join(" "), "3f 4f 5f 6 7 8", "final through 5, both tips checked");
        assert!(!producer.is_finished(), "following");

        let foreign = BlockRef { hash: foreign_4.hash, height: foreign_4.height };
        let (_chain, chain_rx) = watch::channel(verified(8, true));
        let mut block_sink = BlockSink::new("blocks");
        let mut index = block_sink.subscribe("index", NonZeroUsize::new(1 << 20).expect("nz"));
        let producer = Producer::new(block_sink, source(), chain_rx, lookahead, [Some(foreign)]);
        let stopped = producer.run(CancellationToken::new()).await.expect_err("foreign durable 4");
        assert!(
            matches!(stopped, ProduceError::Diverged { height, expected, got }
                if u32::from(height) == 4 && expected == foreign.hash && got == at(4).hash),
            "{stopped}"
        );
        assert!(matches!(index.next().await, Step::Shutdown), "no step before the stop");
    }
}
