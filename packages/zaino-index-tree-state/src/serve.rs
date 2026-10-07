//! RPC surface: `GetTreeState`, `GetLatestTreeState`, `GetSubtreeRoots`
//!
//! - one `ArcSwap` load per request pins a [`ReadView`] (held blocks + committed files)
//! - per request: one 48 B record read, ≤ 33 node reads (32 B) per pool, ~1 KB serialized, no
//!   hashing

use std::sync::Arc;

use zaino_persistence::SequenceRead;
use zaino_primitives::types::{
    BlockchainInfo, ConsensusBranchId, Height, ShieldedPool, SubtreeRoot, Treestate,
};
use zaino_sync::Served;
use zcash_protocol::consensus::{BranchId, NetworkType};

use crate::ReadView;

/// Height each pool's tree begins, from the validator's schedule (`None` = unscheduled)
///
/// - zebra's `z_gettreestate` omits a pool below its upgrade, lightwalletd then answers `""`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolActivations {
    pub sapling: Height,
    pub orchard: Option<Height>,
    pub ironwood: Option<Height>,
}

impl PoolActivations {
    /// Keyed by branch id: Sapling, NU5 (orchard), NU6.3 (ironwood)
    pub fn from_validator(info: &BlockchainInfo) -> Self {
        let activation = |branch: BranchId| {
            let id = ConsensusBranchId::new(u32::from(branch));
            info.upgrades.iter().find(|upgrade| upgrade.branch_id == id)
        };
        Self {
            sapling: info.sapling_activation,
            orchard: activation(BranchId::Nu5).map(|upgrade| upgrade.activation_height),
            ironwood: activation(BranchId::Nu6_3).map(|upgrade| upgrade.activation_height),
        }
    }

    /// `pool` has a tree at `at`
    pub fn active(&self, pool: ShieldedPool, at: Height) -> bool {
        let from = match pool {
            ShieldedPool::Sapling => Some(self.sapling),
            ShieldedPool::Orchard => self.orchard,
            ShieldedPool::Ironwood => self.ironwood,
        };
        from.is_some_and(|from| from <= at)
    }
}

/// Small (transport maps these onto gRPC codes; this crate names no transport)
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServeError {
    /// Binary, progress-free (a partial index cannot tell "no such block" from "not yet")
    #[error("the tree-state index is still syncing")]
    Syncing,

    #[error("no tree state at height {height}")]
    NotFound { height: Height },

    /// lightwalletd: no tree state before any shielded pool exists (a bad request, not a miss)
    #[error("no tree state at height {height}, below Sapling activation {sapling}")]
    BeforeSapling { height: Height, sapling: Height },

    /// Stored nodes that will not rebuild a frontier (a fold bug, not a bad request)
    #[error("stored tree state at height {height} is inconsistent")]
    Inconsistent { height: Height },

    #[error("the tree-state index holds no blocks")]
    Empty,
}

#[derive(Debug, Clone)]
pub struct TreeStateService<V> {
    served: Served<ReadView<V>>,
    network: NetworkType,
    activations: PoolActivations,
}

impl<V: SequenceRead> TreeStateService<V> {
    /// - unsynced → every method [`ServeError::Syncing`] (committed heights excepted)
    /// - `network` = operator-declared (regtest reports as `"test"` over the validator's RPC)
    pub fn new(
        served: Served<ReadView<V>>,
        network: NetworkType,
        activations: PoolActivations,
    ) -> Self {
        Self { served, network, activations }
    }

    /// Chain this index was built against (`TreeState.network`)
    pub fn network(&self) -> NetworkType {
        self.network
    }

    /// Which pools a tree state at a height carries (a transport's wire shape)
    pub fn activations(&self) -> PoolActivations {
        self.activations
    }

    /// Tree state at `at` from one pinned `view` (a transport memoizing per publication)
    pub fn treestate_in(&self, view: &ReadView<V>, at: Height) -> Result<Treestate, ServeError> {
        let sapling = self.activations.sapling;
        match at < sapling {
            true => Err(ServeError::BeforeSapling { height: at, sapling }),
            false => view.treestate(at),
        }
    }

