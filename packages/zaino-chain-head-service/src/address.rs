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
    Height, HeightRange, Outpoint, OutputIndex, Transaction, TransparentAddress,
    TransparentReceive, TransparentSpend,
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
            for (block_index, transaction) in enumerated(transactions(block)) {
                receives.extend(paid_outputs(transaction, addr, height, block_index));
            }
        }
        Ok(receives)
    }

    async fn spends(
        &self,
        outpoints: &[Outpoint],
        range: HeightRange,
    ) -> Result<Vec<TransparentSpend>, AddressReadError> {
        let mut spends = Vec::new();
        for block in self.window().best_chain() {
            let height = block.height();
            if !covers(range, height) {
                continue;
            }
            for (block_index, transaction) in enumerated(transactions(block)) {
                spends.extend(consumed(transaction, outpoints, height, block_index));
            }
        }
        Ok(spends)
    }
}

/// Every input of `transaction` (at block position `block_index`) consuming one
/// of `outpoints`, as spends at `height`.
///
/// An input's position in the transaction is its index, so enumeration is the
/// numbering. A consensus-valid chain spends an outpoint once, so each one
/// appears at most once across the window.
fn consumed<'a>(
    transaction: &'a Transaction,
    outpoints: &'a [Outpoint],
    height: Height,
    block_index: u32,
) -> impl Iterator<Item = TransparentSpend> + 'a {
    transaction
        .transparent
        .inputs
        .iter()
        .enumerate()
        .filter_map(move |(index, input)| {
            let outpoint = *outpoints.iter().find(|outpoint| {
                input.prev_txid == outpoint.txid && input.prev_index == outpoint.index
            })?;
            // As in `paid_outputs`: an index past `u32` cannot occur in a block
            // that parsed, and the spend it would describe is unrepresentable.
            Some(TransparentSpend {
                outpoint,
                by: transaction.txid,
                input_index: OutputIndex::try_from(index).ok()?,
                height,
                block_index,
            })
        })
}

/// Whether `range` includes `height`.
fn covers(range: HeightRange, height: Height) -> bool {
    range.start <= height && height <= range.end
}

/// The transactions of a retained block, in block order.
fn transactions(block: &ChainHeadBlock) -> &[Transaction] {
    &block.block.transactions
}

/// A block's transactions paired with their in-block position as a `u32`.
///
/// A block that parsed cannot hold more than `u32::MAX` transactions (its size
/// is consensus-bounded far below that), so the narrowing never drops one in
/// practice; a position past `u32` would describe an unrepresentable location, so
/// that transaction is skipped rather than failing the window's infallible read —
/// the same handling as an out-of-range output index.
fn enumerated(transactions: &[Transaction]) -> impl Iterator<Item = (u32, &Transaction)> {
    transactions
        .iter()
        .enumerate()
        .filter_map(|(index, transaction)| Some((u32::try_from(index).ok()?, transaction)))
}

/// Every output of `transaction` (at block position `block_index`) paying `addr`,
/// as receives at `height`.
///
/// The output's position in the transaction is its index, so enumeration is the
/// numbering rather than something re-derived.
fn paid_outputs<'a>(
    transaction: &'a Transaction,
    addr: &'a TransparentAddress,
    height: Height,
    block_index: u32,
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
                block_index,
            })
        })
}
