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

use crate::reads::{
    AddressRead, BlockHashRead, BlockRead, BlockVerboseRead, ChainInfoRead, CompactBlockRead,
    CompactNullifierRead, RawTransactionRead, TransactionRead, TransactionViewRead, TreestateRead,
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
/// composed headers.
///
/// Spend status (`SpendRead`) is deliberately absent. None of the methods this
/// set serves needs an outpoint's spend state: the explorer is served
/// passthrough, and spend status has only a local implementation, so requiring
/// it would pin the set to a capability no served method consumes and no
/// passthrough tier provides. It returns when the served `gettxout` /
/// `getspentinfo` surface lands together with the tier spend-status index, at
/// which point this set gains `SpendRead` as its own addition.
pub trait NodeRpcReads:
    BlockRead
    + BlockVerboseRead
    + BlockHashRead
    + TransactionRead
    + TransactionViewRead
    + RawTransactionRead
    + AddressRead
    + TreestateRead
    + ChainInfoRead
{
}
impl<T> NodeRpcReads for T where
    T: BlockRead
        + BlockVerboseRead
        + BlockHashRead
        + TransactionRead
        + TransactionViewRead
        + RawTransactionRead
        + AddressRead
        + TreestateRead
        + ChainInfoRead
{
}
