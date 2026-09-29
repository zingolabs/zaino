//! Drives one [`IndexWriter`] from its [`Subscription`]: `tokio::spawn(follower.run())`
//!
//! - Final `Apply` → staged for `finalize` (bulk path: one fold per block); non-final → `apply`
//!   into pre-commit, staged later when its `Finalized` arrives
//! - Stops when the subscription closes (producer dropped): drains, writes what is final, returns

use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc};

use tokio::sync::watch;
use tracing::{debug, info};
use zaino_primitives::types::{BlockHash, Extent, Height};

use crate::data_sink::Woke;
use crate::{IndexWriter, Linked, Served, Step, Subscription, Weight};

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
pub struct IndexFollower<W: IndexWriter> {
    writer: W,
    subscription: Subscription<W::Input>,
    batch_bytes: NonZeroUsize,
    /// Durable extent, published after each fsync
    finalized: watch::Sender<Extent>,
    /// Pre-commit state, published after each step (what serving pins)
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

impl<W: IndexWriter> IndexFollower<W> {
    /// `batch_bytes` = staged [`Weight`] that triggers a `finalize` (one fsync); past bulk, each
    /// final block commits as it arrives regardless
    pub fn new(writer: W, subscription: Subscription<W::Input>, batch_bytes: NonZeroUsize) -> Self {
        let durable = writer.finalized_height();
        assert_eq!(writer.applied_height(), durable, "{}: pre-commit at boot", W::NAME);
        let (has_tip, non_empty) = (writer.finalized_tip().is_some(), durable.last().is_some());
        assert_eq!(has_tip, non_empty, "{}: durable tip hash iff extent non-empty", W::NAME);
        Self {
            finalized: watch::Sender::new(durable),
            view: Arc::new(arc_swap::ArcSwap::from_pointee(writer.view())),
            synced: watch::Sender::new(false),
            linked: None,
            in_flight: None,
            writer,
            subscription,
            batch_bytes,
        }
    }

