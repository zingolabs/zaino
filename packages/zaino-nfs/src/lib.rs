#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod core;
mod emit;
mod fetch;
mod fold;
mod graph;
mod report;
mod snapshot;

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use zaino_header_chain::{Record, VerifiedChain};
use zaino_persistence::{IndexKind, Layer, MapRead, SequenceRead};
use zaino_primitives::types::{Block, BlockHash, BlockRef, Height, ReorgDepth};
use zaino_source::ChainDataSource;
use zaino_sync::{compute, Final, Human, IndexerDataSink, Step, Subscription};

use crate::core::{Diverged, Input, NfsCore, Output, SnapshotTip};
use crate::fetch::{check_block, Answer};
use crate::fold::{fold_block, schema, Folded};
use crate::report::Progress;
use crate::snapshot::{PerIndex, Publisher};

pub use crate::emit::describe_metrics;
pub use crate::fold::FoldError;
pub use crate::snapshot::{ChainParams, NfsHandle, Snapshot, Views};

/// Core re-asked at this pace (retries, hedges)
const TICK: Duration = Duration::from_secs(1);

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

/// [`NfsCore`] run against real sources, folds, writers and readers
///
/// - Inputs: the verified chain, fetched bodies, fold results, each index's committed view
/// - Outputs: fetches (tasks), folds (compute pool), the final stream, [`Snapshot`]s
/// - `handed` = last block handed to the indexes (folded, or sent unfolded); `served` = last
///   published tip (reorgs and new tips logged against it)
pub struct Nfs<S, V> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    sources: Vec<Arc<S>>,
    params: ChainParams,
    lookahead: NonZeroUsize,
    depth: ReorgDepth,
    sink: IndexerDataSink<Final>,
    committed: PerIndex<watch::Receiver<V>>,
    root: PerIndex<Layer>,
    published: Publisher<V>,
    handed: watch::Sender<Option<Height>>,
    progress: Arc<Progress>,
    served: Option<BlockRef>,
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Nfs<S, V> {
    /// - `sources` = everything serving blocks by hash (each answer checked)
    /// - `lookahead` = bodies fetched or folding ahead of the next one needed
    /// - `depth` = the header chain's reorg depth (side-node bound)
    pub fn new(
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        sources: Vec<Arc<S>>,
        params: ChainParams,
        lookahead: NonZeroUsize,
        depth: ReorgDepth,
    ) -> Self {
        Self {
            chain,
            sources,
            params,
            lookahead,
            depth,
            sink: IndexerDataSink::new("final"),
            committed: PerIndex::default(),
            root: PerIndex::default(),
            published: Publisher::new(),
            handed: watch::Sender::new(None),
            progress: Arc::default(),
            served: None,
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
        self.root.insert(kind, Layer::empty(&schema(kind, self.params.network)));
        self.committed.insert(kind, committed);
        self.sink.subscribe(kind.name(), queue)
    }

    pub fn handle(&self) -> NfsHandle<V> {
        self.published.handle()
    }

    /// Last height handed to the indexes (sync progress between their commits)
    pub fn subscribe_handed(&self) -> watch::Receiver<Option<Height>> {
        self.handed.subscribe()
    }

    /// - Cancel → `Ok`
    /// - Either way: the final stream ends with `Shutdown` (every writer commits, stops)
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), NfsError> {
        let report = report::run(Arc::clone(&self.progress));
        let follow = async {
            tokio::select! {
                followed = self.follow() => followed,
                () = report => unreachable!("the reporter loops until dropped"),
            }
        };
        let followed = cancel.run_until_cancelled(follow).await;
        self.sink.shutdown();
        followed.unwrap_or(Ok(()))
    }

    async fn follow(&mut self) -> Result<(), NfsError> {
        let mut committed: PerIndex<V> = PerIndex::default();
        let mut commits = JoinSet::new();
        for (position, (kind, watch)) in self.committed.iter().enumerate() {
            let mut watch = watch.clone();
            committed.insert(kind, watch.borrow_and_update().clone());
            commits.spawn(next_commit(position, watch));
        }
        let durable = committed.iter().map(|(_, view)| view.tip()).collect();
        let (sources, lookahead) = (self.sources.len(), self.lookahead.get());
        let mut core = NfsCore::new(sources, lookahead, self.depth, durable);
        let mut work = JoinSet::new();
        let mut ticks = tokio::time::interval(TICK);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut input = self.chain.borrow_and_update().clone().map_or(Input::Tick, Input::Chain);
        loop {
            if let Input::Chain(chain) = &input {
                emit::best(chain.best().height);
                self.progress.target(chain.best().height);
            }
            let now = tokio::time::Instant::now().into_std();
            let outputs = core.step(input, now).map_err(|diverged| self.diverged(diverged))?;
            for output in outputs {
                self.execute(output, &committed, &mut work).await;
            }
            if cfg!(debug_assertions) {
                core.check();
            }
            input = tokio::select! {
                changed = self.chain.changed() => {
                    changed.map_err(|_| NfsError::ChainGone)?;
                    self.chain.borrow_and_update().clone().map_or(Input::Tick, Input::Chain)
                }
                Some(done) = work.join_next() => joined(done)?,
                Some(commit) = commits.join_next() => {
                    let (index, mut watch, open) = joined(commit);
                    let (kind, view) = committed.at_mut(index);
                    if !open {
                        return Err(NfsError::WriterGone(kind.name()));
                    }
                    *view = watch.borrow_and_update().clone();
                    let tip = view.tip();
                    commits.spawn(next_commit(index, watch));
                    Input::Durable { index, tip }
                }
                _ = ticks.tick() => Input::Tick,
            };
        }
    }

    /// `Send`s awaited in order (backpressure); the rest spawned or immediate
    async fn execute(
        &mut self,
        output: Output<Folded>,
        committed: &PerIndex<V>,
        work: &mut JoinSet<Result<Input<Folded>, FoldError>>,
    ) {
        match output {
            Output::Fetch { from, height, record } => {
                let source = Arc::clone(&self.sources[from]);
                work.spawn(async move { Ok(fetch(source, from, height, record).await) });
            }
            Output::Fold { at, parent, block } => {
                self.hand(&block);
                let parent = self.views(committed, parent.as_deref());
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
                self.sink.send(Step::Apply { height, data }).await;
            }
            Output::Publish(SnapshotTip { chain, tip, folded }) => {
                self.log_served(&chain, tip);
                let views = self.views(committed, folded.as_deref());
                self.published.publish(Snapshot { chain, tip, params: self.params, views });
            }
            Output::Misanswered { from, at, why } => {
                let height = u32::from(at.height);
                warn!(source = from, height, %why, "Source misanswered a block, asking another");
            }
            Output::Unserved { height } => {
                debug!(height = u32::from(height), "No source served the block, retrying");
            }
        }
    }

    /// `block` handed to the indexes: folded, or sent unfolded (each block counted once per branch)
    fn hand(&self, block: &Block) {
        let height = block.header().height;
        emit::handed(block);
        self.progress.handed(height);
        self.handed.send_replace(Some(height));
    }

    /// Served tip moved: a reorg (the last one off `chain`'s best), or a new best tip (~75 s apart)
    fn log_served(&mut self, chain: &VerifiedChain, tip: BlockRef) {
        let last = self.served.replace(tip);
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

    /// `folded` = `None`: the root (committed views alone)
    fn views(&self, committed: &PerIndex<V>, folded: Option<&Folded>) -> Views<V> {
        let layers = folded.map_or(&self.root, |folded| &folded.layers);
        Views::new(self.params.network, committed, layers)
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

/// `getblock <hash> 0` off `source`, checked against `record`
async fn fetch<S: ChainDataSource>(
    source: Arc<S>,
    from: usize,
    height: Height,
    record: Record,
) -> Input<Folded> {
    let answer = match source.get_block_by_hash(record.hash).await {
        Ok(block) => match check_block(block, height, &record) {
            Ok(checked) => Answer::Checked(checked),
            Err(why) => Answer::Misanswered(why),
        },
        Err(error) => {
            debug!(source = from, height = u32::from(height), %error, "Block fetch failed");
            Answer::Failed
        }
    };
    Input::Body { from, at: BlockRef { hash: record.hash, height }, answer }
}

/// A task's value; its panic resumed here (a fold or fetch never half-done)
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
