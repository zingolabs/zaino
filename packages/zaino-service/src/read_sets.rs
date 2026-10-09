//! Read-sets: bundles of read capabilities, named.
//!
//! A read-set is the pinned half of a use case's demand — the reads it pulls
//! through one coherent view. It is a bundle of capabilities, not a use case:
//! two use cases may share one ([`WalletReadCore`] backs both wallet shapes),
//! and coherence is orthogonal, asserted once by
//! [`TakeSnapshot`](crate::TakeSnapshot)'s associated-type bound, so a read-set
//! never names the pin.
//!
//! Each is blanket-implemented: a type *is* a read-set exactly when it has the
//! constituent reads.

use crate::block_deltas::BlockDeltasRead;
use crate::reads::{
    AddressRead, BlockHashRead, BlockRead, BlockVerboseRead, ChainInfoRead, ChainTipsRead,
    CompactBlockRead, CompactNullifierRead, RawTransactionRead, SpendRead, TransactionRead,
    TransactionViewRead, TreestateRead,
};

/// Reads shared by every wallet-shaped consumer — scan compact blocks, build
/// note-commitment witnesses, track transparent funds, fetch a transaction's
/// bytes. The common base of [`FullWalletReads`] and [`LightWalletReads`],
/// which are siblings over it: extracting the core keeps the two from evolving
/// through each other.
///
/// A wallet takes a transaction as raw bytes ([`RawTransactionRead`]) and parses
/// it locally, so it needs no pool-decomposed form. The node/explorer surface
/// ([`NodeRpcReads`]) needs *both*: `getrawtransaction` is one RPC with a
/// verbosity parameter, raw bytes at 0 and the decoded transaction at 1.
pub trait WalletReadCore:
    CompactBlockRead + TreestateRead + AddressRead + RawTransactionRead
{
}
impl<T> WalletReadCore for T where
    T: CompactBlockRead + TreestateRead + AddressRead + RawTransactionRead
{
}

/// The full-wallet read demand. A sibling of [`LightWalletReads`] over
/// [`WalletReadCore`] — full-wallet-only reads land here as its own delta,
/// never on a line the light path shares.
pub trait FullWalletReads: WalletReadCore {}
impl<T> FullWalletReads for T where T: WalletReadCore {}

/// The light-wallet read demand. A sibling of [`FullWalletReads`] over
/// [`WalletReadCore`]; its delta is the compact-block nullifier serving
/// variant.
pub trait LightWalletReads: WalletReadCore + CompactNullifierRead {}
impl<T> LightWalletReads for T where T: WalletReadCore + CompactNullifierRead {}

/// The node-RPC / explorer read demand: raw blocks the wallet-shaped consumers
/// never need, plus the chain-info aggregate. A distinct shape, not a wallet
/// delta.
///
/// Carries both transaction reads. They are not alternatives: one RPC serves
/// raw bytes at verbosity 0 and the decoded transaction at verbosity 1, so a
/// surface that speaks zcashd's contract needs both forms. It also carries
/// [`BlockVerboseRead`] alongside [`BlockRead`]: the block page composes the two
/// (the block's contents plus its chain position) into one `getblock` response.
/// [`TransactionViewRead`] is the resolved-transaction surface over these:
/// `getrawtransaction <txid> 1` and `getblock <block> 2` render each transparent
/// input with the output it spends, which only this read resolves.
/// [`BlockHashRead`] is the timestamp-range block selection behind
/// `getblockhashes` — the explorer's block-list keystone, served locally over the
/// composed headers. [`BlockDeltasRead`] is the composed `getblockdeltas` surface:
/// an indexer-only method (Zebra answers `-32601`) assembled from the block,
/// verbose, resolved-transaction and header reads already in this set — it adds no
/// source port.
///
/// [`ChainTipsRead`] is the `getchaintips` surface: a third indexer-only method
/// (Zebra answers `-32601`), read from the non-finalised head's retained graph.
///
/// [`SpendRead`] is the `getspentinfo` surface: another indexer-only method
/// (Zebra answers `-32601`) that locates where a transparent outpoint was spent.
/// It is served **locally** from the tier spends index, which both the finalised
/// store and the non-finalised head build and read; the composer threads the two
/// across the watermark so a spend above it of an output created below it reports
/// the spending height. It adds no source port — a validator with no "who spent
/// this outpoint" method is exactly why the read is local.
pub trait NodeRpcReads:
    BlockRead
    + BlockVerboseRead
    + BlockHashRead
    + BlockDeltasRead
    + TransactionRead
    + TransactionViewRead
    + RawTransactionRead
    + AddressRead
    + SpendRead
    + TreestateRead
    + ChainInfoRead
    + ChainTipsRead
{
}
impl<T> NodeRpcReads for T where
    T: BlockRead
        + BlockVerboseRead
        + BlockHashRead
        + BlockDeltasRead
        + TransactionRead
        + TransactionViewRead
        + RawTransactionRead
        + AddressRead
        + SpendRead
        + TreestateRead
        + ChainInfoRead
        + ChainTipsRead
{
}
