//! The receive side of address history, over the non-finalised window.
//!
//! The window holds the outputs of every block it retains, and an output names
//! its recipient in its script, so every receive inside the window is
//! answerable here. A *spend* is not: a transparent input carries the outpoint
//! it consumes and nothing else, so attributing one to an address needs the
//! output that outpoint created, which the window does not have once that
//! output predates its floor.
//!
//! That is why this tier implements [`AddressReceiveRead`] rather than
//! `AddressRead`. The composer joins it with [`SpendRead`](zaino_service::SpendRead)
//! — "was this outpoint spent in your range", which the window answers by
//! outpoint and needs no history for — and supplies the owned-outpoint set from
//! the finalised store.
//!
//! Infallible, like the window's other reads: the retained chain is in memory.

use zaino_address::script_pays;
use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot};
use zaino_primitives::types::{
    Height, HeightRange, OutputIndex, Transaction, TransparentAddress, TransparentReceive,
};
use zaino_service::error::AddressReadError;
use zaino_service::AddressReceiveRead;

use crate::serve::HeadSnapshot;

impl AddressReceiveRead for HeadSnapshot {
    async fn receives(
        &self,
        addr: &TransparentAddress,
        range: HeightRange,
    ) -> Result<Vec<TransparentReceive>, AddressReadError> {
        let mut receives = Vec::new();
        // The best chain ascends, and a block's transactions are in block order,
        // so appending as we walk yields height order without a sort.
        for block in self.window().best_chain() {
            let height = block.height();
            if !covers(range, height) {
                continue;
            }
            for transaction in transactions(block) {
                receives.extend(paid_outputs(transaction, addr, height));
            }
        }
        Ok(receives)
    }
}

/// Whether `range` includes `height`.
fn covers(range: HeightRange, height: Height) -> bool {
    range.start <= height && height <= range.end
}

/// The transactions of a retained block, in block order.
fn transactions(block: &ChainHeadBlock) -> &[Transaction] {
    &block.block.transactions
}

/// Every output of `transaction` paying `addr`, as receives at `height`.
///
/// The output's position in the transaction is its index, so enumeration is the
/// numbering rather than something re-derived.
fn paid_outputs<'a>(
    transaction: &'a Transaction,
    addr: &'a TransparentAddress,
    height: Height,
) -> impl Iterator<Item = TransparentReceive> + 'a {
    transaction
        .transparent
        .outputs
        .iter()
        .enumerate()
        .filter(|(_, output)| script_pays(output.script.as_bytes(), addr))
        .filter_map(move |(index, output)| {
            // An output index past `u32` cannot occur in a block that parsed,
            // since the count came from a CompactSize narrowed at the parse
            // boundary. Dropping rather than failing keeps the window's reads
            // infallible, and the receive it would describe is unrepresentable.
            let output_index = OutputIndex::try_from(index).ok()?;
            Some(TransparentReceive {
                txid: transaction.txid,
                output_index,
                script: output.script.clone(),
                value: output.value,
                height,
            })
        })
}
