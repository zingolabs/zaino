//! Treestate and subtree roots, per placement.
//!
//! Dispatched through [`TreestatePlacement`] on the placement marker, once for
//! [`Passthrough`] and once for [`Local`]. The two have different `Self` types,
//! so the impls cannot overlap; which one a use case gets is its routing's
//! `Treestate` type.
//!
//! **Passthrough** relays each read live to the validator, disclosing the
//! queried heights to it.
//!
//! **Local** composes across the seam. A height at or below the watermark is the
//! finalised store's, read straight from the `tree_state` index. Above it, the
//! window answers by folding its blocks forward from the finalised frontier at
//! the watermark — the seed the store supplies — so the two sides agree and a
//! reorg in the window is reflected without the store moving. Subtree roots are
//! the finalised tier's page followed by the window's newly completed ones, whose
//! global indices continue above it. Requires the finalised tier to have a
//! [`TreestateRead`] and the window a [`TreestateWindowRead`].

use std::future::Future;

use crate::chain_view::ChainTier;
use crate::chain_view::ChainViewSnapshot;
use crate::routing::{Local, Passthrough, Routing};
use zaino_primitives::types::{Height, ShieldedPool, SubtreeRoot, Treestate};
use zaino_service::error::TreestateReadError;
use zaino_service::{Capability, PoolActivationSource, TreestateRead, TreestateWindowRead};
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

// --- Local --------------------------------------------------------------------

/// The finalised treestate at the watermark — the seed the window folds from —
/// or `None` when the store holds nothing (the window folds from genesis).
async fn seed<F, N>(
    local: &ChainViewSnapshot<F, N>,
) -> Result<Option<Treestate>, TreestateReadError>
where
    F: ChainTier + TreestateRead,
    N: ChainTier,
{
    match local.watermark() {
        Some(watermark) => Ok(Some(local.finalised().treestate(watermark).await?)),
        None => Ok(None),
    }
}

impl<F, N, Src> TreestatePlacement<F, N, Src> for Local
where
    F: ChainTier + TreestateRead + PoolActivationSource,
    N: ChainTier + TreestateWindowRead,
    Src: Send + Sync + 'static,
{
    async fn treestate(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        at: Height,
    ) -> Result<Treestate, TreestateReadError> {
        // At or below the watermark is the finalised store's; the store refuses a
        // height above its coverage, which the seam routing never asks it for.
        if local.watermark().is_some_and(|watermark| at <= watermark) {
            return local.finalised().treestate(at).await;
        }
        // Above the watermark, the window folds from the finalised seed, deciding
        // each pool's presence against the same activation schedule the store
        // used so the two sides of the seam agree. A height above the served tip
        // has no block: there is no treestate to return, and the read cannot
        // express a domain miss, so it is NotServiceable rather than a fabricated
        // empty tree.
        let seed = seed(local).await?;
        let activations = local.finalised().pool_activations();
        local
            .non_finalised()
            .window_treestate(seed.as_ref(), activations, at)
            .await?
            .ok_or(TreestateReadError::NotServiceable(Capability::Treestate))
    }

    async fn subtree_roots(
        local: &ChainViewSnapshot<F, N>,
        _passthrough: &PassthroughProvider<Src>,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>, TreestateReadError> {
        // The finalised tier's page first — its subtree indices are the lowest —
        // then the window's completions in the same index range. A subtree
        // completes at one height, so a window completion's global index is above
        // every finalised one; concatenating and capping at `limit` yields the
        // globally ordered page.
        let mut roots = local
            .finalised()
            .subtree_roots(pool, start_index, limit)
            .await?;
        let seed = seed(local).await?;
        let window = local
            .non_finalised()
            .window_subtree_roots(seed.as_ref(), pool, start_index, limit)
            .await?;
        roots.extend(window);
        if let Some(limit) = limit {
            roots.truncate(usize::from(limit));
        }
        Ok(roots)
    }
}