    /// `data` must extend the last delivered block, and match the durable tip where it lands on it
    ///
    /// - First block: its parent checkable only when it extends the durable tip (a replay starts
    ///   below, where this index keeps no hashes)
    fn link(&mut self, height: Height, data: &W::Input) -> Result<(), FollowError<W::Error>> {
        assert_eq!(data.height(), height, "{}: step height != block height", W::NAME);
        let durable = self.writer.finalized_height();
        let parent = match self.linked {
            None if height == durable.next() => self.writer.finalized_tip(),
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
        if durable.last() == Some(height) {
            let expected = self
                .writer
                .finalized_tip()
                .expect("durable tip hash present iff the extent is non-empty");
            if data.hash() != expected {
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

    /// The writer, for handles it hands out (e.g. a service over its store) before `run`
    pub fn writer(&self) -> &W {
        &self.writer
    }

    pub fn subscribe_finalized(&self) -> watch::Receiver<Extent> {
        self.finalized.subscribe()
    }

    pub fn subscribe_synced(&self) -> watch::Receiver<bool> {
        self.synced.subscribe()
    }

    /// What this index's services read: the view published after every step and commit, gated
    /// on `synced`
    pub fn served(&self) -> Served<W::View> {
        Served::new(Arc::clone(&self.view), self.synced.subscribe())
    }

    pub async fn run(mut self) -> Result<(), FollowError<W::Error>> {
        let fail = |source| FollowError::Index { index: W::NAME, source };
        // applied, not yet final (oldest first)
        let mut window: VecDeque<Arc<W::Input>> = VecDeque::new();
        // final, not yet fsynced (one `finalize` per `batch_bytes`)
        let mut staged: Staged<W::Input> = Staged::default();
        // delivered so far (= durable + staged + window, once past any replay below durable);
        // `None` until the first block fixes where the sink started
        let mut delivered: Option<Extent> = None;
        // non-`Apply` step popped while gathering a run (handled next, keeping step order)
        let mut held: Option<Step<W::Input>> = None;

        loop {
            let step = match held.take() {
                Some(step) => step,
                None => match self.next_landing_writes().await.map_err(fail)? {
                    Some(Woke::Step(step)) => step,
                    Some(Woke::Tip) => {
                        if self.subscription.steps.is_empty() {
                            self.idle(&mut staged, delivered).await.map_err(fail)?;
                        }
                        continue;
                    }
                    None => break,
                },
            };
            match step {
                Step::Apply { height, finalized, data } => {
                    // run = this block + every `Apply` already queued, to one batch's bytes
                    let mut run = vec![(height, finalized, data)];
                    let mut bytes = run[0].2.weight();
                    while bytes < self.batch_bytes.get() {
                        match self.subscription.try_next() {
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
                        let in_reach = delivered.is_some() || *height <= durable.next();
                        assert!(in_reach, "{}: sink started above the durable extent", W::NAME);
                        let from = *delivered.get_or_insert(Extent::before(*height));
                        assert_eq!(
                            *height,
                            from.next(),
                            "{}: sink delivered out of order",
                            W::NAME
                        );
                        self.link(*height, data)?;
                        delivered = Some(Extent::through(*height));
                        durable_at.push(durable);
                    }
                    let blocks: Vec<Arc<W::Input>> =
                        run.iter().map(|(_, _, data)| Arc::clone(data)).collect();
                    self.writer.deliver(&blocks).await.map_err(fail)?;

                    for ((height, finalized, data), durable) in run.into_iter().zip(durable_at) {
                        if durable.contains(height) {
                            // replay below this index's own durable tip: already held
                            assert!(finalized, "{}: durable height not final", W::NAME);
                        } else if finalized {
                            assert!(window.is_empty(), "{}: final block above pre-commit", W::NAME);
                            staged.push(data);
                        } else {
                            // pre-commit starts where durable ends (bulk→tip: staged written first)
                            if self.writer.applied_height() < Extent::before(height) {
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
                    let delivered = delivered.expect("Finalized follows its Apply");
                    let first = first_unfinal(delivered, &window);
                    assert_eq!(height, first, "{}: finalized out of order", W::NAME);
                    let Some(data) = window.pop_front() else {
                        panic!("{}: Finalized with no pre-commit block", W::NAME)
                    };
                    staged.push(data);
                }
                Step::Reset => {
                    // final is final: write it, drop only pre-commit → replay from first non-final
                    let replay_from = Extent::before(first_unfinal(
                        delivered.expect("Reset follows an Apply"),
                        &window,
                    ));
                    self.drain(&mut staged).await.map_err(fail)?;
                    self.writer.reset().await.map_err(fail)?;
                    window.clear();
                    delivered = Some(replay_from);
                    assert_eq!(self.writer.finalized_height(), replay_from, "{}: durable", W::NAME);
                    assert_eq!(self.writer.applied_height(), replay_from, "{}: applied", W::NAME);
                    // replay links onto the last final block, now durable
                    self.linked = self.writer.finalized_tip();
                    self.set_synced(false);
                }
            }

            self.view.store(Arc::new(self.writer.view()));

            if staged.bytes >= self.batch_bytes.get() {
                self.flush(&mut staged).await.map_err(fail)?;
            }
            // on idle, not per step (a queued burst cannot flap the gate or split a batch)
            if held.is_none() && self.subscription.steps.is_empty() {
                self.idle(&mut staged, delivered).await.map_err(fail)?;
            }
        }

        self.drain(&mut staged).await.map_err(fail)
    }

    /// Queue empty: final blocks durable once at the tip, and the serving gate set
    async fn idle(
        &mut self,
        staged: &mut Staged<W::Input>,
        delivered: Option<Extent>,
    ) -> Result<(), W::Error> {
        let tip = *self.subscription.tip.borrow_and_update();
        let reached = tip.is_some_and(|tip| delivered.is_some_and(|d| d.contains(tip)));
        // tip itself final (retreat onto the final boundary) → only the staged write puts it in
        // the view
        if reached && !tip.is_some_and(|tip| self.writer.applied_height().contains(tip)) {
            self.drain(staged).await?;
        }
        let applied = self.writer.applied_height();
        let at_tip = tip.is_some_and(|tip| applied.contains(tip));
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
                u32::from(tip).saturating_sub(applied.last().map_or(0, u32::from))
                    <= self.subscription.depth.get()
            }),
        });
        Ok(())
    }

    /// Next step, or a tip moved with no step (a lowered tip sends none); a write finishing
    /// first is landed meanwhile (durability published as it happens, not at the next block)
    async fn next_landing_writes(&mut self) -> Result<Option<Woke<W::Input>>, W::Error> {
        loop {
            let Some(in_flight) = self.in_flight.as_mut() else {
                return Ok(self.subscription.next_or_tip().await);
            };
            let done = tokio::select! {
                woke = self.subscription.next_or_tip() => return Ok(woke),
                done = &mut in_flight.write => done,
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
            height = %finalized.last().map_or(0, u32::from),
            blocks = in_flight.blocks,
            bytes = in_flight.bytes,
            elapsed = ?in_flight.started.elapsed(),
            "Committed batch"
        );
        self.view.store(Arc::new(self.writer.view()));
        let before = self.finalized.send_replace(finalized);
        assert!(before <= finalized, "{}: durable extent moved back", W::NAME);
        Ok(())
    }

    fn set_synced(&self, serving: bool) {
        let changed = self.synced.send_if_modified(|current| {
            let changed = *current != serving;
            *current = serving;
            changed
        });
        if changed {
            let height = self.writer.applied_height().last().map_or(0, u32::from);
            match serving {
                true => info!(%height, "Serving"),
                false => info!(%height, "Syncing, requests refused"),
            }
        }
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

/// Oldest pre-commit height (`delivered.next()` when the window is empty)
fn first_unfinal<T>(delivered: Extent, window: &VecDeque<T>) -> Height {
    let pending = u32::try_from(window.len()).expect("pre-commit window within the reorg depth");
    delivered.next().checked_sub(pending).expect("pre-commit window within the delivered run")
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroU32, time::Duration};

    use zaino_primitives::types::ReorgDepth;

    use super::*;
    use crate::SinkBuilder;

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
    #[derive(Default)]
    struct Counting {
        applied: Extent,
        finalized: Extent,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl IndexWriter for Counting {
        type Input = Chained;
        type View = Extent;
        type Error = std::convert::Infallible;
        type Done = Extent;
        const NAME: &'static str = "counting";

        fn finalized_height(&self) -> Extent {
            self.finalized
        }
        fn finalized_tip(&self) -> Option<BlockHash> {
            self.finalized.last().map(|tip| hash_of(u32::from(tip), 0))
        }
        fn applied_height(&self) -> Extent {
            self.applied
        }
        fn view(&self) -> Extent {
            self.applied
        }
        async fn deliver(&mut self, blocks: &[Arc<Chained>]) -> Result<(), Self::Error> {
            let mut log = self.log.lock().expect("log");
            log.extend(blocks.iter().map(|block| format!("D{}", block.height)));
            Ok(())
        }
        /// Same contract the real writers assert: pre-commit contiguous from `applied`
        async fn apply(&mut self, block: &Arc<Chained>) -> Result<(), Self::Error> {
            assert_eq!(block.height, self.applied.next(), "apply saw a gap");
            self.applied = Extent::through(block.height);
            Ok(())
        }
        async fn finalize(
            &mut self,
            blocks: &[Arc<Chained>],
        ) -> Result<impl FnOnce() -> Result<Extent, Self::Error> + Send + 'static, Self::Error>
        {
            assert_eq!(blocks[0].height, self.finalized.next(), "finalize saw a gap");
            let reached = Extent::through(blocks[blocks.len() - 1].height);
            self.log.lock().expect("log").push(format!(
                "F{}..={}",
                blocks[0].height,
                blocks[blocks.len() - 1].height
            ));
            Ok(move || Ok(reached))
        }
        async fn committed(&mut self, reached: Extent) -> Result<(), Self::Error> {
            self.finalized = reached;
            self.applied = self.applied.max(reached);
            Ok(())
        }
        async fn reset(&mut self) -> Result<(), Self::Error> {
            self.applied = self.finalized;
            Ok(())
        }
    }

    /// Durable through 9 on fork 0; the sink starts at 10 (this index rearmost) or 5 (another
    /// index durable through 4):
    /// - fork 0: 5..=9 reach only `deliver` (held already), 10.. are staged and written
    /// - fork 1 (a reorg below the window, a validator resynced elsewhere): fatal, both hashes
    ///   named; `Unlinked` when 10 comes first (its parent checked), `Diverged` when a replay
    ///   lands on 9 (its own hash checked)
    #[tokio::test]
    async fn delivered_chain_must_match_the_durable_tip_when_extending_or_replaying_onto_it() {
        for (start, fork) in [(10u32, 0u8), (5, 0), (10, 1), (5, 1)] {
            let mut builder = SinkBuilder::<Chained>::new(ReorgDepth::CONSENSUS);
            let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
            let durable = Extent::through(h(9));
            let writer = Counting { applied: durable, finalized: durable, ..Default::default() };
            let log = Arc::clone(&writer.log);
            let follower =
                IndexFollower::new(writer, builder.subscribe("counting", queue, durable), queue);
            let _other = builder.subscribe("other", queue, Extent::before(h(start)));
            let mut sink = builder.seal();
            let running = tokio::spawn(follower.run());

            sink.set_tip(h(2_000)).await.expect("tip");
            for height in start..=12 {
                if sink.add(h(height), on(fork, height)).await.is_err() {
                    break;
                }
            }
            drop(sink);
            let stopped = running.await.expect("joined");

            let case = format!("start {start}, fork {fork}");
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
        let mut builder = SinkBuilder::<Chained>::new(ReorgDepth::CONSENSUS);
        let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
        let writer = Counting::default();
        let log = Arc::clone(&writer.log);
        let follower = IndexFollower::new(
            writer,
            builder.subscribe("counting", queue, Extent::ZERO),
            std::num::NonZeroUsize::new(100).expect("nz"),
        );
        let mut sink = builder.seal();
        let running = tokio::spawn(follower.run());

        sink.set_tip(h(2_000)).await.expect("tip");
        for (height, weight) in (0..=9).map(|height| (height, 10)).chain([(10, 500), (11, 10)]) {
            let block = Arc::new(Chained { height: h(height), fork: 0, weight });
            sink.add(h(height), block).await.expect("queued");
        }
        drop(sink);
        running.await.expect("joined").expect("clean stop");

        let commits: Vec<String> =
            log.lock().expect("log").iter().filter(|e| e.starts_with('F')).cloned().collect();
        assert_eq!(commits, ["F0..=9", "F10..=10", "F11..=11"]);
    }

    /// Batch of 1000 unit-weight blocks, depth 1000, 501 bulk blocks (not a batch multiple):
    /// - bulk→tip transition flushes the partial batch before the first `apply`
    /// - gate opens when the tip is applied, not when a batch fills
    /// - a buried pre-commit block is finalised by its `Finalized`, contiguously
    /// - a reset writes what is final (the finalised 501 too), replays only the non-final window,
    ///   and closes the gate until the replay is back at the tip
    #[tokio::test]
    async fn the_sync_gate_opens_at_the_tip_and_a_reset_closes_it_until_replayed() {
        let mut builder = SinkBuilder::<Chained>::new(ReorgDepth::CONSENSUS);
        let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
        let follower = IndexFollower::new(
            Counting::default(),
            builder.subscribe("counting", queue, Extent::ZERO),
            std::num::NonZeroUsize::new(1_000).expect("nz"),
        );
        let mut sink = builder.seal();
        let synced = follower.subscribe_synced();
        let finalized = follower.subscribe_finalized();
        let running = tokio::spawn(follower.run());
        let gate = |open: bool, when: &'static str| {
            let mut synced = synced.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), synced.wait_for(|s| *s == open))
                    .await
                    .unwrap_or_else(|_| panic!("gate never became {open} {when}"))
                    .expect("follower alive");
            }
        };

        sink.set_tip(h(1_500)).await.expect("tip");
        for height in 0..=1_500 {
            sink.add(h(height), on(0, height)).await.expect("queued");
        }
        gate(true, "after the tip (1500) was applied").await;

        sink.set_tip(h(1_501)).await.expect("buries 501");
        sink.add(h(1_501), on(0, 1_501)).await.expect("queued");
        let replay_from = sink.reset().await.expect("queued");
        assert_eq!(replay_from, h(502), "first non-final height (1501 + 1 - 1000)");
        gate(false, "after a reset dropped pre-commit").await;
        let durable = *finalized.borrow();
        assert_eq!(durable, Extent::before(h(502)), "reset wrote final 0..=501 first");

        for height in 502..=1_501 {
            sink.add(h(height), on(0, height)).await.expect("queued");
        }
        gate(true, "after the non-final window was replayed").await;

        drop(sink);
        running.await.expect("joined").expect("clean stop");
        assert_eq!(*finalized.borrow(), Extent::before(h(502)), "nothing newly final at stop");
    }

    /// Depth 3, batch of 1000 unit-weight blocks, following the tip block by block:
    /// - each final block durable as it arrives (a full batch would take hours at the tip)
    /// - gate never flaps as each new tip lands ahead of its block
    /// - falling more than the depth behind closes it
    #[tokio::test]
    async fn following_the_tip_commits_each_final_block_and_never_flaps_the_gate() {
        let depth = ReorgDepth::new(NonZeroU32::new(3).expect("non-zero"));
        let mut builder = SinkBuilder::<Chained>::new(depth);
        let queue = std::num::NonZeroUsize::new(1 << 20).expect("nz");
        let follower = IndexFollower::new(
            Counting::default(),
            builder.subscribe("counting", queue, Extent::ZERO),
            std::num::NonZeroUsize::new(1_000).expect("nz"),
        );
        let mut sink = builder.seal();
        let mut synced = follower.subscribe_synced();
        let mut finalized = follower.subscribe_finalized();
        let served = follower.served();
        assert_eq!((served.pin(), *served.pin_any()), (None, Extent::ZERO), "syncing at boot");
        let running = tokio::spawn(follower.run());
        let within = Duration::from_secs(5);

        sink.set_tip(h(20)).await.expect("tip");
        for height in 0..=20 {
            sink.add(h(height), on(0, height)).await.expect("queued");
        }
        tokio::time::timeout(within, finalized.wait_for(|f| *f == Extent::through(h(17))))
            .await
            .expect("final 0..=17 durable at the tip, 18 blocks short of a batch")
            .expect("follower alive");
        tokio::time::timeout(within, synced.wait_for(|open| *open))
            .await
            .expect("gate opens at the tip")
            .expect("follower alive");
        synced.mark_unchanged();

        for height in 21..=30 {
            sink.set_tip(h(height)).await.expect("tip");
            sink.add(h(height), on(0, height)).await.expect("queued");
            tokio::time::timeout(
                within,
                finalized.wait_for(|f| *f == Extent::through(h(height - 3))),
            )
            .await
            .unwrap_or_else(|_| panic!("block {} durable once final", height - 3))
            .expect("follower alive");
        }
        assert!(!synced.has_changed().expect("follower alive"), "gate flapped at the tip");
        let applied = Extent::through(h(30));
        assert_eq!(served.pin().as_deref(), Some(&applied), "served = the latest step's view");

        sink.set_tip(h(34)).await.expect("tip four ahead, nothing delivered");
        tokio::time::timeout(within, synced.wait_for(|open| !*open))
            .await
            .expect("more than the depth behind closes the gate")
            .expect("follower alive");
        assert_eq!((served.pin(), *served.pin_any()), (None, applied), "gated, view kept");

        drop(sink);
        running.await.expect("joined").expect("clean stop");
    }
}
