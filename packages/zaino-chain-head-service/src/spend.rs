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
use zaino_primitives::types::{Outpoint, OutputIndex, Transaction, TransparentSpend};
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

    async fn spend_info(
        &self,
        outpoint: Outpoint,
    ) -> Result<Option<TransparentSpend>, SpendReadError> {
        for block in self.window().best_chain() {
            let height = block.height();
            for transaction in transactions(block) {
                if let Some(spend) = spend_of(transaction, outpoint, height) {
                    // A consensus-valid chain spends an outpoint once, so the
                    // first match in best-chain order is the spend.
                    return Ok(Some(spend));
                }
            }
        }
        Ok(None)
    }
}

/// The spend of `outpoint` in `transaction` at `height`, if one of its inputs
/// consumes it. An input's position is its index, so enumeration is the
/// numbering; an index past the wire limit cannot occur in a block that parsed.
fn spend_of(
    transaction: &Transaction,
    outpoint: Outpoint,
    height: zaino_primitives::types::Height,
) -> Option<TransparentSpend> {
    transaction
        .transparent
        .inputs
        .iter()
        .enumerate()
        .find(|(_, input)| input.prev_txid == outpoint.txid && input.prev_index == outpoint.index)
        .and_then(|(index, _)| {
            Some(TransparentSpend {
                outpoint,
                by: transaction.txid,
                input_index: OutputIndex::try_from(index).ok()?,
                height,
            })
        })
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_chain_head::{ChainHeadBlock, ChainHeadWork};
    use zaino_primitives::types::{
        Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, CompactDifficulty,
        EquihashSolution, Height, MerkleRoot, Outpoint, Script, TransactionId, TransparentData,
        TransparentInput, TransparentOutput, TreeRoots, Zatoshis,
    };
    use zaino_service::{SpendRead, SpendStatus};

    use crate::graph::ChainGraph;
    use crate::serve::HeadSnapshot;
    use crate::snapshot::MapBackedSnapshot;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("a valid test height")
    }

    fn outpoint(txid_byte: u8, index: u32) -> Outpoint {
        Outpoint {
            txid: TransactionId::from([txid_byte; 32]),
            index,
        }
    }

    /// A transparent output of `zats`, with an inert script.
    fn output(zats: u64) -> TransparentOutput {
        TransparentOutput {
            value: Zatoshis::new(zats).expect("a valid amount"),
            script: Script::new(vec![0x51]),
        }
    }

    /// A transaction that spends `inputs` and creates `outputs` outputs.
    fn transaction(
        txid_byte: u8,
        inputs: Vec<Outpoint>,
        outputs: usize,
    ) -> zaino_primitives::types::Transaction {
        zaino_primitives::types::Transaction {
            txid: TransactionId::from([txid_byte; 32]),
            transparent: TransparentData {
                inputs: inputs
                    .into_iter()
                    .map(|o| TransparentInput {
                        prev_txid: o.txid,
                        prev_index: o.index,
                    })
                    .collect(),
                outputs: (0..outputs)
                    .map(|i| output(1_000 + u64::try_from(i).expect("a small index")))
                    .collect(),
            },
            sapling: Default::default(),
            orchard: Default::default(),
            ironwood: Default::default(),
        }
    }

    /// A chain-head block at `h` with hash `[hash_byte; 32]`, parent
    /// `[parent_byte; 32]`, carrying `transactions`.
    fn block(
        h: u32,
        hash_byte: u8,
        parent_byte: u8,
        transactions: Vec<zaino_primitives::types::Transaction>,
    ) -> ChainHeadBlock {
        let hash = BlockHash::from([hash_byte; 32]);
        let parent_hash = BlockHash::from([parent_byte; 32]);
        ChainHeadBlock {
            reference: zaino_primitives::types::BlockRef {
                hash,
                height: height(h),
            },
            parent_hash,
            work: ChainHeadWork::anchored_at(u128::from(h)),
            block: Block {
                header: BlockHeader {
                    hash,
                    version: 4,
                    prev_hash: parent_hash,
                    height: height(h),
                    time: 1_000 + h,
                    merkle_root: MerkleRoot::from([0; 32]),
                    block_commitments: BlockCommitments::from([0; 32]),
                    bits: CompactDifficulty::try_from_bits(0x2007_ffff).expect("valid nBits"),
                    nonce: [0; 32],
                    solution: EquihashSolution::Regtest([0; 36]),
                },
                transactions,
                chain_metadata: ChainMetadata::ZERO,
            },
            tree_roots: TreeRoots {
                sapling: None,
                orchard: None,
                ironwood: None,
            },
        }
    }

    /// A window `[0, 1]` where height 1 mines a transaction that spends two
    /// outpoints — the target at input index 1 — and creates one output.
    fn window() -> HeadSnapshot {
        let spender = transaction(0xB2, vec![outpoint(0xD0, 0), outpoint(0xA1, 0)], 1);
        let mut graph = MapBackedSnapshot::from_initial_block(block(0, 0x00, 0xFF, vec![]));
        graph
            .extend(block(1, 0x01, 0x00, vec![spender]))
            .expect("height 1 extends the tip");
        HeadSnapshot::over(Arc::new(graph))
    }

    /// `spend_info` locates a spend in the window: the spending txid, the input
    /// index that consumed the outpoint (the second input, so not a trivial zero),
    /// and the block height.
    #[tokio::test]
    async fn spend_info_locates_a_spend_in_the_window() {
        let spend = window()
            .spend_info(outpoint(0xA1, 0))
            .await
            .expect("the read succeeds")
            .expect("the outpoint was spent in the window");
        assert_eq!(spend.outpoint, outpoint(0xA1, 0));
        assert_eq!(spend.by, TransactionId::from([0xB2; 32]));
        assert_eq!(spend.input_index, 1);
        assert_eq!(spend.height, height(1));
    }

    /// An outpoint the window never spent — created-but-unspent here, or
    /// out-of-window entirely — locates nothing.
    #[tokio::test]
    async fn spend_info_is_none_for_an_unspent_or_out_of_window_outpoint() {
        let window = window();
        assert_eq!(
            window
                .spend_info(outpoint(0xB2, 0))
                .await
                .expect("the read succeeds"),
            None,
            "the output the window created is unspent here",
        );
        assert_eq!(
            window
                .spend_info(outpoint(0xEE, 9))
                .await
                .expect("the read succeeds"),
            None,
            "an outpoint no block in the window touched",
        );
    }

    /// The pre-existing `spend_status`, over the same window: a spent outpoint,
    /// an output created-but-unspent in the window, and an outpoint no block
    /// touched.
    #[tokio::test]
    async fn spend_status_reports_the_three_states() {
        let window = window();
        assert_eq!(
            window
                .spend_status(outpoint(0xA1, 0))
                .await
                .expect("the read succeeds"),
            SpendStatus::Spent {
                by: TransactionId::from([0xB2; 32])
            },
        );
        assert_eq!(
            window
                .spend_status(outpoint(0xB2, 0))
                .await
                .expect("the read succeeds"),
            SpendStatus::Unspent,
            "the window created this output and spent nothing of it",
        );
        assert_eq!(
            window
                .spend_status(outpoint(0xEE, 9))
                .await
                .expect("the read succeeds"),
            SpendStatus::NoSuchOutput,
        );
    }
}
