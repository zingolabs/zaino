//! Drives one [`IndexWriter`] from its [`Feed`]: `tokio::spawn(follower.run(shutdown))`
//!
//! - Final `Apply` → staged for `finalize` (bulk path: one fold per block); non-final → `apply`
//!   into the non-finalized state, staged later when its `Finalized` arrives
//! - Every step forwarded 1:1 to its [`Downstream`] (`Apply` data = what the writer derives)
//! - `Shutdown` → writes what is final, ends the downstream sink, returns
//! - Failure → cancels `shutdown`, pops its feed through `Shutdown` (never dropped before)

use std::{collections::VecDeque, future::Future, num::NonZeroUsize, sync::Arc, time::Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_chainview::QuorumTip;
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};

use crate::{
    report::Human, Derives, Feed, IndexWriter, IndexerDataSink, Linked, Reads, Served, Step,
    Subscription, Weight,
};

mod sealed {
    pub trait Sealed {}
}

/// Where a follower republishes the steps it follows (sealed: nowhere, or an
/// [`IndexerDataSink`] of what its writer derives)
pub trait Downstream<W: IndexWriter>: sealed::Sealed + Send + 'static {
    /// One `Apply` per block of the run just delivered
    fn applied(
        &mut self,
        writer: &mut W,
        run: &[(Height, bool, Arc<W::Input>)],
    ) -> impl Future<Output = Result<(), W::Error>> + Send;

    /// `Finalized` or `Reorg`, as followed
    fn signal(&mut self, step: &Step<W::Input>) -> impl Future<Output = ()> + Send;

    fn shutdown(self);
}

impl sealed::Sealed for () {}

impl<W: IndexWriter> Downstream<W> for () {
    async fn applied(
        &mut self,
        _: &mut W,
        _: &[(Height, bool, Arc<W::Input>)],
    ) -> Result<(), W::Error> {
        Ok(())
    }

    async fn signal(&mut self, _: &Step<W::Input>) {}

    fn shutdown(self) {}
}

impl<T> sealed::Sealed for IndexerDataSink<T> {}

impl<W: Derives> Downstream<W> for IndexerDataSink<W::Item> {
    async fn applied(
        &mut self,
        writer: &mut W,
        run: &[(Height, bool, Arc<W::Input>)],
    ) -> Result<(), W::Error> {
        let blocks: Vec<Arc<W::Input>> = run.iter().map(|(_, _, data)| Arc::clone(data)).collect();
        let items = writer.derive(&blocks).await?;
        assert_eq!(items.len(), run.len(), "{}: one derived item per block", W::NAME);
        for (&(height, finalized, _), item) in run.iter().zip(items) {
            self.send(Step::Apply { height, finalized, data: Arc::new(item) }).await;
        }
        Ok(())
    }

    async fn signal(&mut self, step: &Step<W::Input>) {
        let step = match *step {
            Step::Finalized { height } => Step::Finalized { height },
            Step::Reorg => Step::Reorg,
            Step::Apply { .. } | Step::Shutdown => {
                unreachable!("Apply = `applied`, Shutdown = `shutdown`")
            }
        };
        self.send(step).await;
    }

    fn shutdown(self) {
        IndexerDataSink::shutdown(self);
    }
}

/// Why a follower stopped (all fatal: zainod exits, never retries)
#[derive(Debug, thiserror::Error)]
pub enum FollowError<E> {
    #[error("{index} index failed: {source}")]
    Index {
        index: &'static str,
        #[source]
        source: E,
    },

    /// Delivered chain does not extend the one this index holds (a reorg below the window, a
    /// validator reset or resynced elsewhere, a directory from another chain): resync required
    #[error(
        "{index} index: block {height} has parent {got}, but the index holds {expected} at \
         {height} - 1; the validator's chain diverged below the durable tip (resync required)"
    )]
    Unlinked { index: &'static str, height: Height, expected: BlockHash, got: BlockHash },

    /// Block replayed at this index's durable tip is not the one it committed there
    #[error(
        "{index} index: block {height} is {got}, but the index committed {expected} at {height}; \
         the validator's chain diverged below the durable tip (resync required)"
    )]
    Diverged { index: &'static str, height: Height, expected: BlockHash, got: BlockHash },
}

