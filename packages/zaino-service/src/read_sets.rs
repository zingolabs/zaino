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
    AddressRead, BlockRead, ChainInfoRead, CompactBlockRead, CompactNullifierRead,
    RawTransactionRead, SpendRead, TransactionRead, TreestateRead,
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

/// The node-RPC / explorer read demand: raw blocks and spend lookups the
/// wallet-shaped consumers never need, plus the chain-info aggregate. A
/// distinct shape, not a wallet delta.
///
/// Carries both transaction reads. They are not alternatives: one RPC serves
/// raw bytes at verbosity 0 and the decoded transaction at verbosity 1, so a
/// surface that speaks zcashd's contract needs both forms.
pub trait NodeRpcReads:
    BlockRead
    + TransactionRead
    + RawTransactionRead
    + SpendRead
    + AddressRead
    + TreestateRead
    + ChainInfoRead
{
}
impl<T> NodeRpcReads for T where
    T: BlockRead
        + TransactionRead
        + RawTransactionRead
        + SpendRead
        + AddressRead
        + TreestateRead
        + ChainInfoRead
{
}
