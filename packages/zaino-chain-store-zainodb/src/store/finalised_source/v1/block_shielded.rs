//! FinalisedState::V1 shielded block reads.

use super::*;

impl DbV1 {
    /// Fetches the ironwood transaction list at a stored `height`, empty when the block has no ironwood row.
    pub(crate) async fn get_block_ironwood(
        &self,
        height: Height,
    ) -> Result<OrchardTxList, StoreError> {
        // The ironwood table is sparse, so only a stored height reads an absent row as "no data".
        self.resolve_stored_height(HashOrHeight::Height(height.into()))
            .await?;
        Ok(self
            .read_row_at_height(self.ironwood, "ironwood", height)
            .await?
            .unwrap_or_else(|| OrchardTxList::new(Vec::new())))
    }
}
