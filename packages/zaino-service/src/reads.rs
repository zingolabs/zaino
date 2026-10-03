//! Read capabilities — carried by the [`crate::Snapshot`] bundle. Each is
//! backed by one index (or small set); the comment names it.

use std::future::Future;

use futures::stream::BoxStream;

use crate::{ForkPoint, Locator, SpendStatus, TxStatus};
use zaino_primitives::types::rpc::BlockHeaderVerbose;
use zaino_primitives::types::{
    AddressBalance, AddressDelta, Block, BlockHash, BlockHeader, BlockRef, BlockSelector,
    BlockTime, BlockVerbose, BlockchainInfo, CompactBlock, DecodedBlock, Height, HeightRange,
    Outpoint, RawTransaction, ShieldedPool, SubtreeRoot, Transaction, TransactionDetail,
    TransactionId, TransactionLocation, TransparentAddress, TransparentInput, TransparentOutput,
    Treestate, Utxo,
};

use crate::error::{
    AddressReadError, BlockReadError, ReadError, SpendReadError, TransactionViewError,
    TreestateReadError, TxReadError,
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

/// The two fields of a block header a timestamp-range search needs: the block's
/// hash and its timestamp. The minimal projection of a header for
/// [`HeaderRead`], carrying neither the full header nor the compact block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderSummary {
    /// The block's hash.
    pub hash: BlockHash,
    /// The block's timestamp (`nTime`), Unix epoch seconds.
    pub time: BlockTime,
}

/// A height-addressed read of a block's hash and timestamp — the header
/// projection the `getblockhashes` candidate search drives, one height at a time,
/// without composing a whole compact block per probe.
///
/// A tier read, answered locally from the headers index (FS) or the in-window
/// headers (NFS), so it joins the always-local blocks bundle alongside
/// [`CompactBlockRead`] in [`ChainTier`](crate::reads) — the chain view routes a
/// `header(h)` with the same seam rule it routes a `compact_block(h)`.
///
/// `Ok(None)` is the domain answer that **this tier does not cover `h`** (above
/// its window, below its floor, or off its best chain). A height the tier *does*
/// cover whose header cannot be read is a failure
/// ([`BlockReadError`]), never `Ok(None)`: a consumer walking a height range
/// treats a `None` inside the chain as a hole, so a readable-but-failed header
/// must surface as an error.
///
/// Backed by: the headers index (FS) or the retained window's headers (NFS).
pub trait HeaderRead: Send + Sync {
    /// The hash and timestamp of the block at `h`, or `Ok(None)` when this tier
    /// does not cover `h`.
    fn header(
        &self,
        h: Height,
    ) -> impl Future<Output = Result<Option<HeaderSummary>, BlockReadError>> + Send;
}

