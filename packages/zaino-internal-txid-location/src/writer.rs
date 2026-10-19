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

    use zaino_persistence::{fs::SimFs, DiskStore, DiskView, Schema};
    use zaino_primitives::testing::{h, MockChain};
    use zaino_sync::testing::{finalized, open, step, Writer, QUEUE};
    use zcash_protocol::consensus::NetworkType;
    use zcash_protocol::TxId;

    use super::*;

    const SCHEMA: Schema = crate::schema(NetworkType::Regtest);

    fn start(store: DiskStore) -> Writer<DiskView> {
        let writer = TxidLocationIndexWriter::new(store);
        Writer::spawn(writer.handle(), QUEUE, move |blocks| writer.run(blocks))
    }

    /// 0..=3 (2 mines two txs: coinbase at 0, then 1, 2), 3 `FinalizedTip`, then a restart
    /// resending 2 (held: skipped) before 4
    ///
    /// - every txid located at its block and position after a reopen
    /// - a txid never mined nowhere
    #[tokio::test]
    async fn every_txid_locates_its_block_and_position_and_a_restart_skips_what_it_holds() {
        let fs = SimFs::new();
        let mut chain = MockChain::regtest();
        chain.mine_empty(1);
        chain.mine(|b| b.tx(|t| t.txid([1; 32])).tx(|t| t.txid([2; 32])));
        let tip = chain.mine_empty(2);
        let blocks = chain.blocks(tip);

        let mut writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));
        for block in &blocks[..3] {
            writer.send(step(block)).await;
        }
        writer.send(finalized(&blocks[3])).await;
        writer.durable_at(h(3)).await;
        writer.stop().await.expect("stops at Shutdown");
        let writer = start(open(&fs, &SCHEMA, NonZeroUsize::MAX));
        for block in &blocks[2..=4] {
            writer.send(step(block)).await;
        }
        writer.stop().await.expect("stops at Shutdown");

        let reader = TxidLocationReader::new(open(&fs, &SCHEMA, NonZeroUsize::MAX).durable());
        let mined: Vec<(TxId, Mined)> = blocks
            .iter()
            .flat_map(|block| {
                let at = block.block_ref();
                (0u32..)
                    .zip(block.transactions())
                    .map(move |(index, tx)| (tx.txid, Mined { block: at, index }))
            })
            .collect();
        assert_eq!(mined.len(), 7, "five coinbases + two");
        for (txid, expected) in &mined {
            assert_eq!(reader.mined(txid), Some(*expected), "{txid}");
        }
        assert_eq!(reader.mined(&TxId::from_bytes([0xEE; 32])), None, "never mined");
    }
}
