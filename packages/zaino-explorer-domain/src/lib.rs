//! The explorer's domain: a block explorer's one driven port, reading chain
//! state. The TUI and the web surface (see `zaino-explorer-web`,
//! `zaino-explorer-tui`) are two driving adapters over this same domain;
//! `zaino-explorer-zaino-client` is its one production implementor, talking
//! to a live zainod. Nothing in this crate depends on jsonrpsee, zaino-noderpc,
//! or any other transport detail — that coupling lives only in an adapter
//! crate, injected at each driving adapter's composition root.
#![forbid(unsafe_code)]

use std::future::Future;

/// Reads chain state. The explorer's one driven port.
pub trait ChainReader: Clone + Send + Sync + 'static {
    /// The current chain height.
    fn chain_height(&self) -> impl Future<Output = Result<u32, ChainReadError>> + Send;

    /// The `count` most recent blocks, newest first.
    fn recent_blocks(
        &self,
        count: u32,
    ) -> impl Future<Output = Result<Vec<BlockSummary>, ChainReadError>> + Send;
}

/// One block's summary, as shown in a block list — not the full block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSummary {
    /// The block's height.
    pub height: u32,
    /// The block's hash, in the wire's display (byte-reversed) hex form.
    pub hash: String,
    /// The block's timestamp (seconds since the Unix epoch).
    pub time: u32,
    /// Number of transactions in the block.
    pub tx_count: u32,
}

/// Why a [`ChainReader`] read failed.
#[derive(Debug, thiserror::Error)]
pub enum ChainReadError {
    /// The RPC call itself failed (network, decode, or an error the node
    /// returned) — the adapter's only failure mode today.
    #[error("chain read failed")]
    Rpc(#[source] Box<dyn std::error::Error + Send + Sync>),
}