/// One index's write loop, plus the handles serving + metrics take before it is spawned
pub struct IndexFollower<W: IndexWriter, F = Subscription<<W as IndexWriter>::Input>, D = ()> {
    writer: W,
    feed: F,
    downstream: D,
    batch_bytes: NonZeroUsize,
    depth: ReorgDepth,
    /// Chainview's quorum tip: the serving gate's reference and the bulk / follow switch
    tips: watch::Receiver<Option<QuorumTip>>,
    /// Durable tip height (inclusive; `None` = empty), published after each fsync
    finalized: watch::Sender<Option<Height>>,
    /// Applied height (inclusive; `None` = empty), published with each view
    applied: watch::Sender<Option<Height>>,
    /// Requests this index's services answered
    reads: Reads,
    /// When the last reset dropped the non-finalized state (`None` = no replay pending)
    reorg: Option<Instant>,
    /// Non-finalized state, published after each step (what serving pins)
    view: Arc<arc_swap::ArcSwap<W::View>>,
    /// Opens once applied through the tip; closes on a reset or past the reorg depth behind (a
    /// syncing index refuses every request: "not found" and "not indexed yet" must not be confusable)
    synced: watch::Sender<bool>,
    /// Hash at the last delivered height (`None` = nothing delivered yet)
    linked: Option<BlockHash>,
    /// Batch being written off the loop (at most one: the next `finalize` settles it first)
    in_flight: Option<InFlight<W::Done, W::Error>>,
}

/// One `finalize` write on the blocking pool, and what its log line reports
struct InFlight<D, E> {
    write: tokio::task::JoinHandle<Result<D, E>>,
    blocks: usize,
    bytes: usize,
    started: std::time::Instant,
}

impl<W: IndexWriter, F: Feed<Item = W::Input>> IndexFollower<W, F, ()> {
    /// - `batch_bytes` = staged [`Weight`] that triggers a `finalize` (one fsync); past bulk, each
    ///   final block commits as it arrives regardless
    /// - `tips` = chainview's quorum tip (`ChainViewSubscriber::subscribe_tip`)
    /// - `depth` = how far behind the tip the serving gate stays open
    pub fn new(
        writer: W,
        feed: F,
        tips: watch::Receiver<Option<QuorumTip>>,
        batch_bytes: NonZeroUsize,
        depth: ReorgDepth,
    ) -> Self {
        let durable = writer.finalized_height();
        assert_eq!(writer.applied_height(), durable, "{}: non-finalized state at boot", W::NAME);
        Self {
            finalized: watch::Sender::new(durable),
            applied: watch::Sender::new(durable),
            reads: Reads::default(),
            reorg: None,
            view: Arc::new(arc_swap::ArcSwap::from_pointee(writer.view())),
            synced: watch::Sender::new(false),
            linked: None,
            in_flight: None,
            tips,
            writer,
            feed,
            downstream: (),
            batch_bytes,
            depth,
        }
    }

    /// Every step followed, republished into `sink` (`Apply` data = what the writer derives)
    pub fn publishing(
        self,
        sink: IndexerDataSink<W::Item>,
    ) -> IndexFollower<W, F, IndexerDataSink<W::Item>>
    where
        W: Derives,
    {
        IndexFollower {
            writer: self.writer,
            feed: self.feed,
            downstream: sink,
            batch_bytes: self.batch_bytes,
            depth: self.depth,
            tips: self.tips,
            finalized: self.finalized,
            applied: self.applied,
            reads: self.reads,
            reorg: self.reorg,
            view: self.view,
            synced: self.synced,
            linked: self.linked,
            in_flight: self.in_flight,
        }
    }
}

impl<W: IndexWriter, F, D> IndexFollower<W, F, D> {
    /// The writer, for handles it hands out (e.g. a service over its store) before `run`
    pub fn writer(&self) -> &W {
        &self.writer
    }

    /// Durable tip height (inclusive; `None` = empty), after each fsync
    pub fn subscribe_finalized(&self) -> watch::Receiver<Option<Height>> {
        self.finalized.subscribe()
    }

    /// Applied height (inclusive; `None` = empty), with each published view
    pub fn subscribe_applied(&self) -> watch::Receiver<Option<Height>> {
        self.applied.subscribe()
    }

    pub fn subscribe_synced(&self) -> watch::Receiver<bool> {
        self.synced.subscribe()
    }

    /// Requests every [`served`](Self::served) handle answered
    pub fn reads(&self) -> Reads {
        self.reads.clone()
    }

    /// What this index's services read: the view published after every step and commit, gated
    /// on `synced`
    pub fn served(&self) -> Served<W::View> {
        Served::counted(Arc::clone(&self.view), self.synced.subscribe(), self.reads.clone())
    }
}

