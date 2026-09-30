//! block_hash index: one hash per block, read off the header, kept by its own loop
//!
//! - Final block (bulk) → `bulk`, committed per batch; non-final → `non_finalized` (hash → height)
//! - A commit moves hashes through a height from `bulk` / `non_finalized` into the store's segments

use std::{num::NonZeroUsize, sync::Arc};

use imbl::HashMap;
use tokio_util::sync::CancellationToken;
use zaino_persistence::{lsm, StoreError};
use zaino_primitives::types::{Block, BlockRef, Height};
use zaino_sync::{IndexFailed, Offloaded, Published, Step, Subscription, Weight};

use crate::{by_hash::HashKey, BlockHashStore, ReadView, HASH};

/// - `durable` / `segments` = the store as of the last commit (what views pin)
/// - `applied` = last applied height, inclusive (`None` = none)
/// - `bulk` = final blocks not yet committed, `bulk_bytes` their [`Weight`]
pub struct BlockHashIndexWriter {
    store: Offloaded<BlockHashStore>,
    durable: Option<BlockRef>,
    segments: Arc<lsm::Snapshot<HashKey>>,
    applied: Option<Height>,
    non_finalized: HashMap<[u8; HASH], Height>,
    bulk: Vec<Arc<Block>>,
    bulk_bytes: usize,
    batch_bytes: NonZeroUsize,
    published: Published<ReadView>,
}

impl BlockHashIndexWriter {
    pub const NAME: &'static str = "block_hash";

    /// `batch_bytes` = final blocks per bulk commit (one fsync)
    pub fn new(store: BlockHashStore, batch_bytes: NonZeroUsize) -> Self {
        let durable = store.finalized_tip();
        let applied = durable.map(|tip| tip.height);
        let segments = store.reader().pin_segments();
        let view = ReadView::new(HashMap::new(), Arc::clone(&segments));
        Self {
            store: Offloaded::new(store),
            durable,
            segments,
            applied,
            non_finalized: HashMap::new(),
            bulk: Vec::new(),
            bulk_bytes: 0,
            batch_bytes,
            published: Published::new(view, applied),
        }
    }

    /// Last committed block (the producer checks the chain it streams links onto it)
    pub fn durable_tip(&self) -> Option<BlockRef> {
        self.durable
    }

    /// View, tips and gate, for serving, metrics and status (taken before [`run`](Self::run))
    pub fn published(&self) -> &Published<ReadView> {
        &self.published
    }

    /// Follows `blocks` through its `Shutdown`; a failure cancels `cancel` (the pipeline) first
    pub async fn run(
        mut self,
        mut blocks: Subscription<Block>,
        cancel: CancellationToken,
    ) -> Result<(), IndexFailed<StoreError>> {
        let followed = self.follow(&mut blocks).await;
        if followed.is_err() {
            cancel.cancel();
        }
        blocks.skip_to_shutdown().await;
        followed.map_err(|source| IndexFailed { index: Self::NAME, source })
    }

