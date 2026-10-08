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
}

/// Why a [`ChainReader`] read failed.
#[derive(Debug, thiserror::Error)]
pub enum ChainReadError {
    /// The RPC call itself failed (network, decode, or an error the node
    /// returned) — the adapter's only failure mode today.
    #[error("chain height read failed")]
    Rpc(#[source] Box<dyn std::error::Error + Send + Sync>),
}
