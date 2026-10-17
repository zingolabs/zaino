//! A transparent output paid to an address, spent or not.

use super::{Height, OutputIndex, Script, TransactionId, TransparentAddress, Utxo, Zatoshis};

/// One transparent output paying an address, at the location that created it.
///
/// Distinct from [`Utxo`], which asserts the output is *unspent*. A receive
/// makes no such claim: it is the creation event alone, and whether the output
/// still exists is a separate question answered by a spend read. A tier that can
/// see outputs but cannot resolve spends — the volatile window, which holds no
/// history behind itself — can report receives honestly and nothing more.
///
/// The address is not repeated here: a receive is only ever produced in answer
/// to a query about one address, so the field would restate the question.
/// [`into_utxo`](Self::into_utxo) supplies it when the caller has established
/// the output is unspent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentReceive {
    /// The transaction that paid the address.
    pub txid: TransactionId,
    /// Output index within that transaction.
    pub output_index: OutputIndex,
    /// The output script, which is what locked the value to the address.
    pub script: Script,
    /// Amount received, in zatoshis.
    pub value: Zatoshis,
    /// Block height that mined the paying transaction.
    pub height: Height,
    /// Position of the paying transaction within its block.
    ///
    /// The receive is local data — the tier that reports it holds the block — so
    /// this is the real block position, never a placeholder. It is the same
    /// `txindex` zcashd keys `getaddresstxids`/`getaddressdeltas` ordering on, so
    /// a caller merging addresses can break same-height ties by block position.
    pub block_index: u32,
}

impl TransparentReceive {
    /// This receive as an unspent output of `address`.
    ///
    /// The caller asserts unspentness by calling this: a receive carries no
    /// such claim, so establishing it — asking every tier that could hold a
    /// spend — happens before the conversion, not inside it.
    pub fn into_utxo(self, address: TransparentAddress) -> Utxo {
        Utxo {
            address,
            txid: self.txid,
            output_index: self.output_index,
            script: self.script,
            satoshis: self.value,
            height: self.height,
        }
    }
}