impl<W: IndexWriter, F: Feed<Item = W::Input>, D: Downstream<W>> IndexFollower<W, F, D> {
    /// `data` must extend the last delivered block, and match the durable tip where it lands on it
    ///
    /// - First block: its parent checkable only when it extends the durable tip (a replay starts
    ///   below, where this index keeps no hashes)
    fn link(&mut self, height: Height, data: &W::Input) -> Result<(), FollowError<W::Error>> {
        assert_eq!(data.height(), height, "{}: step height != block height", W::NAME);
        let durable = self.writer.finalized_tip();
        let parent = match self.linked {
            None if height.checked_sub(1) == durable.map(|tip| tip.height) => {
                durable.map(|tip| tip.hash)
            }
            linked => linked,
        };
        if let Some(expected) = parent {
            if data.prev_hash() != expected {
                return Err(FollowError::Unlinked {
                    index: W::NAME,
                    height,
                    expected,
                    got: data.prev_hash(),
                });
            }
        }
        if let Some(BlockRef { height: tip, hash: expected }) = durable {
            if tip == height && data.hash() != expected {
                return Err(FollowError::Diverged {
                    index: W::NAME,
                    height,
                    expected,
                    got: data.hash(),
                });
            }
        }
        self.linked = Some(data.hash());
        Ok(())
    }

