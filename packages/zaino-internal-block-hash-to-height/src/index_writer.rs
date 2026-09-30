//! [`IndexWriter`]: one hash per block, read off the header

use std::sync::Arc;

use imbl::HashMap;
use zaino_persistence::{lsm, StoreError};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_sync::{IndexWriter, Offloaded};

use crate::{by_hash::HashKey, BlockHashStore, ReadView, HASH};

/// - `durable`, `segments` = the store as of the last `committed` (answered without the store
///   while a write has it)
/// - `durable` = last committed block, `applied` = last applied height (both inclusive; `None` =
///   none)
pub struct BlockHashIndexWriter {
    store: Offloaded<BlockHashStore>,
    durable: Option<BlockRef>,
    segments: Arc<lsm::Snapshot<HashKey>>,
    applied: Option<Height>,
    non_finalized: HashMap<[u8; HASH], Height>,
}

impl BlockHashIndexWriter {
    pub fn new(store: BlockHashStore) -> Self {
        let durable = store.finalized_tip();
        Self {
            durable,
            segments: store.reader().pin_segments(),
            applied: durable.map(|tip| tip.height),
            store: Offloaded::new(store),
            non_finalized: HashMap::new(),
        }
    }
}

impl IndexWriter for BlockHashIndexWriter {
    type Input = Block;
    type View = ReadView;
    type Error = StoreError;
    /// The store back, plus the hashes it now holds
    type Done = (BlockHashStore, Vec<[u8; HASH]>);

    const NAME: &'static str = "block_hash";

    fn finalized_tip(&self) -> Option<BlockRef> {
        self.durable
    }

    fn applied_height(&self) -> Option<Height> {
        self.applied
    }

    fn view(&self) -> ReadView {
        ReadView::new(self.non_finalized.clone(), Arc::clone(&self.segments))
    }

    async fn apply(&mut self, block: &Arc<Block>) -> Result<(), StoreError> {
        let height = block.header().height;
        let next = self.applied.map_or(Height::GENESIS, Height::next);
        assert_eq!(height, next, "block_hash: blocks must arrive contiguously");
        self.non_finalized.insert(block.header().hash.into(), height);
        self.applied = Some(height);
        Ok(())
    }

    async fn finalize(
        &mut self,
        blocks: &[Arc<Block>],
    ) -> Result<impl FnOnce() -> Result<Self::Done, StoreError> + Send + 'static, StoreError> {
        let finalised: Vec<(Height, [u8; HASH])> = blocks
            .iter()
            .map(|block| (block.header().height, block.header().hash.into()))
            .collect();
        let mut store = self.store.lend();
        Ok(move || {
            store.commit(&finalised)?;
            Ok((store, finalised.into_iter().map(|(_, hash)| hash).collect()))
        })
    }

    async fn committed(&mut self, (store, written): Self::Done) -> Result<(), StoreError> {
        self.durable = store.finalized_tip();
        self.segments = store.reader().pin_segments();
        self.store.restore(store);
        for hash in &written {
            self.non_finalized.remove(hash);
        }

        // non-finalized entry at or below the new tip = another branch's hash at a now-final height
        let durable = self.finalized_height();
        let stale = self.non_finalized.values().any(|&height| Some(height) <= durable);
        assert!(!stale, "block_hash: finalised over another branch's non-finalized block");
        self.applied = self.applied.max(durable);
        Ok(())
    }

    async fn reset(&mut self) -> Result<(), StoreError> {
        self.non_finalized = HashMap::new();
        self.applied = self.finalized_height();
        Ok(())
    }

    fn wants_commit(&self) -> bool {
        self.store.get().merge_finished()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{BlockHash, BlockHeader, Transaction, TransactionId};
    use zcash_protocol::consensus::NetworkType;

    use super::*;

    fn block(height: u32, hash: u8) -> Arc<Block> {
        Arc::new(Block::new(
            BlockHeader::for_tests(height, [hash; 32], [0; 32], 1_700_000_000 + height),
            vec![Transaction {
                txid: TransactionId::from([hash; 32]),
                transparent: Default::default(),
                sprout: Default::default(),
                sapling: Default::default(),
                orchard: Default::default(),
                ironwood: Default::default(),
            }],
        ))
    }

    fn h(n: u32) -> Height {
        Height::try_from(n).expect("h")
    }

    /// Bulk finalise, apply above it, a losing branch reset away, a write landed late, a reopen:
    /// every view locates exactly the winning chain's hashes, finalised or not, in one tier
    #[tokio::test]
    async fn locates_both_tiers_across_a_reset_a_late_landing_and_a_reopen() {
        let fs = SimFs::new();
        let open = |fs: &Arc<SimFs>| {
            BlockHashStore::open(fs.clone(), Path::new("/bh"), NetworkType::Regtest).expect("open")
        };
        let mut writer = BlockHashIndexWriter::new(open(&fs));

        zaino_sync::finalize_now(&mut writer, &[block(0, 10), block(1, 11)])
            .await
            .expect("bulk finalize");
        writer.apply(&block(2, 12)).await.expect("apply");
        writer.apply(&block(3, 0xee)).await.expect("losing branch");
        let through = |n| Some(h(n));
        let tip_12 = Some(BlockRef { hash: BlockHash::from([12; 32]), height: h(2) });
        assert_eq!((writer.finalized_height(), writer.applied_height()), (through(1), through(3)));
        let located = [10, 11, 12, 0xee].map(|hash| writer.view().height_of_hash(&[hash; 32]));
        assert_eq!(located, [Some(h(0)), Some(h(1)), Some(h(2)), Some(h(3))], "both tiers locate");

        writer.reset().await.expect("reset");
        assert_eq!(writer.applied_height(), through(1));
        writer.apply(&block(2, 12)).await.expect("reapply");
        writer.apply(&block(3, 13)).await.expect("winning branch");
        let write = writer.finalize(&[block(2, 12)]).await.expect("finalize applied");
        let done = write().expect("written");
        assert_eq!(writer.finalized_height(), through(1), "durable moves only on landing");
        let unlanded = writer.view().height_of_hash(&[12; 32]);
        assert_eq!(unlanded, Some(h(2)), "written, not landed: still non-finalized");
        writer.committed(done).await.expect("landed");
        let located = [12, 13, 0xee].map(|hash| writer.view().height_of_hash(&[hash; 32]));
        assert_eq!(located, [Some(h(2)), Some(h(3)), None], "losing branch gone");
        assert_eq!((writer.applied_height(), writer.finalized_tip()), (through(3), tip_12));

        drop(writer);
        let writer = BlockHashIndexWriter::new(open(&fs));
        let resumed = (writer.applied_height(), writer.finalized_tip());
        assert_eq!(resumed, (through(2), tip_12), "resumes at the durable tip");
        let located = [10, 11, 12, 13].map(|hash| writer.view().height_of_hash(&[hash; 32]));
        assert_eq!(located, [Some(h(0)), Some(h(1)), Some(h(2)), None], "non-finalized gone");
    }
}
