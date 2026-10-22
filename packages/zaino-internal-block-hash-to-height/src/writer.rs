//! Block hash → height writer: the finalized stream → one [`fold`] per block → its store

use std::time::{Duration, Instant};

use zaino_persistence::{BlockChanges, MapRead, Store};
use zaino_primitives::types::Block;
use zaino_sync::{
    apply, assert_next, blocking, commit, held, ran, IndexHandle, IndexPublisher, Subscription,
};

use crate::{
    by_hash::{encode_height, BY_HASH},
    BlockHashReader,
};

pub struct BlockHashIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: MapRead>> BlockHashIndexWriter<S> {
    /// Over `store` (opened with [`schema`](crate::schema))
    pub fn new(store: S) -> Self {
        let publisher = IndexPublisher::new(&store);
        Self { store, publisher }
    }

    /// For `Nfs::add`: durable view after every commit
    pub fn handle(&self) -> IndexHandle<S::View> {
        self.publisher.handle()
    }

    /// Follows `blocks` through `Shutdown` (a failure panics)
    ///
    /// - commits after the finalized tip + at `Shutdown` (a full buffer commits on its own)
    pub async fn run(self, mut blocks: Subscription<Block>) {
        let Self { mut store, publisher } = self;
        while let Some(run) = blocks.next_run().await {
            let started = Instant::now();
            let write;
            (store, write) = blocking(move || {
                let mut write = Duration::ZERO;
                for block in &run.blocks {
                    if !held(&store, block) {
                        let mut changes = store.changes(block.block_ref());
                        fold(&BlockHashReader::new(store.applied()), block, &mut changes);
                        write += apply(&mut store, changes);
                    }
                }
                if run.ends_at_finalized_tip {
                    write += commit(&mut store);
                }
                (store, write)
            })
            .await;
            publisher.publish(&store);
            ran(&store, started, write);
        }
        store = blocking(move || {
            commit(&mut store);
            store
        })
        .await;
        publisher.publish(&store);
    }
}

/// `block` onto `parent`: its one `by_hash` row (its own header only, nothing read)
pub fn fold<V: MapRead>(parent: &BlockHashReader<V>, block: &Block, out: &mut BlockChanges) {
    assert_next(out, parent.view().tip(), block);
    let header = block.header();
    out.map(BY_HASH).insert(&header.hash.0, &encode_height(header.height));
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema, View};
    use zaino_source::testing::{h, MockChain};
    use zaino_sync::testing::{finalized, open, step, Writer, QUEUE};
    use zcash_protocol::consensus::BlockHeight;
    use zcash_protocol::consensus::NetworkType;
    use zcash_protocol::BlockHash;

    use super::*;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    fn start(store: DiskStore) -> Writer<DiskView> {
        let writer = BlockHashIndexWriter::new(store);
        Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
    }

    /// - each block → one golden row at its own tip
    /// - reader locates every folded hash and nothing else
    #[test]
    fn each_block_folds_to_its_golden_row_and_the_reader_locates_exactly_those() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(2);
        let blocks = chain.blocks(tip);
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);

        for (height, block) in (0u8..).zip(&blocks) {
            let hash = block.header().hash.0;
            let mut changes = store.changes(block.block_ref());
            fold(&BlockHashReader::new(store.applied()), block, &mut changes);
            let rows: Vec<(&[u8], &[u8])> = changes.inserts(BY_HASH).collect();
            assert_eq!(rows, [(&hash[..], &[0, 0, 0, height][..])], "block {height}: hash → BE");
            store.apply(changes);
        }

        let reader = BlockHashReader::new(store.applied());
        let located: Vec<_> =
            blocks.iter().map(|block| reader.height_of(&block.header().hash)).collect();
        let expected: Vec<_> = (0..3u32).map(|n| Some(BlockHeight::from(n))).collect();
        assert_eq!(located, expected, "height_of each folded block's hash, 0..=2");
        assert_eq!(reader.height_of(&BlockHash([0xee; 32])), None, "never folded");
    }

    /// Six blocks, 0..=4 each its own commit (write_buffer = 1 byte), crashed after every
    /// operation
    ///
    /// - each state reopens to an acknowledged or the attempted commit
    /// - locates every hash it holds at its height, none past it
    /// - a writer on it skips the resent held blocks and commits the next at `FinalizedTip`
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_durable_prefix_that_skips_what_it_holds() {
        let mut chain = MockChain::regtest();
        let tip = chain.mine_empty(5);
        let blocks = chain.blocks(tip);
        let located = |view: DiskView| -> Vec<Option<BlockHeight>> {
            let reader = BlockHashReader::new(view);
            blocks.iter().map(|block| reader.height_of(&block.header().hash)).collect()
        };
        // `count` blocks held = heights `0..count` located, the rest nowhere
        let expected = |count: u32| -> Vec<Option<BlockHeight>> {
            (0..6).map(|n| (n < count).then(|| BlockHeight::from(n))).collect()
        };

        let (fs, recording) = SimFs::recording();
        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MIN));
        for (acked, block) in (1u64..).zip(&blocks[..5]) {
            writer.send(step(block)).await;
            writer.durable_at(block.header().height).await;
            fs.set_tag(acked);
        }
        writer.stop().await.expect("stops at Shutdown");

        let states = recording.crash_states();
        assert!(states.len() > 10, "enumerated {} crash states", states.len());
        for state in states {
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MAX);
            let count = store.durable().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let label = format!("{}: recovered {count} blocks, next height {count}", state.label);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5));
            assert!(acked.contains(&u64::from(count)), "{label}: acknowledged {acked:?}");
            assert_eq!(located(store.durable()), expected(count), "{label}: held hashes only");

            let mut writer = start(store);
            for block in &blocks[..count as usize] {
                writer.send(step(block)).await;
            }
            writer.send(finalized(&blocks[count as usize])).await;
            writer.durable_at(h(count)).await;
            let handle = writer.stop().await.unwrap_or_else(|error| panic!("{label}: {error}"));
            assert_eq!(located(handle.durable_view()), expected(count + 1), "{label}: next held");
        }
    }
}
