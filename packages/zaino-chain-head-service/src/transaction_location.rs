//! Where a transaction sits in the non-finalised window.
//!
//! The window is the only tier that holds displaced branches, so it is the only
//! one that can answer "mined, but not on the best chain". A canonical position
//! outranks a displaced one: the same transaction is usually re-mined on the
//! branch that won.

use zaino_chain_head::ChainHeadTransactionService;
use zaino_primitives::types::{TransactionId, TransactionLocation};
use zaino_service::error::TxReadError;
use zaino_service::TransactionLocationRead;

use crate::serve::HeadSnapshot;

impl TransactionLocationRead for HeadSnapshot {
    async fn transaction_location(
        &self,
        id: TransactionId,
    ) -> Result<Option<TransactionLocation>, TxReadError> {
        let locations = self.window().transaction_locations(&id);
        Ok(match locations.best_chain {
            Some(position) => Some(TransactionLocation::BestChain(position.block.height)),
            None if !locations.non_best_chain.is_empty() => Some(TransactionLocation::NonBestChain),
            None => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zaino_primitives::types::{BlockHash, BlockRef, TransactionId, TransactionLocation};
    use zaino_service::TransactionLocationRead;

    use crate::graph::ChainGraph;
    use crate::serve::HeadSnapshot;
    use crate::snapshot::MapBackedSnapshot;
    use crate::spend::tests::{block, height, outpoint, transaction};

    fn txid(byte: u8) -> TransactionId {
        TransactionId::from([byte; 32])
    }

    /// A window where height 1 was mined twice: block `0x01` carrying `0xA1` and
    /// `0xC1`, then displaced by block `0x11` carrying `0xB1` and `0xC1` again.
    fn reorged_window() -> HeadSnapshot {
        let mut graph = MapBackedSnapshot::from_initial_block(block(0, 0x00, 0xFF, vec![]));
        graph
            .extend(block(
                1,
                0x01,
                0x00,
                vec![
                    transaction(0xA1, vec![outpoint(0xD0, 0)], 1),
                    transaction(0xC1, vec![outpoint(0xD1, 0)], 1),
                ],
            ))
            .expect("height 1 extends the tip");
        graph
            .rewind_to(BlockRef {
                hash: BlockHash::from([0x00; 32]),
                height: height(0),
            })
            .expect("height 0 is on the best chain");
        graph
            .extend(block(
                1,
                0x11,
                0x00,
                vec![
                    transaction(0xB1, vec![outpoint(0xD2, 0)], 1),
                    transaction(0xC1, vec![outpoint(0xD1, 0)], 1),
                ],
            ))
            .expect("the replacement extends the tip");
        HeadSnapshot::over(Arc::new(graph))
    }

    /// A transaction on the winning branch is on the best chain at its height;
    /// one only on the displaced branch is mined but not on the best chain; one
    /// mined on both is reported where the best chain has it; one the window
    /// never saw is not located.
    #[tokio::test]
    async fn the_window_tells_the_best_chain_from_a_displaced_branch() {
        let window = reorged_window();
        let locate = |byte: u8| {
            let window = window.clone();
            async move {
                window
                    .transaction_location(txid(byte))
                    .await
                    .expect("the read succeeds")
            }
        };

        assert_eq!(
            locate(0xB1).await,
            Some(TransactionLocation::BestChain(height(1)))
        );
        assert_eq!(locate(0xA1).await, Some(TransactionLocation::NonBestChain));
        assert_eq!(
            locate(0xC1).await,
            Some(TransactionLocation::BestChain(height(1)))
        );
        assert_eq!(locate(0xEE).await, None);
    }
}
