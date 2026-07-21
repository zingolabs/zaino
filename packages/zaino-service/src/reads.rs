//! Read capabilities — carried by the [`crate::Snapshot`] bundle. Each is
//! backed by one index (or small set); the comment names it.

use std::future::Future;

use futures::stream::BoxStream;

use zaino_core::{
    AddressBalance, AddressDelta, Block, BlockHeader, BlockHash, BlockId, BlockRef, ForkPoint,
    Height, HeightRange, Locator, Outpoint, ShieldedPool, SpendStatus, SubtreeRoot, Transaction,
    TransactionHash, TransparentAddress, Treestate, TxStatus, Utxo,
};

use crate::error::{
    AddressReadError, BlockReadError, ReadError, SpendReadError, TreestateReadError, TxReadError,
};

/// Backed by: headers + block-bytes indexes.
pub trait BlockRead: Send + Sync {
    fn tip(&self) -> impl Future<Output = Result<BlockId, BlockReadError>> + Send;
    fn block(&self, at: BlockRef) -> impl Future<Output = Result<Option<Block>, BlockReadError>> + Send;
    fn block_header(
        &self,
        at: BlockRef,
    ) -> impl Future<Output = Result<Option<BlockHeader>, BlockReadError>> + Send;
    fn block_height(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, BlockReadError>> + Send;
    fn stream_blocks(&self, range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>>;
}

/// Backed by: txid-location index.
pub trait TransactionRead: Send + Sync {
    fn transaction(
        &self,
        id: TransactionHash,
    ) -> impl Future<Output = Result<Option<Transaction>, TxReadError>> + Send;
    fn transaction_status(
        &self,
        id: TransactionHash,
    ) -> impl Future<Output = Result<TxStatus, TxReadError>> + Send;
}

/// Backed by: commitment-tree index.
pub trait TreestateRead: Send + Sync {
    fn treestate(
        &self,
        at: Height,
    ) -> impl Future<Output = Result<Treestate, TreestateReadError>> + Send;
    fn subtree_roots(
        &self,
        pool: ShieldedPool,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<SubtreeRoot>, TreestateReadError>> + Send;
}

/// Backed by: transparent/address index. Consumers use the subset they need
/// (zallet: `unspent_outpoints` + `tx_ids`; an explorer: `balance` + `deltas`).
pub trait AddressRead: Send + Sync {
    fn balance(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<AddressBalance, AddressReadError>> + Send;
    fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
    ) -> impl Future<Output = Result<Vec<Utxo>, AddressReadError>> + Send;
    fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<AddressDelta>, AddressReadError>> + Send;
    fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<TransactionHash>, AddressReadError>> + Send;
}

/// Backed by: spend index.
pub trait SpendRead: Send + Sync {
    fn spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, SpendReadError>> + Send;
}

/// Backed by: headers (over the non-finalised branch set).
pub trait ForkReconcile: Send + Sync {
    fn fork_point(
        &self,
        locator: Locator,
    ) -> impl Future<Output = Result<Option<ForkPoint>, ReadError>> + Send;
    fn blocks_to_tip(&self, from: Height) -> BoxStream<'_, Result<Block, ReadError>>;
}
