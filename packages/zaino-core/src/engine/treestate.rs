//! Treestate and subtree roots, per placement.
//!
//! Dispatched through [`TreestatePlacement`] on the placement marker. Only
//! [`Passthrough`] implements it: no local tier keeps a commitment-tree frontier,
//! so a routing with `Treestate = Local` has no impl to resolve to and fails
//! at the wiring bound. When a local tree index lands, its impl goes here
//! beside this one, on `Local`, bounded on the index's port.

use std::future::Future;

use crate::chain_view::ChainTier;
use crate::chain_view::ChainViewSnapshot;
use crate::routing::{Passthrough, Routing};
use zaino_primitives::types::{Height, ShieldedPool, SubtreeRoot, Treestate};
use zaino_service::TreestateRead;
use zaino_service::error::TreestateReadError;
use zaino_source::{GetSubtreeRoots, GetTreestate};

use super::EngineSnapshot;
use crate::passthrough::PassthroughProvider;

/// How a placement answers treestate reads over the providers `(F, N, Src)`.
pub trait TreestatePlacement<F, N, Src>: Send + Sync + 'static {
    fn treestate(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        at: Height,
    ) -> impl Future<Output = Result<Treestate, TreestateReadError>> + Send;

    fn subtree_roots(
        local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> impl Future<Output = Result<Vec<SubtreeRoot>, TreestateReadError>> + Send;
}

impl<F, N, Src, R> TreestateRead for EngineSnapshot<F, N, Src, R>
where
    F: ChainTier,
    N: ChainTier,
    Src: Send + Sync + 'static,
    R: Routing,
    R::Treestate: TreestatePlacement<F, N, Src>,
{
    async fn treestate(&self, at: Height) -> Result<Treestate, TreestateReadError> {
        R::Treestate::treestate(self.local(), self.passthrough(), at).await
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        R::Treestate::subtree_roots(self.local(), self.passthrough(), pool, start_index, limit)
            .await
    }
}

impl<F, N, Src> TreestatePlacement<F, N, Src> for Passthrough
where
    F: ChainTier,
    N: ChainTier,
    Src: GetTreestate + GetSubtreeRoots + Send + Sync + 'static,
{
    async fn treestate(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        at: Height,
    ) -> Result<Treestate, TreestateReadError> {
        passthrough.treestate(at).await
    }

    async fn subtree_roots(
        _local: &ChainViewSnapshot<F, N>,
        passthrough: &PassthroughProvider<Src>,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        // Index-addressed exactly like the source's `z_getsubtreesbyindex`, so
        // it relays straight through.
        passthrough.subtree_roots(pool, start_index, limit).await
    }
}
