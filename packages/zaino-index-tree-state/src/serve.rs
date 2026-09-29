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

    use arc_swap::ArcSwap;
    use zaino_persistence::fs::SimFs;
    use zaino_primitives::types::{
        Block, BlockHeader, CompactCiphertext, SaplingData, SaplingOutput, Transaction,
        TransactionId,
    };
    use zaino_sync::IndexWriter;

    use super::*;
    use crate::{TreeStateIndexWriter, TreeStateStore};

    /// Syncing: a committed height answers (final); a non-finalized height, the tip and a pool
    /// scan refused alike (never a partial answer); once synced all answer
    #[tokio::test]
    async fn an_unsynced_index_serves_committed_heights_only_and_everything_once_synced() {
        let mut writer = TreeStateIndexWriter::new(
            TreeStateStore::open(SimFs::new(), Path::new("/ts"), NetworkType::Regtest)
                .expect("open"),
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
        zaino_sync::finalize_now(&mut writer, &[block(0)]).await.expect("finalize");
        writer.apply(&block(1)).await.expect("apply");

        let (follower, synced) = tokio::sync::watch::channel(false);
        let view = Arc::new(ArcSwap::from_pointee(writer.view()));
        let service = TreeStateService::new(Served::new(view, synced), NetworkType::Regtest);
        let h = |n: u32| Height::try_from(n).expect("h");

        assert_eq!(service.treestate(h(0)).expect("committed").height, h(0));
        assert_eq!(service.treestate(h(1)), Err(ServeError::Syncing), "non-finalized");
        assert_eq!(service.treestate(h(2)), Err(ServeError::Syncing), "not yet held");
        assert_eq!(service.latest(), Err(ServeError::Syncing));
        assert_eq!(service.subtree_roots(ShieldedPool::Sapling, 0, 0), Err(ServeError::Syncing));

        follower.send(true).expect("service holds the receiver");

        assert_eq!(service.latest().expect("tip").height, h(1));
        assert!(service.treestate(h(1)).expect("non-finalized").sapling.as_bytes().len() > 1);
        assert_eq!(service.subtree_roots(ShieldedPool::Sapling, 0, 0), Ok(Vec::new()));
        // Above the index = absent, never a progress report
        assert_eq!(service.treestate(h(2)), Err(ServeError::NotFound { height: h(2) }));
    }
}
