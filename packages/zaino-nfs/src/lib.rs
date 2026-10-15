#![doc = include_str!("../usage.md")]
#![forbid(unsafe_code)]

mod core;
mod emit;
mod fold;
mod graph;
mod publish;

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
use zaino_persistence::{IndexKind, PerIndex, PinnedView};
use zaino_primitives::display::Human;
use zaino_primitives::types::{BlockRef, ReorgDepth};
use zaino_source::ChainDataSource;
use zaino_sync::{compute, fetch, Checked, IndexHandle};
use zaino_traffic::{TrafficBalancer, Urgency};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::BlockHash;

use crate::core::{Indexes, Input, NfsCore, Output, Publication};
use crate::fold::{fold_block, Folded};

pub use crate::emit::describe_metrics;
pub use crate::fold::FoldError;
pub use crate::publish::{At, Branch, ChainParams, Indexed, Published, Views};

#[derive(Debug, thiserror::Error)]
pub enum NfsError {
    #[error(transparent)]
    Fold(#[from] FoldError),
    #[error("header sync stopped publishing the verified chain")]
    ChainGone,
    #[error("{0} writer stopped publishing its durable view")]
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
    durable: PerIndex<V>,
    published: watch::Sender<Option<Arc<Indexed<V>>>>,
    served: Option<BlockRef>,
}

impl<S: ChainDataSource, V: PinnedView> Nfs<S, V> {
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
            durable: PerIndex::default(),
            published: watch::Sender::new(None),
            served: None,
        }
    }

    /// `kind` enabled: its durable view read and extended at the tip (panics: `kind` twice)
    pub fn add(&mut self, kind: IndexKind, index: IndexHandle<V>) {
        self.indexes.insert(kind, index);
    }

    /// Every publish as a watch: served tip moved or a durable tip moved (`GrpcSnapshot`'s
    /// input)
    pub fn indexed(&self) -> Published<V> {
        self.published.subscribe()
    }

    /// Cancel → `Ok` (an input gone after it included: its owner stops on the same cancel)
    pub async fn run(mut self, cancel: CancellationToken) -> Result<(), NfsError> {
        match cancel.run_until_cancelled(self.follow()).await {
            Some(Err(NfsError::ChainGone | NfsError::IndexGone(_))) if cancel.is_cancelled() => {
                Ok(())
            }
            followed => followed.unwrap_or(Ok(())),
        }
    }

    async fn follow(&mut self) -> Result<(), NfsError> {
        let positions: Vec<IndexHandle<V>> =
            self.indexes.iter().map(|(_, handle)| handle.clone()).collect();
        let mut commits = JoinSet::new();
        for (position, handle) in positions.into_iter().enumerate() {
            commits.spawn(next_commit(position, handle));
        }
        let mut core = NfsCore::new(self.lookahead.get(), self.indexes.len(), self.window);
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
                        let (kind, _) = self.indexes.at(position);
                        return Err(NfsError::IndexGone(kind.name()));
                    }
                    commits.spawn(next_commit(position, handle));
                    inputs.push(Input::Durable(self.refresh()));
                }
            }
        }
    }

    /// Each index's durable view reloaded into `durable`; their durable tips
    ///
    /// - `durable` changes only here: every fold and publication pairs layers with exactly the
    ///   durable tips the core was last told
    fn refresh(&mut self) -> Vec<Option<BlockRef>> {
        let (mut durable, mut tips) = (PerIndex::default(), Vec::new());
        for (kind, handle) in self.indexes.iter() {
            let view = handle.view();
            tips.push(view.tip());
            durable.insert(kind, view);
        }
        self.durable = durable;
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
                let below = u32::from(at.height).checked_sub(1);
                let parent = Views::at(&self.durable, layers, below.map(BlockHeight::from));
                work.spawn(async move {
                    let folded = compute(move || fold_block(&parent, &block, &covered)).await?;
                    Ok(Input::Folded { at, covers, folded: Arc::new(folded) })
                });
            }
            Output::Publish(None) => {
                self.published.send_replace(None);
            }
            Output::Publish(Some(Publication { chain, tip, root, graph })) => {
                self.log_served(&chain, tip);
                let views = self.durable.clone();
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
async fn next_commit<V: PinnedView>(
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

#[cfg(test)]
mod tests;