    /// Follows through `Shutdown`; a failure cancels `shutdown` (the pipeline's stop) and is
    /// returned once `Shutdown` reaches every queue of the feed
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), FollowError<W::Error>> {
        let followed = self.follow().await;
        if followed.is_err() {
            shutdown.cancel();
        }
        // no-op past a clean stop; a zip whose derived stream ended first still owes its upstream
        self.feed.skip_to_shutdown().await;
        self.downstream.shutdown();
        followed
    }

    async fn follow(&mut self) -> Result<(), FollowError<W::Error>> {
        let fail = |source| FollowError::Index { index: W::NAME, source };
        // applied, not yet final (oldest first)
        let mut window: VecDeque<Arc<W::Input>> = VecDeque::new();
        // final, not yet fsynced (one `finalize` per `batch_bytes`)
        let mut staged: Staged<W::Input> = Staged::default();
        // last delivered height, inclusive (= durable + staged + window, once past any replay
        // below durable); `None` = nothing yet (the first block fixes where the sink started)
        let mut delivered: Option<Height> = None;
        // non-`Apply` step popped while gathering a run (handled next, keeping step order)
        let mut held: Option<Step<W::Input>> = None;

        loop {
            let step = match held.take() {
                Some(step) => step,
                None => match self.next_landing_writes().await.map_err(fail)? {
                    Some(step) => step,
                    // tip moved, no block behind it: gate re-judged (e.g. the producer stalled)
                    None => {
                        if self.feed.is_idle() {
                            self.idle(&mut staged, delivered).await.map_err(fail)?;
                        }
                        continue;
                    }
                },
            };
            match step {
                Step::Apply { height, finalized, data } => {
                    // run = this block + every `Apply` already queued, to one batch's bytes
                    let mut run = vec![(height, finalized, data)];
                    let mut bytes = run[0].2.weight();
                    while bytes < self.batch_bytes.get() {
                        match self.feed.try_next() {
                            Some(Step::Apply { height, finalized, data }) => {
                                bytes = bytes.saturating_add(data.weight());
                                run.push((height, finalized, data));
                            }
                            other => {
                                held = other;
                                break;
                            }
                        }
                    }

                    let mut durable_at = Vec::with_capacity(run.len());
                    for (height, _, data) in &run {
                        let durable = self.writer.finalized_height();
                        match delivered {
                            None => assert!(
                                height.checked_sub(1) <= durable,
                                "{}: sink started above the durable tip",
                                W::NAME
                            ),
                            Some(last) => assert_eq!(
                                *height,
                                last.next(),
                                "{}: sink delivered out of order",
                                W::NAME
                            ),
                        }
                        self.link(*height, data)?;
                        delivered = Some(*height);
                        durable_at.push(durable);
                    }
                    let blocks: Vec<Arc<W::Input>> =
                        run.iter().map(|(_, _, data)| Arc::clone(data)).collect();
                    self.writer.deliver(&blocks).await.map_err(fail)?;
                    self.downstream.applied(&mut self.writer, &run).await.map_err(fail)?;

                    for ((height, finalized, data), durable) in run.into_iter().zip(durable_at) {
                        if Some(height) <= durable {
                            // replay below this index's own durable tip: already held
                            assert!(finalized, "{}: durable height not final", W::NAME);
                        } else if finalized {
                            assert!(
                                window.is_empty(),
                                "{}: final block above non-finalized",
                                W::NAME
                            );
                            staged.push(data);
                        } else {
                            // non-finalized from durable end (bulk→tip: staged written first)
                            if self.writer.applied_height() < height.checked_sub(1) {
                                self.drain(&mut staged).await.map_err(fail)?;
                            }
                            self.writer.apply(&data).await.map_err(fail)?;
                            window.push_back(data);
                        }
                        // per block, not per run: batch boundaries independent of run lengths
                        if staged.bytes >= self.batch_bytes.get() {
                            self.flush(&mut staged).await.map_err(fail)?;
                        }
                    }
                }
                Step::Finalized { height } => {
                    let Some(oldest) = window.pop_front() else {
                        panic!("{}: Finalized with no non-finalized block", W::NAME)
                    };
                    assert_eq!(height, oldest.height(), "{}: finalized out of order", W::NAME);
                    staged.push(oldest);
                    self.downstream.signal(&Step::Finalized { height }).await;
                }
                Step::Reorg => {
                    // final is final: write it, drop the non-finalized state, replay after it
                    let finalized_tip = match window.front() {
                        Some(oldest) => oldest.height().checked_sub(1),
                        None => delivered,
                    };
                    let (from, to) =
                        (finalized_tip.map_or(0, u32::from), delivered.map_or(0, u32::from));
                    warn!(durable = from, dropped = to - from, "Reorg received, replaying");
                    self.reorg = Some(Instant::now());
                    self.drain(&mut staged).await.map_err(fail)?;
                    self.writer.reset().await.map_err(fail)?;
                    self.downstream.signal(&Step::Reorg).await;
                    window.clear();
                    delivered = finalized_tip;
                    let (durable, applied) =
                        (self.writer.finalized_height(), self.writer.applied_height());
                    assert_eq!(durable, finalized_tip, "{}: durable", W::NAME);
                    assert_eq!(applied, finalized_tip, "{}: applied", W::NAME);
                    // replay links onto the last final block, now durable
                    self.linked = self.writer.finalized_tip().map(|tip| tip.hash);
                    self.set_synced(false);
                }
                Step::Shutdown => break,
            }

            self.publish_view();

            if staged.bytes >= self.batch_bytes.get() {
                self.flush(&mut staged).await.map_err(fail)?;
            }
            // on idle, not per step (a queued burst cannot flap the gate or split a batch)
            if held.is_none() && self.feed.is_idle() {
                self.idle(&mut staged, delivered).await.map_err(fail)?;
            }
        }

        self.drain(&mut staged).await.map_err(fail)
    }

    /// Queue empty: final blocks durable once at the tip, and the serving gate set
    async fn idle(
        &mut self,
        staged: &mut Staged<W::Input>,
        delivered: Option<Height>,
    ) -> Result<(), W::Error> {
        let tip = self.tips.borrow().map(|tip| tip.block.height);
        let reached = tip.is_some() && tip <= delivered;
        // tip itself final (retreat onto the final boundary) → only the staged write puts it in
        // the view
        if reached && tip > self.writer.applied_height() {
            self.drain(staged).await?;
        }
        let applied = self.writer.applied_height();
        let at_tip = tip.is_some() && tip <= applied;
        // following: each final block durable as it arrives (a batch would take hours)
        if at_tip && !staged.blocks.is_empty() {
            self.flush(staged).await?;
        }
        // copied out: a `borrow()` guard held into `set_synced` deadlocks its write
        let serving = *self.synced.borrow();
        self.set_synced(match serving {
            false => at_tip,
            // open → closes only past the reorg depth (a new tip lands before its block)
            true => tip.is_none_or(|tip| {
                u32::from(tip).saturating_sub(applied.map_or(0, u32::from)) <= self.depth.get()
            }),
        });
        Ok(())
    }

    /// Next step (`None` = the quorum tip moved first); a write finishing first is landed
    /// meanwhile (durability published as it happens, not at the next block)
    /// - chainview gone → its branch disabled (`Ok` pattern), steps still followed to `Shutdown`
    async fn next_landing_writes(&mut self) -> Result<Option<Step<W::Input>>, W::Error> {
        loop {
            let (feed, tips, in_flight) = (&mut self.feed, &mut self.tips, &mut self.in_flight);
            let landing = async {
                match in_flight.as_mut() {
                    Some(in_flight) => (&mut in_flight.write).await,
                    None => std::future::pending().await,
                }
            };
            let done = tokio::select! {
                step = feed.next() => return Ok(Some(step)),
                Ok(()) = tips.changed() => return Ok(None),
                done = landing => done,
            };
            self.land(done).await?;
        }
    }

    /// Starts writing `staged` off the loop, once the previous write has landed
    async fn flush(&mut self, staged: &mut Staged<W::Input>) -> Result<(), W::Error> {
        self.settle().await?;
        if staged.blocks.is_empty() {
            return Ok(());
        }
        let write = self.writer.finalize(&staged.blocks).await?;
        self.in_flight = Some(InFlight {
            write: tokio::task::spawn_blocking(write),
            blocks: staged.blocks.len(),
            bytes: staged.bytes,
            started: std::time::Instant::now(),
        });
        *staged = Staged::default();
        Ok(())
    }

    /// `staged` written and landed: nothing in flight after
    async fn drain(&mut self, staged: &mut Staged<W::Input>) -> Result<(), W::Error> {
        self.flush(staged).await?;
        self.settle().await
    }

    /// Waits for the write in flight, if any, and lands it
    async fn settle(&mut self) -> Result<(), W::Error> {
        match self.in_flight.as_mut() {
            Some(in_flight) => {
                let done = (&mut in_flight.write).await;
                self.land(done).await
            }
            None => Ok(()),
        }
    }

    /// Durable *before* published: nothing downstream is told of a height not on disk
    async fn land(
        &mut self,
        done: Result<Result<W::Done, W::Error>, tokio::task::JoinError>,
    ) -> Result<(), W::Error> {
        let in_flight = self.in_flight.take().expect("landing a write in flight");
        let done = done.unwrap_or_else(|join| std::panic::resume_unwind(join.into_panic()))?;
        self.writer.committed(done).await?;
        let finalized = self.writer.finalized_height();
        debug!(
            height = finalized.map_or(0, u32::from),
            blocks = in_flight.blocks,
            bytes = in_flight.bytes,
            elapsed = ?in_flight.started.elapsed(),
            "Committed batch"
        );
        self.publish_view();
        let before = self.finalized.send_replace(finalized);
        assert!(before <= finalized, "{}: durable tip moved back", W::NAME);
        Ok(())
    }

    /// Serving gate set; a reorg's replay is timed from its reset to the gate reopening
    fn set_synced(&mut self, serving: bool) {
        let changed = self.synced.send_if_modified(|current| {
            let changed = *current != serving;
            *current = serving;
            changed
        });
        if !changed {
            return;
        }
        let height = self.writer.applied_height().map_or(0, u32::from);
        match (serving, self.reorg) {
            (true, Some(reset)) => {
                info!(height, took = %Human(reset.elapsed()), "Reorg replayed, serving");
                self.reorg = None;
            }
            (true, None) => info!(height, "Serving"),
            // `Reorg received` already said so
            (false, Some(_)) => {}
            (false, None) => info!(height, "Syncing, requests refused"),
        }
    }

    /// View + applied height published together
    fn publish_view(&self) {
        self.view.store(Arc::new(self.writer.view()));
        let applied = self.writer.applied_height();
        self.applied.send_if_modified(|current| std::mem::replace(current, applied) != applied);
    }
}