/// The chain-position overlay on a block — confirmations, difficulty,
/// chainwork, and the neighbouring block hashes — that the block's own contents
/// cannot give, because they describe the block's place in the current chain
/// rather than its bytes. The explorer surface behind `getblock(_, 2)` and
/// `getblockheader`.
///
/// A separate trait, not methods on [`BlockRead`], for the same reason
/// [`RawTransactionRead`] and [`TransactionRead`] are separate: [`BlockRead`]
/// returns the domain block every consumer uses, while chain position is an
/// explorer-only surface a wallet never asks for. A caller assembling a verbose
/// response combines [`BlockRead::block`] with this.
///
/// Backed by: passthrough to the validator's verbose header / block reads.
pub trait BlockVerboseRead: Send + Sync {
    /// The verbose block header for `hash` — hash-addressed, matching both the
    /// source port and `getblockheader`. `Ok(None)` when no block has that hash.
    fn block_header_verbose(
        &self,
        hash: BlockHash,
    ) -> impl Future<Output = Result<Option<BlockHeaderVerbose>, BlockReadError>> + Send;
    /// The verbose chain-position facts for the block `at`. `Ok(None)` when the
    /// selector names no block.
    fn block_verbose(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<BlockVerbose>, BlockReadError>> + Send;
    /// The raw consensus bytes of the block `at` — exactly the bytes the block
    /// hash commits to. `Ok(None)` when the selector names no block.
    ///
    /// The node-only surface behind `getblock(_, 0)`, which the explorer's search
    /// page calls to test whether a string is a block. Always passthrough, for the
    /// same reason as the full [`BlockRead::block`] read: the finalised store holds
    /// compact projections, not full block bytes.
    fn raw_block(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, BlockReadError>> + Send;
}

/// A transparent input paired with the output it spends.
///
/// `getrawtransaction`/`getblock(_, 2)` render each transparent input with the
/// value and script of the output being spent, which the input itself only
/// references by outpoint. Resolving that reference is the work
/// [`TransactionViewRead`] does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInput {
    /// The input, as it names the output it spends.
    pub outpoint: TransparentInput,
    /// The output that input spends.
    pub spent: TransparentOutput,
}

/// A transaction with its envelope detail and every transparent input resolved
/// to the output it spends.
///
/// The explorer shape: the indexing [`Transaction`] plus the
/// [`TransactionDetail`] it drops, plus the resolved inputs the wire form needs
/// to show each spend's value and address.
#[derive(Debug, Clone)]
pub struct TransactionView {
    /// The transaction, decomposed by pool.
    pub transaction: Transaction,
    /// The envelope, coinbase input, and Sprout values the indexing shape drops.
    pub detail: TransactionDetail,
    /// The resolved transparent inputs, in the order of
    /// [`transaction.transparent.inputs`](zaino_primitives::types::TransparentData::inputs).
    pub inputs: Vec<ResolvedInput>,
    /// The transaction's raw consensus bytes, carried so the explorer's `hex`
    /// field renders from the same decode rather than a refetch.
    pub raw: Vec<u8>,
}

/// A [`TransactionView`] with where the transaction lives in the chain — the
/// `getrawtransaction` surface, which reports the containing block.
#[derive(Debug, Clone)]
pub struct LocatedTransactionView {
    /// The resolved transaction.
    pub view: TransactionView,
    /// Where the transaction was found.
    pub location: TransactionLocation,
}

/// Every transaction of a block as a [`TransactionView`], plus the block's
/// serialized size — the `getblock(_, 2)` surface.
#[derive(Debug, Clone)]
pub struct BlockTransactionViews {
    /// Serialized byte length of the whole block.
    pub size: u64,
    /// The block's transactions, in block order, each with its inputs resolved.
    pub transactions: Vec<TransactionView>,
}

/// The resolved-transaction read: a transaction, or a whole block's
/// transactions, with every transparent input resolved to the output it spends.
/// The explorer surface behind `getrawtransaction <txid> 1` and
/// `getblock <block> 2`.
///
/// Distinct from [`TransactionRead`], which returns the pool-decomposed
/// transaction as it stands: resolving an input to its spent output needs a
/// second lookup per distinct prevout, which the explorer wire shape requires
/// and a wallet never asks for.
///
/// Backed by: passthrough to the validator's decoded transaction / decoded block
/// reads. A prevout is resolved first from the same block, otherwise through the
/// decoded-transaction read. A miss on the requested transaction or block is a
/// domain answer (`Ok(None)`); a miss on a *prevout* is a source inconsistency
/// ([`TransactionViewError::MissingPrevout`]), never a blank value.
pub trait TransactionViewRead: Send + Sync {
    /// The resolved view of the transaction `id`, with its location. `Ok(None)`
    /// when no transaction has that id.
    fn transaction_view(
        &self,
        id: TransactionId,
    ) -> impl Future<Output = Result<Option<LocatedTransactionView>, TransactionViewError>> + Send;
    /// The resolved views of every transaction in the block `at`. `Ok(None)`
    /// when the selector names no block.
    ///
    /// This resolves every transparent prevout, so a caller that needs only the
    /// block's size and transaction ids — `getblock` verbosity 1, the explorer's
    /// hot per-page call — uses [`decoded_block`](Self::decoded_block) instead,
    /// which does no resolution.
    fn block_transaction_views(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<BlockTransactionViews>, TransactionViewError>> + Send;
    /// The block `at` decoded into every transaction with its detail — the
    /// block's size and transaction ids, with **no** prevout resolution.
    /// `Ok(None)` when the selector names no block.
    ///
    /// This is the cheap read behind `getblock` verbosity 1: it neither fetches
    /// nor resolves the outputs the block's inputs spend, so a prevout the
    /// validator cannot serve never fails a page that shows no input values.
    fn decoded_block(
        &self,
        at: BlockSelector,
    ) -> impl Future<Output = Result<Option<DecodedBlock>, TransactionViewError>> + Send;
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
/// Backed by: the validator's own [`BlockchainInfo`], passed through whole. The
/// aggregate describes one chain position, so its fields are read together from
/// one source rather than assembled from a mix of local and passthrough reads
/// that could disagree about the height they describe.
pub trait ChainInfoRead: Send + Sync {
    fn chain_info(&self) -> impl Future<Output = Result<BlockchainInfo, ReadError>> + Send;
}
