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
    TransparentReceive, TransparentSpend, Treestate, Utxo,
};

use crate::error::{
    AddressReadError, BlockHashReadError, BlockReadError, ReadError, SpendReadError,
    TransactionViewError, TreestateReadError, TxReadError,
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

/// A block identified by its height, hash and timestamp — one entry of the
/// `getblockhashes` answer.
///
/// The three facts the timestamp-range selection carries out: `height` locates
/// the block on the chain, `hash` is what the explorer renders, and `time` is
/// the timestamp the range matched (also the `logicalts` the verbose wire shape
/// reports). The adapter renders the bare hash or the `{blockhash, logicalts}`
/// object from this one shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHashAt {
    /// The block's height.
    pub height: Height,
    /// The block's hash.
    pub hash: BlockHash,
    /// The block's timestamp (`nTime`), the value the range matched.
    pub time: BlockTime,
}

/// The timestamp-range block selection behind `getblockhashes`: every block
/// whose `nTime` lies in the half-open range `[low, high)`, ordered ascending by
/// time, then by hash.
///
/// A **local** read, driven over the composed chain view's [`HeaderRead`] one
/// height at a time — no new source port. Zcash block timestamps are not
/// monotonic (a block's `nTime` is bounded only relative to the median-time-past
/// of its predecessors), so this is not a slice of the height axis: the engine
/// derives a candidate height bracket guaranteed to contain every in-range block
/// from the median-time-past consensus rule, then filters that bracket by each
/// block's actual timestamp.
///
/// Backed by: the headers index (FS) or the retained window's headers (NFS) —
/// the same [`HeaderRead`] backing. Its availability is therefore type-level,
/// not a runtime capability: a tier implements [`HeaderRead`] only where that
/// backing exists (on the finalised store the implementation is bounded on the
/// local blocks bundle, which includes the headers index), so there is no
/// serviceability variant to consult at read time. A range beyond the tip or
/// before genesis is an empty list, never an error; a hole in the chain view at
/// or below the pinned tip is a typed failure
/// ([`BlockHashReadError::MissingHeader`]), never a silently dropped block.
pub trait BlockHashRead: Send + Sync {
    /// Every block with `low <= nTime < high`, ascending by time then by hash.
    fn block_hashes(
        &self,
        low: BlockTime,
        high: BlockTime,
    ) -> impl Future<Output = Result<Vec<BlockHashAt>, BlockHashReadError>> + Send;
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

/// The default per-request ceiling on how many address-history entries a single
/// query may collect, across every address it reads.
///
/// Sized for the passthrough phase, before a balance/UTXO-by-address aggregate
/// makes these reads cheap at any history size. At roughly a couple hundred bytes
/// resident per entry across the index scan and the joined vectors, five million
/// is on the order of a gigabyte — comfortably under the process memory limit,
/// yet well above any legitimate single transparent address observed on mainnet
/// (the largest pool-payout addresses hold on the order of a million outputs). A
/// request past this bound fails with [`AddressReadError::TooLarge`] instead of
/// letting the scan grow unbounded and take the whole process down with it.
const MAX_ADDRESS_ENTRIES: usize = 5_000_000;

/// A request-scoped ceiling on how many address-history entries a single query
/// may collect, across every address it reads.
///
/// [`AddressRead`]'s methods are called once per address, and a multi-address
/// query loops over them accumulating the results. A ceiling enforced per call
/// would bound each address but let a request naming K pool-scale addresses grow
/// to roughly K × the ceiling, breaking the guarantee that one request is
/// bounded. One budget — created per request and passed by `&mut` through every
/// per-address read — bounds the request as a whole instead: the index scan
/// charges each entry it collects against it and refuses with
/// [`AddressReadError::TooLarge`] the moment a charge would overrun, before the
/// over-limit entry is materialised.
///
/// A query that reads several facets of one address (a composed balance scans the
/// address's receives *and* its unspent set) charges each scan against the same
/// budget, so a single address is counted more than once; the ceiling is
/// deliberately far enough above any legitimate single address that this
/// conservative over-count never refuses a real one.
pub struct ReadBudget {
    limit: usize,
    remaining: usize,
}

impl ReadBudget {
    /// A budget for one request, with the default production ceiling
    /// ([`MAX_ADDRESS_ENTRIES`]).
    pub fn for_request() -> Self {
        Self::with_limit(MAX_ADDRESS_ENTRIES)
    }

