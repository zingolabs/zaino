//! Spend status over the non-finalised window.
//!
//! The volatile half of the `SpendStatus` capability. Every answer is a walk of
//! the retained best chain, which is in memory, so none of it can fail — the
//! same reason the window's block reads are infallible.
//!
//! # What the window can say, and what it cannot
//!
//! A transparent input carries the outpoint it spends and nothing else — no
//! script, so not the address it paid. That makes *address* history
//! unanswerable here for an output created below the watermark, but spend
//! status is keyed by the outpoint itself, so the window answers it in full:
//!
//! ```text
//! ∃ input in window spending o   → Spent { by }
//! ∃ tx in window creating o      → Unspent
//! otherwise                      → NoSuchOutput
//! ```
//!
//! `NoSuchOutput` is the window saying "not mine", not "nowhere": an outpoint
//! created at or below the watermark is simply not in this tier. The composer
//! reads it that way — `zaino_core`'s `Local` spend placement asks the window
//! first, because a spend here is the newer fact, and falls through to the
//! finalised store on anything but a spend.

use zaino_chain_head::{ChainHeadBlock, ChainHeadSnapshot};
use zaino_primitives::types::{Outpoint, Transaction};
use zaino_service::error::SpendReadError;
use zaino_service::{SpendRead, SpendStatus};

use crate::serve::HeadSnapshot;

impl SpendRead for HeadSnapshot {
    async fn spend_status(&self, outpoint: Outpoint) -> Result<SpendStatus, SpendReadError> {
        let mut created = false;
        for block in self.window().best_chain() {
            for transaction in transactions(block) {
                if spends(transaction, outpoint) {
                    // A spend is the decisive fact and the window holds at most
                    // one, so stop at the first.
                    return Ok(SpendStatus::Spent {
                        by: transaction.txid,
                    });
                }
                // Creation is not decisive on its own: a later block in the
                // window may still spend it, so record and keep walking.
                created |= creates(transaction, outpoint);
            }
        }
        Ok(match created {
            true => SpendStatus::Unspent,
            false => SpendStatus::NoSuchOutput,
        })
    }
}

/// The transactions of a retained block, in block order.
fn transactions(block: &ChainHeadBlock) -> &[Transaction] {
    &block.block.transactions
}

/// Whether `transaction` spends `outpoint`.
fn spends(transaction: &Transaction, outpoint: Outpoint) -> bool {
    transaction
        .transparent
        .inputs
        .iter()
        .any(|input| input.prev_txid == outpoint.txid && input.prev_index == outpoint.index)
}

/// Whether `transaction` is the one that created `outpoint`.
fn creates(transaction: &Transaction, outpoint: Outpoint) -> bool {
    if transaction.txid != outpoint.txid {
        return false;
    }
    usize::try_from(outpoint.index).is_ok_and(|index| index < transaction.transparent.outputs.len())
}
