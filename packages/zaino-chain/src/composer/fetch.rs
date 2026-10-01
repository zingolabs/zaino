//! Reading from the validator, under a shared bound.
//!
//! # Why this is its own type
//!
//! Every validator read in the crate goes through here, which makes it the one
//! place that has to be right about concurrency — and the one place a future
//! gap-block cache would sit. With thousands of clients syncing the same
//! history, collapsing identical fetches is the largest win available later,
//! and it is only available if there is a single choke point to add it to.

use std::sync::Arc;

use futures::{stream, StreamExt as _, TryStreamExt as _};
use tokio::sync::Semaphore;
use zaino_primitives::types::{Block, BlockHash, ChainMetadata, CompactBlock, Height, TreeRoots};
use zaino_source::QueryError;

use crate::composer::config::ChainViewConfig;
use crate::error::{ChainViewError, Result};
use crate::source::ChainViewSource;

/// A validator answer, with a domain rejection folded into `None`.
///
/// The validator saying "no such block" is an answer, not a failure; a
/// transport failure carries its cause through unchanged. One helper rather
/// than a mapping per read.
pub(crate) fn miss<T, E>(result: core::result::Result<T, QueryError<E>>) -> Result<Option<T>>
where
    E: core::fmt::Debug + core::fmt::Display,
{
    match result {
        Ok(value) => Ok(Some(value)),
        Err(QueryError::Domain(_)) => Ok(None),
        Err(QueryError::Fetch(error)) => Err(ChainViewError::SourceUnavailable(error)),
    }
}

/// Validator access, bounded across every client.
pub(crate) struct Fetcher<Source> {
    source: Arc<Source>,
    permits: Arc<Semaphore>,
    config: Arc<ChainViewConfig>,
}

impl<Source> Clone for Fetcher<Source> {
    fn clone(&self) -> Self {
        Self {
            source: Arc::clone(&self.source),
            permits: Arc::clone(&self.permits),
            config: Arc::clone(&self.config),
        }
    }
}

impl<Source> core::fmt::Debug for Fetcher<Source> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fetcher")
            .field("available_permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

impl<Source: ChainViewSource> Fetcher<Source> {
    pub(crate) fn new(
        source: Arc<Source>,
        permits: Arc<Semaphore>,
        config: Arc<ChainViewConfig>,
    ) -> Self {
        Self {
            source,
            permits,
            config,
        }
    }

    /// The underlying source, for reads that are already one request.
    pub(crate) fn source(&self) -> &Source {
        &self.source
    }

    /// Whether the validator may be consulted at all.
    pub(crate) fn enabled(&self) -> bool {
        self.config.passthrough_enabled
    }

    /// Refuses when the validator is switched off.
    pub(crate) fn require(&self, what: &'static str) -> Result<()> {
        if self.enabled() {
            Ok(())
        } else {
            Err(ChainViewError::NotServiceable(what))
        }
    }

    /// Runs `f` holding one permit.
    ///
    /// The permit is held for the duration of the fetch and no longer — never
    /// for the length of a stream. A client streaming a million blocks holds
    /// one only while each individual block is in flight, so one long sync
    /// cannot starve anybody.
    async fn permitted<T, F>(&self, f: F) -> Result<T>
    where
        F: core::future::Future<Output = Result<T>>,
    {
        // Acquiring can only fail if the semaphore was closed, which nothing
        // does — it lives as long as the composer.
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| ChainViewError::Fatal(String::from("validator permit pool closed")))?;
        f.await
    }

    /// Maps `heights` through `fetch`, bounded per request as well as globally.
    ///
    /// Two bounds, and both matter. The shared pool stops the *fleet* from
    /// overwhelming the validator; this per-request cap stops one large range
    /// from occupying the whole pool and stalling every other client behind it.
    ///
    /// Stops at the first height the validator does not have rather than
    /// leaving a hole mid-range: a caller walking heights would read a hole as
    /// "these blocks are empty".
    async fn fill<T, F, Fut>(&self, heights: Vec<Height>, fetch: F) -> Result<Vec<T>>
    where
        F: Fn(Height) -> Fut,
        Fut: core::future::Future<Output = Result<Option<T>>>,
    {
        let concurrency = self
            .config
            .passthrough_per_request
            .min(self.config.passthrough_permits)
            .max(1);

        let filled: Vec<Option<T>> = stream::iter(heights)
            .map(|height| self.permitted(fetch(height)))
            .buffered(concurrency)
            .try_collect()
            .await?;

        // `buffered` preserves order, so truncating at the first absence keeps
        // the run contiguous and ascending.
        Ok(filled.into_iter().map_while(|block| block).collect())
    }

    /// A parsed block and its commitment roots.
    ///
    /// Two requests in sequence: the roots are asked for by the hash the block
    /// read returned, so both describe the same block. Blocks within a range
    /// still fill concurrently with one another, so this costs latency per
    /// block rather than serialising the range.
    pub(crate) async fn block_at(&self, height: Height) -> Result<Option<(Block, TreeRoots)>> {
        let Some(block) = miss(self.source.get_block(height).await)? else {
            return Ok(None);
        };
        let Some(roots) = miss(
            self.source
                .get_commitment_tree_roots(block.header.hash)
                .await,
        )?
        else {
            return Ok(None);
        };
        Ok(Some((block, roots)))
    }

    /// A compact block.
    ///
    /// Both requests are by height and independent, so they are issued
    /// together — the projection from the compact port, and the cumulative tree
    /// sizes from the verbose port. Fetching a whole block and projecting it
    /// would transfer and parse data the caller filtered out, and chaining the
    /// roots call behind it would double the latency of the read this crate is
    /// shaped around.
    pub(crate) async fn compact_at(
        &self,
        height: Height,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Option<CompactBlock>> {
        let (compact, verbose) = futures::join!(
            self.source.get_pre_index_compact_block(height),
            self.source.get_block_verbose(height),
        );

        let Some(pre_index) = miss(compact)? else {
            return Ok(None);
        };
        let Some(verbose) = miss(verbose)? else {
            return Ok(None);
        };

        Ok(Some(super::stream::compact_from_pre_index(
            pre_index,
            ChainMetadata {
                sapling_tree_size: verbose.tree_sizes.sapling,
                orchard_tree_size: verbose.tree_sizes.orchard,
                ironwood_tree_size: verbose.tree_sizes.ironwood,
            },
            pools,
        )))
    }

    /// Consensus bytes at `height`.
    pub(crate) async fn raw_block_at(&self, height: Height) -> Result<Option<Vec<u8>>> {
        miss(self.source.get_raw_block(height).await)
    }

    /// Fills `heights` with parsed blocks and their roots.
    pub(crate) async fn fill_blocks(
        &self,
        heights: Vec<Height>,
    ) -> Result<Vec<(Block, TreeRoots)>> {
        self.fill(heights, |height| self.block_at(height)).await
    }

    /// Fills `heights` with compact blocks.
    pub(crate) async fn fill_compact(
        &self,
        heights: Vec<Height>,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Vec<CompactBlock>> {
        self.fill(heights, |height| self.compact_at(height, pools))
            .await
    }

    /// Fills `heights` with consensus bytes.
    pub(crate) async fn fill_raw_blocks(&self, heights: Vec<Height>) -> Result<Vec<Vec<u8>>> {
        self.fill(heights, |height| self.raw_block_at(height)).await
    }

    /// A parsed block by hash, for an id-addressed read no provider covers.
    pub(crate) async fn block_by_hash(&self, hash: BlockHash) -> Result<Option<Block>> {
        self.permitted(async { miss(self.source.get_block_by_hash(hash).await) })
            .await
    }
}