    /// Tree state at `view`'s tip
    pub fn latest_in(&self, view: &ReadView<V>) -> Result<Treestate, ServeError> {
        self.treestate_in(view, view.tip().ok_or(ServeError::Empty)?)
    }

    /// The latest publication, synced only (one load; every tree state it answers comes from it)
    ///
    /// - one `Arc` per publication: a transport may key per-publication memos on it
    pub fn pin(&self) -> Result<Arc<ReadView<V>>, ServeError> {
        self.served.pin().ok_or(ServeError::Syncing)
    }

    /// `GetTreeState`: a committed height answers while syncing too (final: no reorg reaches
    /// it); anything above it only once synced
    pub fn treestate(&self, at: Height) -> Result<Treestate, ServeError> {
        let view = self.served.pin_any();
        match Some(at) <= view.finalized() || self.served.synced() {
            true => self.treestate_in(&view, at),
            false => Err(ServeError::Syncing),
        }
    }

    /// `GetLatestTreeState`, non-finalized included (tracks the tip, not the fsync)
    pub fn latest(&self) -> Result<Treestate, ServeError> {
        self.latest_in(&*self.pin()?)
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
    use crate::{schema, TreeStateIndexWriter};
    use tokio_util::sync::CancellationToken;
    use zaino_header_chain::VerifiedChain;
    use zaino_persistence::{fs::SimFs, DiskEngine, IndexKind, PersistenceEngine};
    use zaino_primitives::testing::Chain;
    use zaino_primitives::types::{
        BlockRef, CompactCiphertext, ReorgDepth, SaplingData, SaplingOutput, Transaction,
        TransactionId,
    };
    use zaino_sync::{BlockSink, Step};

    /// Keyed by branch id, not list order or name: Sapling, NU5 (orchard), NU6.3 (ironwood);
    /// absent upgrade = `None`; pending = still its scheduled height
    #[test]
    fn pool_activations_come_from_the_validators_upgrade_schedule() {
        use zaino_primitives::types::{
            BlockHash, BlockchainInfo, ConsensusBranchId, ConsensusBranchIds, NetworkUpgradeInfo,
            NetworkUpgradeStatus,
        };

        let h = |n: u32| Height::try_from(n).expect("h");
        let upgrade = |branch: u32, height: u32, status| NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch),
            name: "label only".to_owned(),
            activation_height: h(height),
            status,
        };
        let info = |upgrades| BlockchainInfo {
            blocks: h(3_500_000),
            estimated_height: h(3_500_000),
            best_block_hash: BlockHash::from([0u8; 32]),
            sapling_activation: h(419_200),
            upgrades,
            consensus: ConsensusBranchIds {
                chain_tip: ConsensusBranchId::new(0x37a5_165b),
                next_block: ConsensusBranchId::new(0x37a5_165b),
            },
        };
        let active = NetworkUpgradeStatus::Active;

        let mainnet = info(vec![
            upgrade(0x37a5_165b, 3_428_143, active),
            upgrade(0xc8e7_1055, 2_726_400, active),
            upgrade(0x76b8_09bb, 419_200, active),
            upgrade(0xc2d6_d0b4, 1_687_104, active),
        ]);
        let expected = PoolActivations {
            sapling: h(419_200),
            orchard: Some(h(1_687_104)),
            ironwood: Some(h(3_428_143)),
        };
        assert_eq!(PoolActivations::from_validator(&mainnet), expected);

