//! Read capabilities — carried by the [`crate::Snapshot`] bundle. Each is
//! backed by one index (or small set); the comment names it.

use std::future::Future;

use futures::stream::BoxStream;

use crate::{ChainInfo, ForkPoint, Locator, SpendStatus, TxStatus};
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockRef, BlockSelector,
    CompactBlock, Height, HeightRange, Outpoint, RawTransaction, ShieldedPool, SubtreeRoot,
    Transaction, TransactionId, TransparentAddress, TransparentReceive, Treestate, Utxo,
};

use crate::error::{
    AddressReadError, BlockReadError, ReadError, SpendReadError, TreestateReadError, TxReadError,
};

/// Backed by: headers + block-bytes indexes.
pub trait BlockRead: Send + Sync {
    fn tip(&self) -> impl Future<Output = Result<BlockRef, BlockReadError>> + Send;
    fn block(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<Block>, BlockReadError>> + Send;
    fn block_header(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<BlockHeader>, BlockReadError>> + Send;
    fn block_height(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<Height>, BlockReadError>> + Send;
    fn stream_blocks(&self, range: HeightRange) -> BoxStream<'_, Result<Block, ReadError>>;
}

/// Backed by: the `compact_block` index (FS) or the NFS `Chain`. The
/// lightwallet-facing block read — `BlockRead::block` (full) is passthrough.
pub trait CompactBlockRead: Send + Sync {
    fn compact_block(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send;
    fn stream_compact(&self, range: HeightRange) -> BoxStream<'_, Result<CompactBlock, ReadError>>;
}

/// The pool-decomposed transaction read: the transaction parsed into its
/// transparent/shielded structure. The explorer/node surface — a wallet takes
/// the transaction as bytes and parses locally (see [`RawTransactionRead`]).
///
/// Backed by: txid-location index plus a bytes→[`Transaction`] parse.
pub trait TransactionRead: Send + Sync {
    fn transaction(
        &self,
        id: TransactionId,
    ) -> impl Future<Output = Result<Option<Transaction>, TxReadError>> + Send;
    fn transaction_status(
        &self,
        id: TransactionId,
    ) -> impl Future<Output = Result<TxStatus, TxReadError>> + Send;
}

/// The wallet-facing transaction read: the transaction as raw serialized bytes
/// plus where it lives, the lightwalletd `GetTransaction` shape. A wallet parses
/// the bytes itself, so this needs no parse and passes straight through to the
/// validator; the pool-decomposed [`TransactionRead`] is the explorer/node
/// surface.
///
/// Backed by: passthrough to the validator's raw-transaction fetch.
pub trait RawTransactionRead: Send + Sync {
    fn raw_transaction(
        &self,
        id: TransactionId,
    ) -> impl Future<Output = Result<Option<RawTransaction>, TxReadError>> + Send;
}

/// Backed by: commitment-tree index.
pub trait TreestateRead: Send + Sync {
    fn treestate(
        &self,
        at: Height,
    ) -> impl Future<Output = Result<Treestate, TreestateReadError>> + Send;
    /// Complete note-commitment subtree roots for `pool`, addressed by subtree
    /// index: a run of at most `limit` roots starting at `start_index` (all from
    /// there when `limit` is `None`). Index-addressed, not height-addressed,
    /// because a subtree completes at a height fixed by note-commitment density —
    /// there is no height→index mapping — and because this is exactly how a wallet
    /// pages the frontier and how the `z_getsubtreesbyindex` source answers.
    fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        limit: Option<u16>,
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
    ) -> impl Future<Output = Result<Vec<TransactionId>, AddressReadError>> + Send;
}

/// Backed by: transparent/address index. The receive side of address history,
/// on its own.
///
/// What a tier holding no history behind itself can say about an address. An
/// output names its recipient in its script, so any tier that holds the output
/// can report the receive. A *spend* names only the outpoint it consumes, so
/// attributing one to an address needs the output that outpoint created — which
/// a bounded window does not have once that output predates it.
///
/// So this is deliberately narrower than [`AddressRead`]: receives only, no
/// netting and no spend attribution. A tier that can answer the full history
/// implements `AddressRead` and has no need of this; the volatile window
/// implements this and [`SpendRead`], and the composer turns the pair into the
/// full answer by supplying the history the window lacks.
///
/// ```text
/// receives(addr, range)                  this trait — every tier holding the outputs
/// spends(addr, range) = owned(addr) ∩ spent(range)
///                                        needs owned(addr), which is history
/// ```
pub trait AddressReceiveRead: Send + Sync {
    /// Every output in `range` paying `addr`, in height order, whether or not
    /// it was later spent.
    fn receives(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<TransparentReceive>, AddressReadError>> + Send;
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

/// Compact blocks with spend nullifiers populated — the lightwalletd
/// `GetBlockNullifiers` serving variant. A read *on top of* the wallet core, so
/// it is the light-wallet use case's delta, not part of `WalletReadCore`.
///
/// Backed by: the compact-block index plus the nullifier set.
pub trait CompactNullifierRead: Send + Sync {
    fn compact_block_nullifiers(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<CompactBlock>, BlockReadError>> + Send;
}

/// Aggregate chain/node info — the domain behind `getblockchaininfo`. The
/// node-rpc use case's delta over the shared reads.
///
/// Backed by: the chain-head tip plus the validator's estimated height.
pub trait ChainInfoRead: Send + Sync {
    fn chain_info(&self) -> impl Future<Output = Result<ChainInfo, ReadError>> + Send;
}
