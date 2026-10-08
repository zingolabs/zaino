#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod core;
mod emit;
mod fetch;
mod fold;
mod graph;
mod progress;
mod snapshot;

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zaino_header_chain::VerifiedChain;
use zaino_persistence::{IndexKind, Layer, MapRead, SequenceRead};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height};
use zaino_source::ChainDataSource;
use zaino_sync::{compute, Final, Human, IndexerDataSink, PerIndex, Step, Subscription};
use zaino_traffic::TrafficBalancer;

use crate::core::{Diverged, Indexes, Input, NfsCore, Output, SnapshotTip};
use crate::fetch::{fetch, Checked};
use crate::fold::{fold_block, Folded};

pub use crate::emit::describe_metrics;
pub use crate::fold::{FoldError, INDEXES};
pub use crate::progress::NfsProgress;
pub use crate::snapshot::{At, Branch, ChainParams, Indexed, Published, Views};

#[derive(Debug, thiserror::Error)]
pub enum NfsError {
    #[error(
        "{index} committed {expected} at {height:?}, the verified chain has {got} (resync \
         required)"
    )]
    Diverged { index: &'static str, height: Height, expected: BlockHash, got: BlockHash },
    #[error(transparent)]
    Fold(#[from] FoldError),
    #[error("header sync stopped publishing the verified chain")]
    ChainGone,
    #[error("{0} writer stopped publishing its committed view")]
    WriterGone(&'static str),
}

/// `NfsCore` run against the balancer, real folds, writers and readers
/// - Inputs: the verified chain, checked bodies, fold results, each index's committed view, each
///   final step delivered
/// - Outputs: fetches (tasks), folds (compute pool), the final stream, [`Indexed`] publishes
pub struct Nfs<S, V> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: TrafficBalancer<S>,
    params: ChainParams,
    lookahead: NonZeroUsize,
    sink: IndexerDataSink<Final>,
    committed: PerIndex<watch::Receiver<V>>,
    root: PerIndex<Layer>,
    published: watch::Sender<Option<Arc<Indexed<V>>>>,
    progress: NfsProgress,
}

/// [`Nfs::run`]'s loop: everything but the sink, which delivers beside it (a full queue never
/// holds the loop)
///
/// - `steps` = final steps to deliver, in order (the core bounds them: `lookahead`)
/// - `served` = last published tip (reorgs and new tips logged against it)
struct Driver<S, V> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: TrafficBalancer<S>,
    params: ChainParams,
    lookahead: NonZeroUsize,
    committed: PerIndex<watch::Receiver<V>>,
    root: PerIndex<Layer>,
    published: watch::Sender<Option<Arc<Indexed<V>>>>,
    progress: NfsProgress,
    steps: mpsc::UnboundedSender<Step<Final>>,
    served: Option<BlockRef>,
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Nfs<S, V> {
    /// - `balancer` = who serves each body (each answer checked; its driver runs elsewhere)
    /// - `lookahead` = bodies fetched or folding ahead of the next one needed
    pub fn new(
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        balancer: TrafficBalancer<S>,
        params: ChainParams,
        lookahead: NonZeroUsize,
    ) -> Self {
        Self {
            chain,
            balancer,
            params,
            lookahead,
            sink: IndexerDataSink::new("final"),
            committed: PerIndex::default(),
            root: PerIndex::default(),
            published: watch::Sender::new(None),
            progress: NfsProgress::default(),
        }
    }

    /// `kind` enabled, its final stream out
    ///
    /// - `committed` = its store's committed view, sent after each commit (durable tip = its tip)
    /// - `queue` = the stream's byte budget ([`zaino_sync::Weight`])
    /// - panics: `kind` twice, `compact_block` before `value_balance` (its fees)
    pub fn subscribe(
        &mut self,
        kind: IndexKind,
        committed: watch::Receiver<V>,
        queue: NonZeroUsize,
    ) -> Subscription<Final> {
        let fees = self.root.get(IndexKind::ValueBalance).is_some();
        let ordered = kind != IndexKind::CompactBlock || fees;
        assert!(ordered, "compact_block folds on value_balance's fees: subscribe that first");
        self.root.insert(kind, Layer::empty(committed.borrow().schema()));
        self.committed.insert(kind, committed);
        self.sink.subscribe(kind.name(), queue)
    }

    /// Every publish as a watch: served tip moved or a durable tip moved (the global snapshot's
    /// input)
    pub fn indexed(&self) -> Published<V> {
        self.published.subscribe()
    }

    /// Blocks handed to the indexes (sync progress between their commits)
    pub fn progress(&self) -> NfsProgress {
        self.progress.clone()
    }

    /// - Cancel → `Ok`
    /// - Either way: the final stream ends with `Shutdown` (every writer commits, stops; a step
    ///   still undelivered delivers nowhere)
    pub async fn run(self, cancel: CancellationToken) -> Result<(), NfsError> {
        let Self { chain, balancer, params, lookahead, sink, committed, root, published, progress } =
            self;
        let (steps, queued) = mpsc::unbounded_channel();
        let (delivered, deliveries) = mpsc::unbounded_channel();
        let mut driver = Driver {
            chain,
            balancer,
            params,
            lookahead,
            committed,
            root,
            published,
            progress,
            steps,
            served: None,
        };
        let followed = cancel
            .run_until_cancelled(async {
                tokio::select! {
                    followed = driver.follow(deliveries) => followed,
                    never = deliver(&sink, queued, delivered) => match never {},
                }
            })
            .await;
        sink.shutdown();
        followed.unwrap_or(Ok(()))
    }
}

/// `queued` into the stream in order, each delivery reported (a full queue waits here alone)
async fn deliver(
    sink: &IndexerDataSink<Final>,
    mut queued: mpsc::UnboundedReceiver<Step<Final>>,
    delivered: mpsc::UnboundedSender<()>,
) -> Infallible {
    while let Some(step) = queued.recv().await {
        sink.send(step).await;
        if delivered.send(()).is_err() {
            break;
        }
    }
    std::future::pending().await
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Driver<S, V> {
    /// `deliveries` = one per final step delivered
    async fn follow(
        &mut self,
        mut deliveries: mpsc::UnboundedReceiver<()>,
    ) -> Result<(), NfsError> {
        let mut committed: PerIndex<V> = PerIndex::default();
        let mut commits = JoinSet::new();
        for (position, (kind, watch)) in self.committed.iter().enumerate() {
            let mut watch = watch.clone();
            committed.insert(kind, watch.borrow_and_update().clone());
            commits.spawn(next_commit(position, watch));
        }
        let durable = committed.iter().map(|(_, view)| view.tip()).collect();
        let mut core = NfsCore::new(self.lookahead.get(), durable, self.groups());
        let mut work = JoinSet::new();
        let mut fetches = Fetches::default();
        let mut input = self.chain.borrow_and_update().clone().map(Input::Chain);
        loop {
            if let Some(input) = input.take() {
                let outputs = core.step(input).map_err(|diverged| self.diverged(diverged))?;
                for output in outputs {
                    self.execute(output, &committed, &mut work, &mut fetches);
                }
                if cfg!(debug_assertions) {
                    core.check();
                }
            }
            input = tokio::select! {
                changed = self.chain.changed() => {
                    changed.map_err(|_| NfsError::ChainGone)?;
                    self.chain.borrow_and_update().clone().map(Input::Chain)
                }
                Some(done) = work.join_next() => Some(joined(done)?),
                body = fetches.next() => Some(Input::Body(body)),
                Some(()) = deliveries.recv() => Some(Input::Delivered),
                Some(commit) = commits.join_next() => {
                    let (index, mut watch, open) = joined(commit);
                    let (kind, view) = committed.at_mut(index);
                    if !open {
                        return Err(NfsError::WriterGone(kind.name()));
                    }
                    *view = watch.borrow_and_update().clone();
                    let tip = view.tip();
                    commits.spawn(next_commit(index, watch));
                    Some(Input::Durable { index, tip })
                }
            };
        }
    }

    /// `Send`s queued for [`deliver`] in order (backpressure: the core's `Delivered`); the rest
    /// spawned or immediate
    fn execute(
        &mut self,
        output: Output<Folded>,
        committed: &PerIndex<V>,
        work: &mut JoinSet<Result<Input<Folded>, FoldError>>,
        fetches: &mut Fetches,
    ) {
        match output {
            Output::Fetch { at, record, urgency } => {
                let fetch = fetch(self.balancer.clone(), at, record, urgency);
                fetches.start(at.hash, fetch);
            }
            Output::Abandon(at) => fetches.abandon(at.hash),
            Output::Fold { at, parent, block, covers } => {
                self.hand(&block);
                let layers = parent.as_deref().map_or(&self.root, |folded| &folded.layers);
                let parent = Views::new(committed, &self.covered(layers, covers));
                work.spawn(async move {
                    let folded = compute(move || fold_block(&parent, &block)).await?;
                    Ok(Input::Folded { at, folded: Arc::new(folded) })
                });
            }
            Output::Send(core::Final { block, folded }) => {
                if folded.is_none() {
                    self.hand(&block);
                }
                let height = block.header().height;
                let folds = folded.map(|folded| Arc::clone(&folded.folds));
                let data = Arc::new(Final { block, folds });
                let queued = self.steps.send(Step::Apply { height, data });
                queued.expect("deliver runs as long as the driver");
            }
            Output::Publish(SnapshotTip { chain, tip, root, graph, joined }) => {
                self.log_served(&chain, tip);
                let (durable, joined) = (committed.clone(), self.covered(&self.root, joined));
                let indexed = Indexed::new(chain, tip, root, self.params, durable, joined, graph);
                self.published.send_replace(Some(Arc::new(indexed)));
            }
        }
    }

    /// `block` handed to the indexes: folded, or sent unfolded (each block counted once per branch)
    fn hand(&self, block: &Block) {
        emit::handed(block);
        self.progress.hand(block.header().height);
    }

    /// Served tip moved: a reorg (the last one off `chain`'s best), or a new best tip (~75 s apart)
    ///
    /// - same tip (a durable-only republish): silent
    fn log_served(&mut self, chain: &VerifiedChain, tip: BlockRef) {
        let last = self.served.replace(tip);
        if last == Some(tip) {
            return;
        }
        let left = last.filter(|last| chain.hash_at(last.height) != Some(last.hash));
        if let Some(left) = left {
            emit::reorg();
            let (from, to) = (u32::from(left.height), u32::from(tip.height));
            warn!(from, to, "Chain reorg detected");
        }
        if tip != chain.best() {
            return;
        }
        let time = chain.header_at(tip.height).map_or(0, |record| record.time);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let age = Human(Duration::from_secs(now.saturating_sub(u64::from(time))));
        let finalized = chain.final_tip().map_or(0, |tip| u32::from(tip.height));
        let (height, hash) = (u32::from(tip.height), tip.hash);
        info!(height, %hash, %age, finalized, "Chain tip advanced");
    }

    /// `layers` of the indexes at `covers`' positions (subscribe order)
    ///
    /// - panics: one of them unfolded in `layers` (a fold parent narrower than its fold)
    fn covered(&self, layers: &PerIndex<Layer>, covers: Indexes) -> PerIndex<Layer> {
        let mut chosen = PerIndex::default();
        for (position, (kind, _)) in self.root.iter().enumerate() {
            if !covers.contains(position) {
                continue;
            }
            let layer = layers.get(kind).unwrap_or_else(|| panic!("{}: unfolded", kind.name()));
            chosen.insert(kind, layer.clone());
        }
        chosen
    }

    /// Positions joining together: value-balance + compact-block (one fee per step each folds
    /// itself, on both sides), every other index alone
    fn groups(&self) -> Vec<Indexes> {
        let mut groups: Vec<Indexes> = Vec::new();
        let mut fees: Option<usize> = None;
        for (position, (kind, _)) in self.committed.iter().enumerate() {
            let one = Indexes::one(position);
            match (kind, fees) {
                (IndexKind::CompactBlock, Some(at)) => groups[at] = groups[at].union(one),
                (IndexKind::ValueBalance, _) => {
                    fees = Some(groups.len());
                    groups.push(one);
                }
                _ => groups.push(one),
            }
        }
        groups
    }

    fn diverged(&self, diverged: Diverged) -> NfsError {
        let Diverged { index, height, expected, got } = diverged;
        let (kind, _) = self.committed.iter().nth(index).expect("a durable tip per index");
        NfsError::Diverged { index: kind.name(), height, expected, got }
    }
}

/// `index`'s next commit (`false` = its writer gone)
async fn next_commit<V>(
    index: usize,
    mut watch: watch::Receiver<V>,
) -> (usize, watch::Receiver<V>, bool) {
    let open = watch.changed().await.is_ok();
    (index, watch, open)
}

/// Fetches in flight, by block (`ids` = each block's live task: an abandoned task ending never
/// unlists its re-want's)
#[derive(Default)]
struct Fetches {
    tasks: JoinSet<Checked>,
    ids: HashMap<BlockHash, (Id, AbortHandle)>,
}

impl Fetches {
    fn start(&mut self, block: BlockHash, fetch: impl Future<Output = Checked> + Send + 'static) {
        let handle = self.tasks.spawn(fetch);
        self.ids.insert(block, (handle.id(), handle));
    }

    fn abandon(&mut self, block: BlockHash) {
        if let Some((_, handle)) = self.ids.remove(&block) {
            handle.abort();
        }
    }

    /// Next checked body (an abandoned fetch's end skipped; none in flight = pending)
    async fn next(&mut self) -> Checked {
        loop {
            let Some(done) = self.tasks.join_next_with_id().await else {
                return std::future::pending().await;
            };
            match done {
                Ok((id, body)) => {
                    let hash = body.at().hash;
                    if self.ids.get(&hash).is_some_and(|(held, _)| *held == id) {
                        self.ids.remove(&hash);
                    }
                    return body;
                }
                Err(join) if join.is_cancelled() => {}
                Err(join) => std::panic::resume_unwind(join.into_panic()),
            }
        }
    }
}

/// Task's value; its panic resumed here (a fold or fetch never half-done)
fn joined<T>(done: Result<T, JoinError>) -> T {
    match done {
        Ok(value) => value,
        Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
        Err(join) => panic!("NFS task cancelled: {join}"),
    }
}

/// Panic message of `run` (`None` = it returned): fire drills
#[cfg(test)]
fn fired(run: impl FnOnce()) -> Option<String> {
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err()?;
    let message = panic.downcast_ref::<String>().cloned();
    message.or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
}

#[cfg(test)]
mod tests;