    async fn follow(&mut self, blocks: &mut Subscription<Block>) -> Result<(), StoreError> {
        loop {
            match blocks.next().await {
                Step::Apply { height, finalized: true, data } => {
                    // replay for an index behind this one: already on disk
                    if Some(height) <= self.durable.map(|tip| tip.height) {
                        continue;
                    }
                    self.bulk_bytes += data.weight();
                    self.bulk.push(data);
                    if self.bulk_bytes >= self.batch_bytes.get() {
                        self.commit(height).await?;
                    }
                }
                Step::Apply { height, finalized: false, data } => {
                    // bulk → tip: what bulk staged commits before the first apply builds on it
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height).await?;
                    }
                    let next = self.applied.map_or(Height::GENESIS, Height::next);
                    assert_eq!(height, next, "block_hash: blocks must arrive contiguously");
                    self.non_finalized.insert(data.header().hash.into(), height);
                    self.applied = Some(height);
                }
                Step::Finalized { height } => self.commit(height).await?,
                Step::Reorg => {
                    assert!(self.bulk.is_empty(), "block_hash: reorg with bulk blocks staged");
                    // back to the durable tip, the winning branch applied from there
                    self.non_finalized = HashMap::new();
                    self.applied = self.durable.map(|tip| tip.height);
                    self.publish();
                    self.published.reorged();
                }
                Step::Shutdown => {
                    if let Some(last) = self.bulk.last() {
                        self.commit(last.header().height).await?;
                    }
                    return Ok(());
                }
            }
            self.publish();
        }
    }

    /// Every final block through `through` → disk (bulk ones, then applied ones), then their
    /// hashes leave `bulk` / `non_finalized` for the segments
    async fn commit(&mut self, through: Height) -> Result<(), StoreError> {
        let bulk = std::mem::take(&mut self.bulk);
        self.bulk_bytes = 0;
        let mut rows: Vec<(Height, [u8; HASH])> =
            bulk.iter().map(|block| (block.header().height, block.header().hash.into())).collect();
        let mut applied: Vec<(Height, [u8; HASH])> = self
            .non_finalized
            .iter()
            .filter(|(_, height)| **height <= through)
            .map(|(hash, height)| (*height, *hash))
            .collect();
        applied.sort_unstable();
        rows.extend(applied);

        let mut next = self.durable.map_or(Height::GENESIS, |tip| tip.height.next());
        for (height, _) in &rows {
            assert_eq!(*height, next, "block_hash: final blocks not contiguous from durable");
            next = next.next();
        }
        assert_eq!(
            Some(through),
            next.checked_sub(1),
            "block_hash: final blocks short of {through}"
        );

        let written: Vec<[u8; HASH]> = rows.iter().map(|(_, hash)| *hash).collect();
        self.store.blocking(move |store| store.commit(&rows)).await?;

        let store = self.store.get();
        self.durable = store.finalized_tip();
        self.segments = store.reader().pin_segments();
        for hash in &written {
            self.non_finalized.remove(hash);
        }
        let durable = self.durable.map(|tip| tip.height);
        self.applied = self.applied.max(durable);
        // view first: a reader woken by the durable tip pins the view holding it
        self.publish();
        self.published.durable(durable);
        Ok(())
    }

    fn publish(&self) {
        let view = ReadView::new(self.non_finalized.clone(), Arc::clone(&self.segments));
        self.published.view(view, self.applied);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{BlockHash, BlockHeader, BlockRef, Transaction, TransactionId};
    use zaino_sync::BlockSink;
    use zcash_protocol::consensus::NetworkType;

    use super::*;

    /// Block `height` hashing to `[hash; 32]`, its parent `[parent; 32]`
    fn block(height: u32, hash: u8, parent: u8) -> Arc<Block> {
        Arc::new(Block::new(
            BlockHeader::for_tests(height, [hash; 32], [parent; 32], 1_700_000_000 + height),
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

    /// Steps sent as the producer sends them: bulk, the tip above it, a block finalized, a losing
    /// branch reorged away, a reopen; every published view locates exactly the winning chain's
    /// hashes, finalized or not
    #[tokio::test]
    async fn locates_both_tiers_across_a_finalize_a_reorg_and_a_reopen() {
        let fs = SimFs::new();
        let open = |fs: &Arc<SimFs>| {
            BlockHashStore::open(fs.clone(), Path::new("/bh"), NetworkType::Regtest).expect("open")
        };
        let batch = NonZeroUsize::new(1 << 20).expect("non-zero");
        let index = BlockHashIndexWriter::new(open(&fs), batch);
        let served = index.published().served();
        let (mut durable, mut applied) =
            (index.published().subscribe_finalized(), index.published().subscribe_applied());
        let located = |hashes: &[u8]| -> Vec<Option<Height>> {
            let view = served.pin_any();
            hashes.iter().map(|hash| view.height_of_hash(&[*hash; 32])).collect()
        };
        let mut sink = BlockSink::new("blocks");
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let blocks = sink.subscribe(BlockHashIndexWriter::NAME, queue);
        let running = tokio::spawn(index.run(blocks, CancellationToken::new()));
        let apply = |height, finalized, hash, parent| Step::Apply {
            height: h(height),
            finalized,
            data: block(height, hash, parent),
        };
        let within = std::time::Duration::from_secs(5);
        let through = |n| Some(h(n));

        for step in [apply(0, true, 10, 0), apply(1, true, 11, 10)] {
            sink.send(step).await;
        }
        for step in [apply(2, false, 12, 11), apply(3, false, 0xee, 12)] {
            sink.send(step).await;
        }
        let tip = tokio::time::timeout(within, applied.wait_for(|at| *at == through(3))).await;
        tip.expect("tip applied").expect("index alive");
        assert_eq!(*durable.borrow(), through(1), "bulk written before the tip applies on it");
        let both = located(&[10, 11, 12, 0xee]);
        assert_eq!(both, [through(0), through(1), through(2), through(3)], "both tiers");

        sink.send(Step::Finalized { height: h(2) }).await;
        let two = tokio::time::timeout(within, durable.wait_for(|at| *at == through(2))).await;
        two.expect("2 written once final").expect("index alive");
        // 3 = a losing branch: dropped, the winner replayed from the first non-final height
        sink.send(Step::Reorg).await;
        sink.send(apply(3, false, 13, 12)).await;
        sink.shutdown();
        running.await.expect("no panic").expect("followed through Shutdown");
        assert_eq!(located(&[12, 13, 0xee]), [through(2), through(3), None], "losing branch gone");

        let index = BlockHashIndexWriter::new(open(&fs), batch);
        let tip_12 = Some(BlockRef { hash: BlockHash::from([12; 32]), height: h(2) });
        assert_eq!(index.durable_tip(), tip_12, "resumes at the durable tip");
        let view = index.published().served().pin_any();
        let reopened = [10, 11, 12, 13].map(|hash| view.height_of_hash(&[hash; 32]));
        assert_eq!(reopened, [through(0), through(1), through(2), None], "non-finalized gone");
    }
}