/// Final blocks awaiting one `finalize`, with their summed [`Weight`]
struct Staged<T> {
    blocks: Vec<Arc<T>>,
    bytes: usize,
}

impl<T> Default for Staged<T> {
    fn default() -> Self {
        Self { blocks: Vec::new(), bytes: 0 }
    }
}

impl<T: Weight> Staged<T> {
    fn push(&mut self, block: Arc<T>) {
        self.bytes = self.bytes.saturating_add(block.weight());
        self.blocks.push(block);
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, time::Duration};

    use tokio_util::sync::CancellationToken;
    use zaino_primitives::types::ReorgDepth;

    use super::*;
    use crate::publisher::Publisher;

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// A block on chain `fork`: hash = (height, fork), parent = the same fork one height down
    #[derive(Debug, Clone, Copy)]
    struct Chained {
        height: Height,
        fork: u8,
        weight: usize,
    }

    impl crate::Weight for Chained {
        fn weight(&self) -> usize {
            self.weight
        }
    }

    fn hash_of(height: u32, fork: u8) -> BlockHash {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&height.to_le_bytes());
        bytes[4] = fork;
        BlockHash::from(bytes)
    }

    fn on(fork: u8, height: u32) -> Arc<Chained> {
        Arc::new(Chained { height: h(height), fork, weight: 1 })
    }

    /// Chainview's quorum tip at `height` on fork 0
    fn quorum(height: u32) -> Option<QuorumTip> {
        let block = BlockRef { hash: hash_of(height, 0), height: h(height) };
        Some(QuorumTip { block, agreed_by: zaino_chainview::EndpointSet::default() })
    }

    impl Linked for Chained {
        fn height(&self) -> Height {
            self.height
        }
        fn hash(&self) -> BlockHash {
            hash_of(u32::from(self.height), self.fork)
        }
        fn prev_hash(&self) -> BlockHash {
            hash_of(u32::from(self.height).wrapping_sub(1), self.fork)
        }
    }

    /// Heights only (durable tip on fork 0); `log` = `D<h>` per deliver, `F<a>..=<b>` per finalize
    /// (`a` to `b`, both inclusive)
    #[derive(Default)]
    struct Counting {
        applied: Option<Height>,
        finalized: Option<Height>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl IndexWriter for Counting {
        type Input = Chained;
        type View = Option<Height>;
        type Error = std::convert::Infallible;
        type Done = Height;
        const NAME: &'static str = "counting";

        fn finalized_tip(&self) -> Option<BlockRef> {
            self.finalized.map(|height| BlockRef { hash: hash_of(u32::from(height), 0), height })
        }
        fn applied_height(&self) -> Option<Height> {
            self.applied
        }
        fn view(&self) -> Option<Height> {
            self.applied
        }
        async fn deliver(&mut self, blocks: &[Arc<Chained>]) -> Result<(), Self::Error> {
            let mut log = self.log.lock().expect("log");
            log.extend(blocks.iter().map(|block| format!("D{}", block.height)));
            Ok(())
        }
        /// Same contract the real writers assert: non-finalized state contiguous from `applied`
        async fn apply(&mut self, block: &Arc<Chained>) -> Result<(), Self::Error> {
            assert_eq!(block.height.checked_sub(1), self.applied, "apply saw a gap");
            self.applied = Some(block.height);
            Ok(())
        }
        async fn finalize(
            &mut self,
            blocks: &[Arc<Chained>],
        ) -> Result<impl FnOnce() -> Result<Height, Self::Error> + Send + 'static, Self::Error>
        {
            assert_eq!(blocks[0].height.checked_sub(1), self.finalized, "finalize saw a gap");
            let reached = blocks[blocks.len() - 1].height;
            self.log.lock().expect("log").push(format!(
                "F{}..={}",
                blocks[0].height,
                blocks[blocks.len() - 1].height
            ));
            Ok(move || Ok(reached))
        }
        async fn committed(&mut self, reached: Height) -> Result<(), Self::Error> {
            self.finalized = Some(reached);
            self.applied = self.applied.max(Some(reached));
            Ok(())
        }
        async fn reset(&mut self) -> Result<(), Self::Error> {
            self.applied = self.finalized;
            Ok(())
        }
    }

    /// Durable through 9 on fork 0; the sink starts at 10 (this index rearmost) or 5 (another
    /// index durable through 4):
    /// - fork 0: 5 to 9 (both inclusive) reach only `deliver` (held already), 10 onward are staged
    ///   and written
    /// - fork 1 (a reorg below the window, a validator resynced elsewhere): fatal, both hashes
    ///   named; `Unlinked` when 10 comes first (its parent checked), `Diverged` when a replay
    ///   lands on 9 (its own hash checked); the failure cancels the pipeline, and the queue is
    ///   still popped through `Shutdown` (the sink never sees it dropped)
    #[tokio::test]
    async fn delivered_chain_must_match_the_durable_tip_when_extending_or_replaying_onto_it() {
        for (start, fork) in [(10u32, 0u8), (5, 0), (10, 1), (5, 1)] {
            let mut sink = IndexerDataSink::<Chained>::new("test");
            let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
            let durable = Some(h(9));
            let writer = Counting { applied: durable, finalized: durable, ..Default::default() };
            let log = Arc::clone(&writer.log);
            let (depth, subscription) = (ReorgDepth::CONSENSUS, sink.subscribe("counting", queue));
            let (_tips, tips_rx) = watch::channel(quorum(2_000));
            let follower = IndexFollower::new(writer, subscription, tips_rx, queue, depth);
            let _other = sink.subscribe("other", queue);
            let mut sink = Publisher::new(sink, depth, [durable, h(start).checked_sub(1)]);
            let shutdown = CancellationToken::new();
            let running = tokio::spawn(follower.run(shutdown.clone()));

            sink.set_tip(h(2_000)).await;
            for height in start..=12 {
                sink.add(h(height), on(fork, height)).await;
            }
            let case = format!("start {start}, fork {fork}");
            if fork == 1 {
                tokio::time::timeout(Duration::from_secs(5), shutdown.cancelled())
                    .await
                    .unwrap_or_else(|_| panic!("{case}: failure never cancelled the pipeline"));
            }
            sink.shutdown();
            let stopped = running.await.expect("joined");

            assert_eq!(shutdown.is_cancelled(), fork == 1, "{case}: cancelled iff failed");
            match fork {
                0 => {
                    stopped.unwrap_or_else(|error| panic!("{case}: {error}"));
                    let expected: Vec<String> = (start..=12)
                        .map(|height| format!("D{height}"))
                        .chain(["F10..=12".to_owned()])
                        .collect();
                    assert_eq!(*log.lock().expect("log"), expected, "{case}");
                }
                _ => {
                    use FollowError::{Diverged, Unlinked};
                    let seen = match stopped {
                        Err(Unlinked { index: "counting", height, expected, got }) => {
                            ("unlinked", height, expected, got)
                        }
                        Err(Diverged { index: "counting", height, expected, got }) => {
                            ("diverged", height, expected, got)
                        }
                        other => panic!("{case}: {other:?}"),
                    };
                    let (kind, at) =
                        if start == 10 { ("unlinked", h(10)) } else { ("diverged", h(9)) };
                    assert_eq!(seen, (kind, at, hash_of(9, 0), hash_of(9, 1)), "{case}");
                }
            }
        }
    }

    /// Bulk, all final, batch of 100 bytes: a batch closes on staged weight, not a block count
    /// - ten 10-byte blocks share one commit; a 500-byte block commits alone
    /// - the partial remainder is written at stop
    #[tokio::test]
    async fn a_batch_closes_on_staged_bytes_so_light_blocks_share_a_commit_and_heavy_ones_do_not() {
        let mut sink = IndexerDataSink::<Chained>::new("test");
        let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
        let writer = Counting::default();
        let log = Arc::clone(&writer.log);
        let (_tips, tips_rx) = watch::channel(quorum(2_000));
        let follower = IndexFollower::new(
            writer,
            sink.subscribe("counting", queue),
            tips_rx,
            std::num::NonZeroUsize::new(100).expect("nz"),
            ReorgDepth::CONSENSUS,
        );
        let mut sink = Publisher::new(sink, ReorgDepth::CONSENSUS, [None]);
        let running = tokio::spawn(follower.run(CancellationToken::new()));

        sink.set_tip(h(2_000)).await;
        for (height, weight) in (0..=9).map(|height| (height, 10)).chain([(10, 500), (11, 10)]) {
            let block = Arc::new(Chained { height: h(height), fork: 0, weight });
            sink.add(h(height), block).await;
        }
        sink.shutdown();
        running.await.expect("joined").expect("clean stop");

        let commits: Vec<String> =
            log.lock().expect("log").iter().filter(|e| e.starts_with('F')).cloned().collect();
        assert_eq!(commits, ["F0..=9", "F10..=10", "F11..=11"]);
    }

    /// Batch of 1000 unit-weight blocks, depth 1000, 501 bulk blocks (not a batch multiple):
    /// - bulk→tip transition flushes the partial batch before the first `apply`
    /// - gate opens when the tip is applied, not when a batch fills
    /// - a buried non-finalized block is finalised by its `Finalized`, contiguously
    /// - a reset writes what is final (the finalised 501 too), replays only the non-final window,
    ///   and closes the gate until the replay is back at the tip
    #[tokio::test]
    async fn the_sync_gate_opens_at_the_tip_and_a_reset_closes_it_until_replayed() {
        let mut sink = IndexerDataSink::<Chained>::new("test");
        let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
        let (tips, tips_rx) = watch::channel(None);
        let follower = IndexFollower::new(
            Counting::default(),
            sink.subscribe("counting", queue),
            tips_rx,
            std::num::NonZeroUsize::new(1_000).expect("nz"),
            ReorgDepth::CONSENSUS,
        );
        let mut sink = Publisher::new(sink, ReorgDepth::CONSENSUS, [None]);
        let synced = follower.subscribe_synced();
        let finalized = follower.subscribe_finalized();
        let running = tokio::spawn(follower.run(CancellationToken::new()));
        let gate = |open: bool, when: &'static str| {
            let mut synced = synced.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), synced.wait_for(|s| *s == open))
                    .await
                    .unwrap_or_else(|_| panic!("gate never became {open} {when}"))
                    .expect("follower alive");
            }
        };

        tips.send_replace(quorum(1_500));
        sink.set_tip(h(1_500)).await;
        for height in 0..=1_500 {
            sink.add(h(height), on(0, height)).await;
        }
        gate(true, "after the tip (1500) was applied").await;

        tips.send_replace(quorum(1_501));
        sink.set_tip(h(1_501)).await;
        sink.add(h(1_501), on(0, 1_501)).await;
        let replay_from = sink.reorg().await;
        assert_eq!(replay_from, h(502), "first non-final height (1501 + 1 - 1000)");
        gate(false, "after a reorg dropped the non-finalized state").await;
        let durable = *finalized.borrow();
        assert_eq!(durable, Some(h(501)), "reset wrote final 0 to 501 (both inclusive) first");

        for height in 502..=1_501 {
            sink.add(h(height), on(0, height)).await;
        }
        gate(true, "after the non-final window was replayed").await;

        sink.shutdown();
        running.await.expect("joined").expect("clean stop");
        assert_eq!(*finalized.borrow(), Some(h(501)), "nothing newly final at stop");
    }

    /// Depth 3, batch of 1000 unit-weight blocks, following the tip block by block:
    /// - each final block durable as it arrives (a full batch would take hours at the tip)
    /// - gate never flaps as each new tip lands ahead of its block
    /// - quorum tip more than the depth ahead closes it, no block behind it (producer stalled)
    #[tokio::test]
    async fn following_the_tip_commits_each_final_block_and_never_flaps_the_gate() {
        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut sink = IndexerDataSink::<Chained>::new("test");
        let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
        let (tips, tips_rx) = watch::channel(None);
        let follower = IndexFollower::new(
            Counting::default(),
            sink.subscribe("counting", queue),
            tips_rx,
            std::num::NonZeroUsize::new(1_000).expect("nz"),
            depth,
        );
        let mut sink = Publisher::new(sink, depth, [None]);
        let mut synced = follower.subscribe_synced();
        let mut finalized = follower.subscribe_finalized();
        let served = follower.served();
        assert_eq!((served.pin(), *served.pin_any()), (None, None), "syncing at boot");
        let running = tokio::spawn(follower.run(CancellationToken::new()));
        let within = Duration::from_secs(5);

        tips.send_replace(quorum(20));
        sink.set_tip(h(20)).await;
        for height in 0..=20 {
            sink.add(h(height), on(0, height)).await;
        }
        tokio::time::timeout(within, finalized.wait_for(|f| *f == Some(h(17))))
            .await
            .expect("final 0..=17 durable at the tip, 18 blocks short of a batch")
            .expect("follower alive");
        tokio::time::timeout(within, synced.wait_for(|open| *open))
            .await
            .expect("gate opens at the tip")
            .expect("follower alive");
        synced.mark_unchanged();

        for height in 21..=30 {
            tips.send_replace(quorum(height));
            sink.set_tip(h(height)).await;
            sink.add(h(height), on(0, height)).await;
            tokio::time::timeout(within, finalized.wait_for(|f| *f == Some(h(height - 3))))
                .await
                .unwrap_or_else(|_| panic!("block {} durable once final", height - 3))
                .expect("follower alive");
        }
        assert!(!synced.has_changed().expect("follower alive"), "gate flapped at the tip");
        let applied = Some(h(30));
        assert_eq!(served.pin().as_deref(), Some(&applied), "served = the latest step's view");

        tips.send_replace(quorum(34));
        tokio::time::timeout(within, synced.wait_for(|open| !*open))
            .await
            .expect("more than the depth behind closes the gate")
            .expect("follower alive");
        assert_eq!((served.pin(), *served.pin_any()), (None, applied), "gated, view kept");

        sink.shutdown();
        running.await.expect("joined").expect("clean stop");
    }
}
