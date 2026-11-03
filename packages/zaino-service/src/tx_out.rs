//! Unspent transparent output lookup, read live from the validator.
//!
//! A control rather than a snapshot read: `gettxout` answers against the
//! validator's *current* UTXO set (optionally including the mempool), which
//! nothing pins to a chain view — the same reason the node-status reads are
//! controls. Zaino indexes no UTXO set of its own, so this is always
//! passthrough. It is **not** [`SpendRead`](crate::SpendRead): that answers
//! whether a known outpoint was spent, a local index question; this answers
//! "is this outpoint unspent right now, and what is in it" against the
//! validator.

use std::future::Future;

use zaino_primitives::types::rpc::TxOut;
use zaino_primitives::types::{OutputIndex, TransactionId};

use crate::error::ReadError;

/// `gettxout`: fetch an unspent transparent output, live.
///
/// `Ok(None)` is the ordinary answer for a **spent or unknown** outpoint — the
/// honest reply to "is this unspent?", not a failure — matching zcashd/zebra,
/// which return JSON `null`. `include_mempool` asks the validator to account for
/// unconfirmed spends, so an output spent only in the mempool reports as absent.
pub trait TxOutRead: Send + Sync {
    /// Fetch the unspent output at `(txid, index)`.
    fn tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> impl Future<Output = Result<Option<TxOut>, ReadError>> + Send;
}
