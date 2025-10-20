use std::sync::Arc;

use proptest::{
    prelude::{Arbitrary as _, BoxedStrategy, Just},
    strategy::Strategy,
};
use tonic::async_trait;
use zebra_chain::{
    block::arbitrary::{self, LedgerStateOverride},
    fmt::SummaryDebug,
    parameters::GENESIS_PREVIOUS_BLOCK_HASH,
    LedgerState,
};
use zebra_state::{FromDisk, HashOrHeight, IntoDisk as _};

use crate::{
    chain_index::source::BlockchainSourceResult, BlockHash, BlockchainSource, TransactionHash,
};

#[test]
fn make_chain() {
    proptest::proptest!(|(segments in make_branching_chain(2, 12))| {
        let (genesis_segment, branch_segments) = segments;
        let mut prev_hash = GENESIS_PREVIOUS_BLOCK_HASH;
        for block in genesis_segment {
            assert_eq!(block.header.previous_block_hash, prev_hash);
            println!("pre-divergence: {:?}", block.coinbase_height());
            prev_hash = block.hash();

        }
        let hash_atop_shared_chain = prev_hash;
        for branch_segment in branch_segments {
            for block in branch_segment {
            assert_eq!(block.header.previous_block_hash, prev_hash);
                println!("post-divergence: {:?}", block.coinbase_height());
            prev_hash = block.hash();
            }
            prev_hash = hash_atop_shared_chain;
        }
    });
}

#[derive(Clone)]
struct ProptestMockchain {
    genesis_segment: SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>,
    branching_segments: Vec<SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>>,
}

impl ProptestMockchain {
    fn best_branch(&self) -> SummaryDebug<Vec<Arc<zebra_chain::block::Block>>> {
        let mut best_branch_and_work = None;
        for branch in self.branching_segments.clone() {
            let branch_chainwork: u128 = branch
                .iter()
                .map(|block| {
                    block
                        .header
                        .difficulty_threshold
                        .to_work()
                        .unwrap()
                        .as_u128()
                })
                .sum();
            match best_branch_and_work {
                Some((ref _b, w)) => {
                    if w < branch_chainwork {
                        best_branch_and_work = Some((branch, branch_chainwork))
                    }
                }
                None => best_branch_and_work = Some((branch, branch_chainwork)),
            }
        }
        best_branch_and_work.unwrap().0
    }

    fn all_blocks_arb_branch_order(&self) -> impl Iterator<Item = &Arc<zebra_chain::block::Block>> {
        self.genesis_segment.iter().chain(
            self.branching_segments
                .iter()
                .flat_map(|branch| branch.iter()),
        )
    }

    fn get_block_and_all_preceeding(
        &self,
        // This probably doesn't need to allow FnMut closures (Fn should suffice)
        // but there's no cost to allowing it
        mut block_identifier: impl FnMut(&zebra_chain::block::Block) -> bool,
    ) -> std::option::Option<Vec<&Arc<zebra_chain::block::Block>>> {
        let mut blocks = Vec::new();
        for block in self.genesis_segment.iter() {
            blocks.push(block);
            if block_identifier(block) {
                return Some(blocks);
            }
        }
        for branch in self.branching_segments.iter() {
            let mut branch_blocks = Vec::new();
            for block in branch.iter() {
                branch_blocks.push(block);
                if block_identifier(block) {
                    blocks.extend_from_slice(&branch_blocks);
                    return Some(blocks);
                }
            }
        }

        None
    }
}

