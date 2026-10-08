//! A transaction as raw serialized bytes, plus where it lives.

use super::TransactionLocation;

/// A transaction served as opaque bytes, the way a light wallet consumes it: it
/// takes the serialized transaction and parses it locally. This is the
/// lightwalletd `GetTransaction` shape (`RawTransaction { data, height }`).
///
/// The pool-decomposed [`Transaction`](super::Transaction) is a distinct,
/// heavier surface for consumers that need the transaction's inner structure (an
/// explorer, a node RPC) rather than its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawTransaction {
    /// The serialized transaction bytes.
    pub data: Vec<u8>,
    /// Where the transaction was found (best chain at a height, a non-best
    /// branch, or the mempool).
    pub location: TransactionLocation,
}
