//! Published full-wallet client over Zaino's inner service.
//!
//! This crate is what a full-wallet consumer (zallet) depends on. It is a
//! concrete **client / adapter** — builder + handle in published bytes/id
//! types — NOT a port: it defines no interface for others to implement. It
//! wraps an `impl zaino_service::IndexerService` and translates domain answers
//! to the published vocabulary in [`types`].
//!
//! Scaffold: method bodies are `todo!()`. The surface exists so we can prove it
//! services zallet's `Chain`/`ChainView` trait (see the `zallet-fit` crate).
#![forbid(unsafe_code)]

mod types;
pub use types::*;

use futures::stream::BoxStream;

use zaino_service::{IndexerService, Snapshot as InnerSnapshot};

/// Where the runtime runs. `SelfHosted` → we own an executor and the consumer
/// needs no async runtime of their own; `Ambient` → share the caller's.
#[derive(Clone, Copy, Debug, Default)]
pub enum Executor {
    #[default]
    SelfHosted,
    Ambient,
}

/// Builds and initialises a [`WalletClient`], spinning the shared runtime.
#[derive(Default)]
pub struct WalletClientBuilder {
    _executor: Executor,
    // TODO: source, backend, index-set selection.
}

impl WalletClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn executor(mut self, executor: Executor) -> Self {
        self._executor = executor;
        self
    }

    /// Kick off the background loops and return a live client.
    pub async fn init<E: IndexerService>(self) -> Result<WalletClient<E>, WalletError> {
        todo!("wire zaino-runtime, spawn loops, return handle")
    }
}

/// A live handle to Zaino, in published types.
#[derive(Clone)]
pub struct WalletClient<E> {
    #[allow(dead_code)]
    inner: E,
}

impl<E: IndexerService> WalletClient<E> {
    pub async fn snapshot(&self) -> Result<WalletSnapshot<E::Snapshot>, WalletError> {
        todo!()
    }

    pub async fn broadcast(&self, _raw_tx: Vec<u8>) -> Result<TxId, WalletError> {
        todo!()
    }

    pub async fn reported_upgrades(&self) -> Result<Vec<ReportedUpgrade>, WalletError> {
        todo!()
    }

    pub fn subscribe_tip(&self) -> BoxStream<'_, BlockId> {
        todo!()
    }

    pub fn subscribe_mempool(&self) -> BoxStream<'_, TxId> {
        todo!()
    }

    pub async fn sapling_subtree_roots(&self) -> Result<Vec<RawSubtreeRoot>, WalletError> {
        todo!()
    }

    pub async fn orchard_subtree_roots(&self) -> Result<Vec<RawSubtreeRoot>, WalletError> {
        todo!()
    }

    pub async fn ironwood_subtree_roots(&self) -> Result<Vec<RawSubtreeRoot>, WalletError> {
        todo!()
    }
}

/// A pinned view, in published types.
#[derive(Clone)]
pub struct WalletSnapshot<S> {
    #[allow(dead_code)]
    snap: S,
}

impl<S: InnerSnapshot> WalletSnapshot<S> {
    pub async fn tip(&self) -> Result<BlockId, WalletError> {
        todo!()
    }

    pub async fn raw_block(&self, _height: u32) -> Result<Option<RawBlock>, WalletError> {
        todo!()
    }

    pub async fn raw_block_header(&self, _height: u32) -> Result<Option<Vec<u8>>, WalletError> {
        todo!()
    }

    pub async fn raw_transaction(
        &self,
        _txid: TxId,
    ) -> Result<Option<RawTransaction>, WalletError> {
        todo!()
    }

    pub async fn transaction_status(&self, _txid: TxId) -> Result<TxStatus, WalletError> {
        todo!()
    }

    pub async fn treestate(&self, _height: u32) -> Result<Option<RawTreestate>, WalletError> {
        todo!()
    }

    pub async fn fork_point(&self, _locator: Vec<[u8; 32]>) -> Result<Option<BlockId>, WalletError> {
        todo!()
    }

    pub async fn unspent_outpoints(&self, _address: &str) -> Result<Vec<Outpoint>, WalletError> {
        todo!()
    }

    pub async fn address_tx_ids(
        &self,
        _address: &str,
        _from: u32,
        _to: u32,
    ) -> Result<Vec<TxId>, WalletError> {
        todo!()
    }

    pub async fn spend_status(&self, _outpoint: Outpoint) -> Result<SpendStatus, WalletError> {
        todo!()
    }

    pub async fn block_height(&self, _hash: [u8; 32]) -> Result<Option<u32>, WalletError> {
        todo!()
    }

    pub fn stream_blocks_to_tip(&self, _from: u32) -> BoxStream<'_, Result<RawBlock, WalletError>> {
        todo!()
    }

    pub fn stream_blocks(
        &self,
        _from: u32,
        _to: u32,
    ) -> BoxStream<'_, Result<RawBlock, WalletError>> {
        todo!()
    }

    pub async fn mempool_stream(
        &self,
    ) -> Result<Option<BoxStream<'_, RawTransaction>>, WalletError> {
        todo!()
    }
}
