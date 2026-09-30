//! RPC surface: `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots`
//!
//! - one `ArcSwap` load per request pins a [`ReadView`] (non-finalized + committed files)
//! - per request: one 48 B record read, ≤ 33 node reads (32 B) per pool, ~1 KB serialized, no
//!   hashing

use std::sync::Arc;

use zaino_primitives::types::{Height, ShieldedPool, SubtreeRoot, Treestate};
use zaino_sync::Served;
use zcash_protocol::consensus::NetworkType;

use crate::ReadView;

/// Small (transport maps these onto gRPC codes; this crate names no transport)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// Binary, progress-free (a partial index cannot tell "no such block" from "not yet")
    #[error("the tree-state index is still syncing")]
    Syncing,

    #[error("no tree state at height {height}")]
    NotFound { height: Height },

    /// Stored nodes that will not rebuild a frontier (a fold bug, not a bad request)
    #[error("stored tree state at height {height} is inconsistent")]
    Inconsistent { height: Height },

    #[error("the tree-state index holds no blocks")]
    Empty,
}

#[derive(Debug, Clone)]
pub struct TreeStateService {
    served: Served<ReadView>,
    network: NetworkType,
}

impl TreeStateService {
    /// - unsynced → every method [`ServeError::Syncing`] (committed heights excepted)
    /// - `network` = operator-declared (regtest reports as `"test"` over the validator's RPC)
    pub fn new(served: Served<ReadView>, network: NetworkType) -> Self {
        Self { served, network }
    }

    /// Chain this index was built against (`TreeState.network`)
    pub fn network(&self) -> NetworkType {
        self.network
    }

    /// The latest publication, synced only (one load; every tree state it answers comes from it)
    ///
    /// - one `Arc` per publication: a transport may key per-publication memos on it
    pub fn pin(&self) -> Result<Arc<ReadView>, ServeError> {
        self.served.pin().ok_or(ServeError::Syncing)
    }

    /// `GetTreeState`: a committed height answers while syncing too (final: no reorg reaches
    /// it); anything above it only once synced
    pub fn treestate(&self, at: Height) -> Result<Treestate, ServeError> {
        let view = self.served.pin_any();
        match Some(at) <= view.finalized() || self.served.synced() {
            true => view.treestate(at),
            false => Err(ServeError::Syncing),
        }
    }

    /// `GetLatestTreeState`, non-finalized included (tracks the tip, not the fsync)
    pub fn latest(&self) -> Result<Treestate, ServeError> {
        self.pin()?.latest()
    }

    /// `GetSubtreeRoots` (a `Vec`, not a stream: ≤ 2^16 subtrees, the whole file is ~2.6 MB)
    pub fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: u16,
    ) -> Result<Vec<SubtreeRoot>, ServeError> {
        self.pin()?.subtree_roots(pool, start_index, max_entries)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use std::{num::NonZeroUsize, time::Duration};

    use super::*;
    use crate::{TreeStateIndexWriter, TreeStateStore};
    use tokio_util::sync::CancellationToken;
    use zaino_chainview::{EndpointSet, QuorumTip};
    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        Block, BlockHeader, BlockRef, CompactCiphertext, ReorgDepth, SaplingData, SaplingOutput,
        Transaction, TransactionId,
    };
    use zaino_sync::{BlockSink, Step};

    /// Block 0 final, block 1 at the tip, sent through the sink. Syncing (chainview's tip ahead):
    /// a committed height answers (final); a non-finalized height, the tip and a pool scan refused
    /// alike (never a partial answer). The tip reached → the gate opens, all answer
    #[tokio::test]
    async fn an_unsynced_index_serves_committed_heights_only_and_everything_once_synced() {
        let index = TreeStateIndexWriter::new(
            TreeStateStore::open(SimFs::new(), Path::new("/ts"), NetworkType::Regtest)
                .expect("open"),
            NonZeroUsize::MIN,
        )
        .expect("new");

        let mut cmu = [0u8; 32];
        cmu[..4].copy_from_slice(&7u32.to_le_bytes());
        let block = |height: u32| {
            Arc::new(Block::new(
                BlockHeader::for_tests(
                    height,
                    [height as u8; 32],
                    [height.wrapping_sub(1) as u8; 32],
                    1_700_000_000 + height,
                ),
                vec![Transaction {
                    txid: TransactionId::from([0x01 + height as u8; 32]),
                    transparent: Default::default(),
                    sprout: Default::default(),
                    sapling: SaplingData {
                        outputs: vec![SaplingOutput {
                            cmu: cmu.into(),
                            ephemeral_key: [2u8; 32].into(),
                            enc_ciphertext: CompactCiphertext::from(
                                [3u8; CompactCiphertext::LENGTH],
                            ),
                        }],
                        ..Default::default()
                    },
                    orchard: Default::default(),
                    ironwood: Default::default(),
                }],
            ))
        };
        let h = |n: u32| Height::try_from(n).expect("h");
        let quorum = |height: u32| {
            let block = BlockRef { hash: [height as u8; 32].into(), height: h(height) };
            Some(QuorumTip { block, agreed_by: EndpointSet::default() })
        };
        let (tips, tip) = tokio::sync::watch::channel(quorum(5));
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(10).expect("non-zero"));
        let cancel = CancellationToken::new();
        let published = index.published();
        let service = TreeStateService::new(published.served(), NetworkType::Regtest);
        let (mut applied, mut synced) =
            (published.subscribe_applied(), published.subscribe_synced());
        let gate = tokio::spawn(published.gate(tip, depth, cancel.clone()));
        let mut sink = BlockSink::new("blocks");
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let blocks = sink.subscribe(TreeStateIndexWriter::NAME, queue);
        let running = tokio::spawn(index.run(blocks, cancel.clone()));
        let within = Duration::from_secs(5);
        for (height, finalized) in [(0, true), (1, false)] {
            sink.send(Step::Apply { height: h(height), finalized, data: block(height) }).await;
        }
        let tip_applied = tokio::time::timeout(within, applied.wait_for(|at| *at == Some(h(1))));
        tip_applied.await.expect("tip applied").expect("index alive");

        assert_eq!(service.treestate(h(0)).expect("committed").height, h(0));
        assert_eq!(service.treestate(h(1)), Err(ServeError::Syncing), "non-finalized");
        assert_eq!(service.treestate(h(2)), Err(ServeError::Syncing), "not yet held");
        assert_eq!(service.latest(), Err(ServeError::Syncing));
        assert_eq!(service.subtree_roots(ShieldedPool::Sapling, 0, 0), Err(ServeError::Syncing));

        tips.send_replace(quorum(1));
        let open = tokio::time::timeout(within, synced.wait_for(|open| *open));
        open.await.expect("the tip reached opens the gate").expect("gate alive");

        assert_eq!(service.latest().expect("tip").height, h(1));
        assert!(service.treestate(h(1)).expect("non-finalized").sapling.as_bytes().len() > 1);
        assert_eq!(service.subtree_roots(ShieldedPool::Sapling, 0, 0), Ok(Vec::new()));
        // Above the index = absent, never a progress report
        assert_eq!(service.treestate(h(2)), Err(ServeError::NotFound { height: h(2) }));

        sink.shutdown();
        running.await.expect("no panic").expect("followed through Shutdown");
        cancel.cancel();
        gate.await.expect("gate ends on cancel");
    }
}