    /// A budget with an explicit ceiling. Production uses [`for_request`](Self::for_request);
    /// a small limit drives the over-ceiling tests without a pool-scale fixture.
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            remaining: limit,
        }
    }

    /// The ceiling this budget was created with — the figure a refusal reports.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Reserve one entry against the budget. `true` when it was within budget and
    /// the slot is now taken; `false` when the budget is exhausted, which the
    /// counting scan turns into [`AddressReadError::TooLarge`].
    pub fn charge_one(&mut self) -> bool {
        match self.remaining.checked_sub(1) {
            Some(remaining) => {
                self.remaining = remaining;
                true
            }
            None => false,
        }
    }
}

/// Backed by: transparent/address index. Consumers use the subset they need
/// (zallet: `unspent_outpoints` + `tx_ids`; an explorer: `balance` + `deltas`).
///
/// Every method takes a [`ReadBudget`] by `&mut`: one budget is created per
/// request and threaded through each address the query reads, so the whole
/// request is bounded rather than each address independently. A single-address
/// caller still passes one (`ReadBudget::for_request`).
pub trait AddressRead: Send + Sync {
    fn balance(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
        budget: &mut ReadBudget,
    ) -> impl Future<Output = Result<AddressBalance, AddressReadError>> + Send;
    fn unspent_outpoints(
        &self,
        addr: &TransparentAddress,
        budget: &mut ReadBudget,
    ) -> impl Future<Output = Result<Vec<Utxo>, AddressReadError>> + Send;
    fn deltas(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
        budget: &mut ReadBudget,
    ) -> impl Future<Output = Result<Vec<AddressDelta>, AddressReadError>> + Send;
    /// Every transaction touching `addr` in `range`, each paired with the height
    /// at which it touched the address when that is known.
    ///
    /// The height is carried out of the read because a caller merging several
    /// addresses must order the union by height (zcashd's `getaddresstxids`
    /// sort), and the bare txid cannot be re-sorted. It is `Option` because a
    /// **passthrough** source may not report it: zcashd/zebra's `getaddresstxids`
    /// returns bare txids with no heights, so that path yields `None` rather than
    /// a fabricated value. A **local** read always supplies `Some` — the index
    /// knows the height. A single address's result is ordered `(height, txid)` and
    /// de-duplicated; a transaction that both pays and spends for the address
    /// appears once.
    fn tx_ids(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
        budget: &mut ReadBudget,
    ) -> impl Future<Output = Result<Vec<(Option<Height>, TransactionId)>, AddressReadError>> + Send;
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

    /// Which of `outpoints` this tier saw spent within `range`, and where.
    ///
    /// The caller supplies the outpoints because deciding *which* belong to an
    /// address is the history this tier does not have. Given them, recognising
    /// a spend needs only the outpoint an input names, so the answer is
    /// complete for the range — and carries the location a balance delta is
    /// reported at, which a bare spend status does not.
    fn spends(
        &self,
        outpoints: &[Outpoint],
        range: HeightRange,
    ) -> impl Future<Output = Result<Vec<TransparentSpend>, AddressReadError>> + Send;
}

/// Backed by: spend index.
pub trait SpendRead: Send + Sync {
    fn spend_status(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<SpendStatus, SpendReadError>> + Send;

    /// Where `outpoint` was spent, or `None` if this view holds no spend of it.
    ///
    /// The locating sibling of [`spend_status`](Self::spend_status): that read
    /// answers the three-way "spent / unspent / never-created" question, while
    /// this one carries the spend's coordinates — the consuming transaction, the
    /// input of it that consumed the outpoint, and the height it was mined at —
    /// as a [`TransparentSpend`]. `None` collapses both "unspent" and
    /// "never-created" into one answer, because a caller that needs only the
    /// location (zcashd's `getspentinfo`) treats the two identically.
    fn spend_info(
        &self,
        outpoint: Outpoint,
    ) -> impl Future<Output = Result<Option<TransparentSpend>, SpendReadError>> + Send;
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