#[async_trait]
impl BlockchainSource for ProptestMockchain {
    /// Returns the block by hash or height
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match id {
            HashOrHeight::Hash(hash) => {
                let matches_hash = |block: &&Arc<zebra_chain::block::Block>| block.hash() == hash;
                Ok(self
                    .genesis_segment
                    .iter()
                    .find(matches_hash)
                    .or_else(|| {
                        self.branching_segments
                            .iter()
                            .flat_map(|vec| vec.iter())
                            .find(matches_hash)
                    })
                    .cloned())
            }
            HashOrHeight::Height(height) => Ok(self
                .genesis_segment
                .iter()
                .find(|block| block.coinbase_height().unwrap() == height)
                .cloned()
                .or_else(|| {
                    self.best_branch()
                        .into_iter()
                        .find(|block| block.coinbase_height().unwrap() == height)
                })),
        }
    }

    /// Returns the block commitment tree data by hash
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        let Some(chain_up_to_block) =
            self.get_block_and_all_preceeding(|block| block.hash().0 == id.0)
        else {
            return Ok((None, None));
        };

        let (sapling, orchard) =
            chain_up_to_block
                .iter()
                .fold((None, None), |(mut sapling, mut orchard), block| {
                    for transaction in &block.transactions {
                        for sap_commitment in transaction.sapling_note_commitments() {
                            let sap_commitment = zebra_chain::sapling::tree::Node::from_bytes(
                                sap_commitment.to_bytes(),
                            );

                            sapling = Some(sapling.unwrap_or_else(|| {
                                incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                            }));

                            sapling = sapling.map(|mut tree| {
                                tree.append(sap_commitment);
                                tree
                            });
                        }
                        for orc_commitment in transaction.orchard_note_commitments() {
                            let orc_commitment =
                                zebra_chain::orchard::tree::Node::from(*orc_commitment);

                            orchard = Some(orchard.unwrap_or_else(|| {
                                incrementalmerkletree::frontier::Frontier::<_, 32>::empty()
                            }));

                            orchard = orchard.map(|mut tree| {
                                tree.append(orc_commitment);
                                tree
                            });
                        }
                    }
                    (sapling, orchard)
                });
        Ok((
            sapling.map(|sap_front| {
                (
                    zebra_chain::sapling::tree::Root::from_bytes(sap_front.root().as_ref()),
                    sap_front.tree_size(),
                )
            }),
            orchard.map(|orc_front| {
                (
                    zebra_chain::orchard::tree::Root::from_bytes(orc_front.root().as_bytes()),
                    orc_front.tree_size(),
                )
            }),
        ))
    }

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        // I don't think this is used for sync?
        unimplemented!()
    }

    /// Returns the complete list of txids currently in the mempool.
    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        Ok(None)
    }

    /// Returns the transaction by txid
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::transaction::Transaction>>> {
        Ok(self.all_blocks_arb_branch_order().find_map(|block| {
            block
                .transactions
                .iter()
                .find(|transaction| transaction.hash() == txid.into())
                .cloned()
        }))
    }

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        Ok(Some(self.best_branch().last().unwrap().hash()))
    }

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let (sender, receiver) = tokio::sync::mpsc::channel(1_000);
        let self_clone = self.clone();
        tokio::task::spawn((|| async move {
            for block in self_clone.all_blocks_arb_branch_order() {
                sender.send((block.hash(), block.clone())).await.unwrap()
            }
        })())
        .await
        .unwrap();
        Ok(Some(receiver))
    }
}

fn make_branching_chain(
    num_branches: usize,
    chain_size: usize,
) -> BoxedStrategy<(
    SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>,
    Vec<SummaryDebug<Vec<Arc<zebra_chain::block::Block>>>>,
)> {
    arbitrary::LedgerState::genesis_strategy(None, None, true)
        .prop_flat_map(move |ledger| {
            zebra_chain::block::Block::partial_chain_strategy(
                ledger,
                chain_size,
                arbitrary::allow_all_transparent_coinbase_spends,
                false,
            )
        })
        .prop_flat_map(|segment| {
            (
                Just(segment.clone()),
                LedgerState::arbitrary_with(LedgerStateOverride {
                    height_override: segment.last().unwrap().coinbase_height().unwrap() + 1,
                    previous_block_hash_override: Some(segment.last().unwrap().hash()),
                    network_upgrade_override: None,
                    transaction_version_override: None,
                    transaction_has_valid_network_upgrade: true,
                    always_has_coinbase: true,
                }),
            )
        })
        .prop_flat_map(move |(segment, ledger)| {
            (
                Just(segment),
                std::iter::repeat_with(|| {
                    zebra_chain::block::Block::partial_chain_strategy(
                        ledger.clone(),
                        chain_size,
                        arbitrary::allow_all_transparent_coinbase_spends,
                        false,
                    )
                })
                .take(num_branches)
                .collect::<Vec<_>>(),
            )
        })
        .boxed()
}
