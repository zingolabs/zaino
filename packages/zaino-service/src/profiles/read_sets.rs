//! Read-sets: *the demand*, named.
//!
//! Which reads a use case pulls through a pinned view — the `required`
//! capability set of the availability model, made first-class. Coherence is
//! orthogonal and asserted once, by [`TakeSnapshot`](crate::TakeSnapshot)'s
//! associated-type bound (`type Snapshot: Snapshot`), so a read-set never names
//! the pin.
//!
//! Each is blanket-implemented: a type *is* a read-set exactly when it has the
//! constituent read capabilities.

use crate::reads::{
    AddressRead, BlockRead, ChainInfoRead, CompactBlockRead, CompactNullifierRead,
    RawTransactionRead, SpendRead, TransactionRead, TreestateRead,
};

/// Reads shared by every wallet-shaped consumer — scan compact blocks, build
/// note-commitment witnesses, track transparent funds, fetch a transaction's
/// bytes. The common base of [`FullWalletReads`] and [`LightWalletReads`], which
/// are siblings over it: extracting the core keeps the two from evolving through
/// each other.
///
/// A wallet takes a transaction as raw bytes ([`RawTransactionRead`]) and parses
/// it locally; the pool-decomposed [`TransactionRead`] is the node/explorer
/// surface ([`NodeRpcReads`]), not a wallet read.
pub trait WalletReadCore:
    CompactBlockRead + TreestateRead + AddressRead + RawTransactionRead
{
}
impl<T> WalletReadCore for T where
    T: CompactBlockRead + TreestateRead + AddressRead + RawTransactionRead
{
}

/// The full-wallet library's read demand. A sibling of [`LightWalletReads`] over
/// [`WalletReadCore`] — full-wallet-only reads land here as its own delta, never
/// on a line the light path shares.
pub trait FullWalletReads: WalletReadCore {}
impl<T> FullWalletReads for T where T: WalletReadCore {}

/// The lightwalletd-compatible read demand. A sibling of [`FullWalletReads`] over
/// [`WalletReadCore`]; its delta is the compact-block nullifier serving variant.
pub trait LightWalletReads: WalletReadCore + CompactNullifierRead {}
impl<T> LightWalletReads for T where T: WalletReadCore + CompactNullifierRead {}

/// The node-RPC / explorer read demand: raw blocks and spend lookups the
/// wallet-shaped consumers never need, plus the chain-info aggregate. A distinct
/// shape, not a wallet delta.
pub trait NodeRpcReads:
    BlockRead + TransactionRead + SpendRead + AddressRead + TreestateRead + ChainInfoRead
{
}
impl<T> NodeRpcReads for T where
    T: BlockRead + TransactionRead + SpendRead + AddressRead + TreestateRead + ChainInfoRead
{
}
