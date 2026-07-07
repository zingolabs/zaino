//! Block fetcher trait — abstracts the remote data source for the sync loop.
//!
//! The store never fetches blocks itself. Instead it calls a [`BlockFetcher`]
//! implementation provided by the caller. This keeps the store free of any
//! knowledge about the network protocol, chain-specific block structure, or
//! serialisation format.

use async_trait::async_trait;

use crate::types::{Block, BlockHash, Height};

/// A source of blocks for the sync loop.
///
/// The implementor is responsible for fetching blocks from a remote node
/// (or any other source), building valid [`Block`] values with correct
/// `hash`, `prev_hash`, `height`, and opaque payload bytes.
#[async_trait]
pub trait BlockFetcher {
    /// Error type for fetch operations.
    type Error: std::fmt::Display + Send + 'static;

    /// Fetch the remote chain tip.
    async fn fetch_tip(&self) -> Result<(BlockHash, Height), Self::Error>;

    /// Fetch a batch of blocks for the inclusive height range `[from, to]`.
    ///
    /// The returned blocks must be in ascending height order and form a
    /// valid chain (each block's `prev_hash` must match the previous block's
    /// hash, and the first block's `prev_hash` must match the local tip at
    /// the time of the call).
    async fn fetch_batch(
        &mut self,
        from: Height,
        to: Height,
    ) -> Result<Vec<(BlockHash, Block)>, Self::Error>;

    /// Fetch a single block at `height`. Used by `find_anchor_index` for the
    /// backward walk during short sync.
    async fn fetch_at_height(
        &mut self,
        height: Height,
    ) -> Result<Block, Self::Error>;
}