        let pending = info(vec![
            upgrade(0x76b8_09bb, 419_200, active),
            upgrade(0x37a5_165b, 4_000_000, NetworkUpgradeStatus::Pending),
        ]);
        let expected =
            PoolActivations { sapling: h(419_200), orchard: None, ironwood: Some(h(4_000_000)) };
        assert_eq!(PoolActivations::from_validator(&pending), expected, "no NU5 = never orchard");
    }

    /// Block 0 final, block 1 at the tip, sent through the sink. Syncing (chainview's tip ahead):
    /// a committed height answers (final); a non-finalized height, the tip and a pool scan refused
    /// alike (never a partial answer). The tip reached → the gate opens, all answer
    #[tokio::test]
    async fn an_unsynced_index_serves_committed_heights_only_and_everything_once_synced() {
        let store =
            DiskEngine::new(SimFs::new()).open(Path::new("/ts"), &schema(NetworkType::Regtest));
        let index =
            TreeStateIndexWriter::new(store.expect("open"), NonZeroUsize::MIN).expect("new");

        let mut cmu = [0u8; 32];
        cmu[..4].copy_from_slice(&7u32.to_le_bytes());
        let tx = |tag: u8| Transaction {
            txid: TransactionId::from([tag; 32]),
            transparent: Default::default(),
            sprout: Default::default(),
            sapling: SaplingData {
                outputs: vec![SaplingOutput {
                    cmu: cmu.into(),
                    ephemeral_key: [2u8; 32].into(),
                    enc_ciphertext: CompactCiphertext::from([3u8; CompactCiphertext::LENGTH]),
                }],
                ..Default::default()
            },
            orchard: Default::default(),
            ironwood: Default::default(),
        };
        // heights 0..=5 each one sapling output; the index is sent 0 and 1
        let mut chain = Chain::with_genesis(vec![tx(0x01)]);
        let tip =
            (2..=6).fold(chain.genesis(), |tip, tag| chain.mine_with(tip.hash, vec![tx(tag)]));
        let path = chain.path(tip.hash);
        let block = |height: u32| Arc::new(path[height as usize].clone());
        let h = |n: u32| Height::try_from(n).expect("h");
        let chain_tip = |height: usize| Some(Arc::new(VerifiedChain::regtest(&path[..=height])));
        let (tips, tip) = tokio::sync::watch::channel(chain_tip(5));
        let depth = ReorgDepth::new(std::num::NonZeroU32::new(10).expect("non-zero"));
        let cancel = CancellationToken::new();
        let published = index.published();
        let genesis = Height::GENESIS;
        let activations =
            PoolActivations { sapling: genesis, orchard: Some(genesis), ironwood: Some(genesis) };
        let service = TreeStateService::new(published.served(), NetworkType::Regtest, activations);
        let (mut applied, mut synced) =
            (published.subscribe_applied(), published.subscribe_synced());
        let gate = tokio::spawn(published.gate(tip, depth, cancel.clone()));
        let mut sink = BlockSink::new("blocks");
        let queue = NonZeroUsize::new(1 << 20).expect("non-zero");
        let blocks = sink.subscribe(IndexKind::TreeState.name(), queue);
        let running = tokio::spawn(index.run(blocks));
        let within = Duration::from_secs(5);
        for (height, finalized) in [(0, true), (1, false)] {
            sink.send(Step::Apply { height: h(height), finalized, data: block(height) }).await;
        }
        let one = Some(BlockRef { hash: path[1].header().hash, height: h(1) });
        let tip_applied = tokio::time::timeout(within, applied.wait_for(|at| *at == one));
        tip_applied.await.expect("tip applied").expect("index alive");

        assert_eq!(service.treestate(h(0)).expect("committed").height, h(0));
        assert_eq!(service.treestate(h(1)), Err(ServeError::Syncing), "non-finalized");
        assert_eq!(service.treestate(h(2)), Err(ServeError::Syncing), "not yet held");
        assert_eq!(service.latest(), Err(ServeError::Syncing));
        assert_eq!(service.subtree_roots(ShieldedPool::Sapling, 0, 0), Err(ServeError::Syncing));

        tips.send_replace(chain_tip(1));
        let open = tokio::time::timeout(within, synced.wait_for(|open| *open));
        open.await.expect("the tip reached opens the gate").expect("gate alive");

        assert_eq!(service.latest().expect("tip").height, h(1));
        assert!(service.treestate(h(1)).expect("non-finalized").sapling.as_bytes().len() > 1);
        assert_eq!(service.subtree_roots(ShieldedPool::Sapling, 0, 0), Ok(Vec::new()));
        // Above the index = absent, never a progress report
        assert_eq!(service.treestate(h(2)), Err(ServeError::NotFound { height: h(2) }));

        sink.shutdown();
        running.await.expect("followed through Shutdown");
        cancel.cancel();
        gate.await.expect("gate ends on cancel");
    }
}
