#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod core;
mod emit;
mod fold;
mod graph;
mod snapshot;

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use zaino_header_chain::VerifiedChain;
use zaino_persistence::{IndexKind, MapRead, SequenceRead};
use zaino_primitives::types::{BlockHash, BlockRef, ReorgDepth};
use zaino_source::ChainDataSource;
use zaino_sync::{compute, fetch, Checked, Human, IndexHandle, PerIndex};
use zaino_traffic::{TrafficBalancer, Urgency};

use crate::core::{Indexes, Input, NfsCore, Output, SnapshotTip};
use crate::fold::{fold_block, Folded};

pub use crate::emit::describe_metrics;
pub use crate::fold::{FoldError, INDEXES};
pub use crate::snapshot::{At, Branch, ChainParams, Indexed, Published, Views};

#[derive(Debug, thiserror::Error)]
pub enum NfsError {
    #[error(transparent)]
    Fold(#[from] FoldError),
    #[error("header sync stopped publishing the verified chain")]
    ChainGone,
    #[error("{0} writer stopped publishing its committed view")]
    IndexGone(&'static str),
}

/// `NfsCore` run against the balancer, real folds and each index's handle
///
/// - Inputs: the verified chain, checked bodies, fold results, each index's commits
/// - Outputs: fetches (tasks), folds (compute pool), [`Indexed`] publishes
/// - `window` = `2 · depth`: the NFS folds while the lowest durable tip is that close to best
pub struct Nfs<S, V> {
    chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
    balancer: TrafficBalancer<S>,
    params: ChainParams,
    window: u32,
    lookahead: NonZeroUsize,
    indexes: PerIndex<IndexHandle<V>>,
    committed: PerIndex<V>,
    published: watch::Sender<Option<Arc<Indexed<V>>>>,
    served: Option<BlockRef>,
}

impl<S: ChainDataSource, V: SequenceRead + MapRead> Nfs<S, V> {
    /// - `balancer` = who serves each body (each answer checked; its driver runs elsewhere)
    /// - `lookahead` = bodies fetched or folding ahead of the next fold
    pub fn new(
        chain: watch::Receiver<Option<Arc<VerifiedChain>>>,
        balancer: TrafficBalancer<S>,
        params: ChainParams,
        depth: ReorgDepth,
        lookahead: NonZeroUsize,
    ) -> Self {
        Self {
            chain,
            balancer,
            params,
            window: depth.get().saturating_mul(2),
            lookahead,
            indexes: PerIndex::default(),
            committed: PerIndex::default(),
            published: watch::Sender::new(None),
            served: None,
        }
    }

    /// `kind` enabled: its committed view read and extended at the tip (panics: `kind` twice)
    pub fn add(&mut self, kind: IndexKind, index: IndexHandle<V>) {
        self.indexes.insert(kind, index);
    }

    /// Every publish as a watch: served tip moved or a durable tip moved (the global snapshot's
    /// input)
    pub fn indexed(&self) -> Published<V> {
        self.published.subscribe()
    }

    /// Cancel → `Ok`
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), NfsError> {
        cancel.run_until_cancelled(self.follow()).await.unwrap_or(Ok(()))
    }

    async fn follow(&mut self) -> Result<(), NfsError> {
        let positions: Vec<IndexHandle<V>> =
            self.indexes.iter().map(|(_, handle)| handle.clone()).collect();
        let mut commits = JoinSet::new();
        for (position, handle) in positions.into_iter().enumerate() {
            commits.spawn(next_commit(position, handle));
        }
        let count = self.indexes.iter().count();
        let mut core = NfsCore::new(self.lookahead.get(), count, self.window);
        let mut work = JoinSet::new();
        let mut fetches = Fetches::default();
        let chain = self.chain.borrow_and_update().clone();
        let mut inputs: Vec<Input<Folded>> = chain.map(Input::Chain).into_iter().collect();
        inputs.push(Input::Durable(self.refresh()));
        loop {
            for input in inputs.drain(..) {
                for output in core.step(input) {
                    self.execute(output, &mut work, &mut fetches);
                }
                if cfg!(debug_assertions) {
                    core.check();
                }
            }
            tokio::select! {
                changed = self.chain.changed() => {
                    changed.map_err(|_| NfsError::ChainGone)?;
                    let chain = self.chain.borrow_and_update().clone();
                    inputs.extend(chain.map(Input::Chain));
                }
                Some(done) = work.join_next() => inputs.push(joined(done)?),
                body = fetches.next() => inputs.push(Input::Body(body)),
                Some(commit) = commits.join_next() => {
                    let (position, handle, open) = joined(commit);
                    if !open {
                        let (kind, _) = self.indexes.at_mut(position);
                        return Err(NfsError::IndexGone(kind.name()));
                    }
                    commits.spawn(next_commit(position, handle));
                    inputs.push(Input::Durable(self.refresh()));
                }
            }
        }
    }

    /// Each index's committed view reloaded into `committed`; their durable tips
    ///
    /// - `committed` changes only here: every fold and snapshot pairs layers with exactly the
    ///   durable tips the core was last told
    fn refresh(&mut self) -> Vec<Option<BlockRef>> {
        let (mut committed, mut tips) = (PerIndex::default(), Vec::new());
        for (kind, handle) in self.indexes.iter() {
            let view = handle.view();
            tips.push(view.tip());
            committed.insert(kind, view);
        }
        self.committed = committed;
        tips
    }

    /// Kinds at `covers`' positions (add order)
    fn kinds(&self, covers: Indexes) -> Vec<IndexKind> {
        let kinds = self.indexes.iter().enumerate();
        kinds
            .filter(|(position, _)| covers.contains(*position))
            .map(|(_, (kind, _))| kind)
            .collect()
    }

    fn execute(
        &mut self,
        output: Output<Folded>,
        work: &mut JoinSet<Result<Input<Folded>, FoldError>>,
        fetches: &mut Fetches,
    ) {
        match output {
            Output::Fetch { at, record } => {
                fetches.start(at.hash, fetch(self.balancer.clone(), at, record, Urgency::Tip));
            }
            Output::Abandon(at) => fetches.abandon(at.hash),
            Output::Fold { at, parent, block, covers } => {
                let covered = self.kinds(covers);
                let empty = PerIndex::default();
                let layers = parent.as_deref().map_or(&empty, |folded| &folded.layers);
                let parent = Views::at(&self.committed, layers, at.height.checked_sub(1));
                work.spawn(async move {
                    let folded = compute(move || fold_block(&parent, &block, &covered)).await?;
                    Ok(Input::Folded { at, covers, folded: Arc::new(folded) })
                });
            }
            Output::Publish(None) => {
                self.published.send_replace(None);
            }
            Output::Publish(Some(SnapshotTip { chain, tip, root, graph })) => {
                self.log_served(&chain, tip);
                let views = self.committed.clone();
                let indexed = Indexed::new(chain, tip, root, self.params, views, graph);
                self.published.send_replace(Some(Arc::new(indexed)));
            }
        }
    }

    /// Served tip moved: a reorg (the last one off `chain`'s best), or a new best tip
    ///
    /// - same tip (a durable-only republish): silent
    fn log_served(&mut self, chain: &VerifiedChain, tip: BlockRef) {
        let last = self.served.replace(tip);
        if last == Some(tip) {
            return;
        }
        let left = last.filter(|last| !chain.on_best(*last));
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
        let finalized = u32::from(chain.final_tip().height);
        let (height, hash) = (u32::from(tip.height), tip.hash);
        info!(height, %hash, %age, finalized, "Chain tip advanced");
    }
}

/// `position`'s next commit (`false` = its writer gone)
async fn next_commit<V: SequenceRead + MapRead>(
    position: usize,
    mut handle: IndexHandle<V>,
) -> (usize, IndexHandle<V>, bool) {
    let open = handle.changed().await;
    (position, handle, open)
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
