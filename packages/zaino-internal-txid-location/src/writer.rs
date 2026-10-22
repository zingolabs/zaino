//! txid_location writer: the finalized stream → one [`fold`] per block → its store

use std::time::{Duration, Instant};

use zaino_persistence::{BlockChanges, MapRead, Store};
use zaino_primitives::types::Block;
use zaino_sync::{
    apply, assert_next, blocking, commit, held, ran, IndexHandle, IndexPublisher, Subscription,
};

use crate::by_txid::{encode, Mined, BY_TXID};
use crate::TxidLocationReader;

pub struct TxidLocationIndexWriter<S: Store> {
    store: S,
    publisher: IndexPublisher<S::View>,
}

impl<S: Store<View: MapRead>> TxidLocationIndexWriter<S> {
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
                        fold(&TxidLocationReader::new(store.applied()), block, &mut changes);
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

/// `block` onto `parent`: one `by_txid` row per transaction (nothing read: txids are unique since
/// genesis, BIP 34's coinbase height)
pub fn fold<V: MapRead>(parent: &TxidLocationReader<V>, block: &Block, out: &mut BlockChanges) {
    assert_next(out, parent.view().tip(), block);
    let at = block.block_ref();
    for (index, tx) in (0u32..).zip(block.transactions()) {
        out.map(BY_TXID).insert(tx.txid.as_ref(), &encode(Mined { block: at, index }));
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema, View};
    use zaino_source::testing::MockChain;
    use zaino_sync::testing::{open, step, Writer, QUEUE};
    use zcash_protocol::consensus::NetworkType;
    use zcash_protocol::TxId;

    use super::*;
    use crate::by_txid::decode;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    fn start(store: DiskStore) -> Writer<DiskView> {
        let writer = TxidLocationIndexWriter::new(store);
        Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
    }

    /// Block 1 = coinbase + two txs: one row per tx, keyed by txid, at its position (coinbase = 0)
    #[test]
    fn each_transaction_folds_to_one_row_naming_its_block_and_position() {
        let mut chain = MockChain::regtest();
        let one = chain.mine(|b| b.tx(|t| t).tx(|t| t));
        let blocks = chain.blocks(one);
        let mut store = open(&SimFs::new(), &SCHEMA, NonZeroUsize::MAX);
        let mut genesis = store.changes(blocks[0].block_ref());
        fold(&TxidLocationReader::new(store.applied()), &blocks[0], &mut genesis);
        store.apply(genesis);

        let mut changes = store.changes(blocks[1].block_ref());
        fold(&TxidLocationReader::new(store.applied()), &blocks[1], &mut changes);

        let rows: Vec<(TxId, Mined)> = changes
            .inserts(BY_TXID)
            .map(|(txid, mined)| {
                let txid = TxId::from_bytes(txid.try_into().expect("by_txid key: 32-byte txid"));
                (txid, decode(mined.try_into().expect("by_txid value: MINED bytes")))
            })
            .collect();
        let expected: Vec<(TxId, Mined)> = (0u32..3)
            .map(|index| (chain.tx(one, index as usize).txid, Mined { block: one, index }))
            .collect();
        assert_eq!(rows, expected, "block 1's rows: (txid, mined) per tx, coinbase first");
    }

    /// Six blocks (2 = coinbase + two txs), 0..=4 each its own commit (write_buffer = 1 byte),
    /// crashed after every operation
    ///
    /// - each state reopens to an acknowledged or the attempted commit
    /// - locates every tx it holds at its block + position, none past it
    /// - a writer on it skips the resent held blocks and commits the next one
    #[tokio::test]
    async fn every_crash_state_reopens_to_a_durable_prefix_that_skips_what_it_holds() {
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        chain.mine(|b| b.tx(|t| t).tx(|t| t));
        let tip = chain.mine_empty(3);
        let blocks = chain.blocks(tip);
        let mined: Vec<(TxId, Mined)> = blocks
            .iter()
            .flat_map(|block| {
                let at = block.block_ref();
                (0u32..)
                    .zip(block.transactions())
                    .map(move |(index, tx)| (tx.txid, Mined { block: at, index }))
            })
            .collect();
        assert_eq!(mined.len(), 8, "six coinbases + two");
        // `count` blocks held = heights `0..count` located, the rest nowhere
        let expected = |count: u32| -> Vec<(TxId, Option<Mined>)> {
            let held = |at: &Mined| u32::from(at.block.height) < count;
            mined.iter().map(|(txid, at)| (*txid, Some(*at).filter(held))).collect()
        };
        let located = |view: DiskView| -> Vec<(TxId, Option<Mined>)> {
            let reader = TxidLocationReader::new(view);
            mined.iter().map(|(txid, _)| (*txid, reader.mined(txid))).collect()
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
            let store = open(&state.fs, &SCHEMA, NonZeroUsize::MIN);
            let count = store.durable().tip().map_or(0, |tip| u32::from(tip.height) + 1);
            let label = format!("{}: recovered {count} blocks, next height {count}", state.label);
            let acked = [state.tag, state.tag + 1].map(|tag| tag.min(5));
            assert!(acked.contains(&u64::from(count)), "{label}: acknowledged {acked:?}");
            assert_eq!(located(store.durable()), expected(count), "{label}: held txs only");

            let writer = start(store);
            for block in &blocks[..=count as usize] {
                writer.send(step(block)).await;
            }
            let handle = writer.stop().await.unwrap_or_else(|error| panic!("{label}: {error}"));
            assert_eq!(located(handle.durable_view()), expected(count + 1), "{label}: next held");
        }
    }
}
