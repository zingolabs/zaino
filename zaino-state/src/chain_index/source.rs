//! Traits and types for the blockchain source thats serves zaino, commonly a validator connection.

use std::{error::Error, str::FromStr as _, sync::Arc};

use crate::chain_index::types::{BlockHash, TransactionHash};
use async_trait::async_trait;
use futures::{future::join, TryFutureExt as _};
use tower::{Service, ServiceExt as _};
use zaino_common::Network;
use zaino_fetch::jsonrpsee::{
    connector::{JsonRpSeeConnector, RpcRequestError},
    response::{GetBlockError, GetBlockResponse, GetTransactionResponse, GetTreestateResponse},
};
use zcash_primitives::merkle_tree::read_commitment_tree;
use zebra_chain::serialization::ZcashDeserialize;
use zebra_state::{HashOrHeight, ReadRequest, ReadResponse, ReadStateService};

macro_rules! expected_read_response {
    ($response:ident, $expected_variant:ident) => {
        match $response {
            ReadResponse::$expected_variant(inner) => inner,
            unexpected => {
                unreachable!("Unexpected response from state service: {unexpected:?}")
            }
        }
    };
}

/// A trait for accessing blockchain data from different backends.
#[async_trait]
pub trait BlockchainSource: Clone + Send + Sync + 'static {
    /// Returns the block by hash or height
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>>;

    /// Returns the block commitment tree data by hash
    async fn get_commitment_tree_roots(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )>;

    /// Returns the sapling and orchard treestate by hash
    async fn get_treestate(
        &self,
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)>;

    /// Returns the complete list of txids currently in the mempool.
    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>>;

    /// Returns the transaction by txid
    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::transaction::Transaction>>>;

    /// Returns the hash of the block at the tip of the best chain.
    async fn get_best_block_hash(&self)
        -> BlockchainSourceResult<Option<zebra_chain::block::Hash>>;

    /// Get a listener for new nonfinalized blocks,
    /// if supported
    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    >;
}

/// An error originating from a blockchain source.
#[derive(Debug, thiserror::Error)]
pub enum BlockchainSourceError {
    /// Unrecoverable error.
    // TODO: Add logic for handling recoverable errors if any are identified
    // one candidate may be ephemerable network hiccoughs
    #[error("critical error in backing block source: {0}")]
    Unrecoverable(String),
}

/// Error type returned when invalid data is returned by the validator.
#[derive(thiserror::Error, Debug)]
#[error("data from validator invalid: {0}")]
pub struct InvalidData(String);

type BlockchainSourceResult<T> = Result<T, BlockchainSourceError>;

/// ReadStateService based validator connector.
///
/// Currently the Mempool cannot utilise the mempool change endpoint in the ReadStateService,
/// for this reason the lagacy jsonrpc inteface is used until the Mempool updates required can be implemented.
///
/// Due to the difference if the mempool inteface provided by the ReadStateService and the Json RPC service
/// two seperate Mempool implementation will likely be required.
#[derive(Clone, Debug)]
pub struct State {
    /// Used to fetch chain data.
    pub read_state_service: ReadStateService,
    /// Temporarily used to fetch mempool data.
    pub mempool_fetcher: JsonRpSeeConnector,
    /// Current network type being run.
    pub network: Network,
}

/// A connection to a validator.
#[derive(Clone, Debug)]
// TODO: Explore whether State should be Boxed.
#[allow(clippy::large_enum_variant)]
pub enum ValidatorConnector {
    /// The connection is via direct read access to a zebrad's data file
    ///
    /// NOTE: See docs for State struct.
    State(State),
    /// We are connected to a zebrad, zcashd, or other zainod via JsonRpc ("JsonRpSee")
    Fetch(JsonRpSeeConnector),
}

#[async_trait]
impl BlockchainSource for ValidatorConnector {
    async fn get_block(
        &self,
        id: HashOrHeight,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
        match self {
            ValidatorConnector::State(state) => match state
                .read_state_service
                .clone()
                .call(zebra_state::ReadRequest::Block(id))
                .await
            {
                Ok(zebra_state::ReadResponse::Block(block)) => Ok(block),
                Ok(otherwise) => panic!(
                    "Read Request of Block returned Read Response of {otherwise:#?} \n\
                    This should be deterministically unreachable"
                ),
                Err(e) => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
            },
            ValidatorConnector::Fetch(fetch) => {
                match fetch
                    .get_block(id.to_string(), Some(0))
                    .await
                {
                    Ok(GetBlockResponse::Raw(raw_block)) => Ok(Some(Arc::new(
                        zebra_chain::block::Block::zcash_deserialize(raw_block.as_ref())
                            .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                    ))),
                    Ok(_) => unreachable!(),
                    Err(e) => match e {
                        RpcRequestError::Method(GetBlockError::MissingBlock(_)) => Ok(None),
                        // TODO/FIX: zcashd returns this transport error when a block is requested higher than current chain. is this correct?
                        RpcRequestError::Transport(zaino_fetch::jsonrpsee::error::TransportError::ErrorStatusCode(500)) => Ok(None),
                        RpcRequestError::ServerWorkQueueFull => Err(BlockchainSourceError::Unrecoverable("Work queue full. not yet implemented: handling of ephemeral network errors.".to_string())),
                        _ => Err(BlockchainSourceError::Unrecoverable(e.to_string())),
                    },
                }
            }
        }
    }

    async fn get_commitment_tree_roots(
        &self,
        // Sould this be HashOrHeight?
        id: BlockHash,
    ) -> BlockchainSourceResult<(
        Option<(zebra_chain::sapling::tree::Root, u64)>,
        Option<(zebra_chain::orchard::tree::Root, u64)>,
    )> {
        match self {
            ValidatorConnector::State(state) => {
                let (sapling_tree_response, orchard_tree_response) =
                    join(
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::SaplingTree(HashOrHeight::Hash(id.into())),
                        ),
                        state.read_state_service.clone().call(
                            zebra_state::ReadRequest::OrchardTree(HashOrHeight::Hash(id.into())),
                        ),
                    )
                    .await;
                let (sapling_tree, orchard_tree) = match (
                    //TODO: Better readstateservice error handling
                    sapling_tree_response
                        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                    orchard_tree_response
                        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))?,
                ) {
                    (ReadResponse::SaplingTree(saptree), ReadResponse::OrchardTree(orctree)) => {
                        (saptree, orctree)
                    }
                    (_, _) => panic!("Bad response"),
                };

                Ok((
                    sapling_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                    orchard_tree
                        .as_deref()
                        .map(|tree| (tree.root(), tree.count())),
                ))
            }
            ValidatorConnector::Fetch(fetch) => {
                let tree_responses = fetch
                    .get_treestate(id.to_string())
                    .await
                    // As MethodError contains a GetTreestateError, which is an enum with no variants,
                    // we don't need to account for it at all here
                    .map_err(|e| match e {
                        RpcRequestError::ServerWorkQueueFull => {
                            BlockchainSourceError::Unrecoverable(
                                "Not yet implemented: handle backing validator\
                                full queue"
                                    .to_string(),
                            )
                        }
                        _ => BlockchainSourceError::Unrecoverable(e.to_string()),
                    })?;
                let GetTreestateResponse {
                    sapling, orchard, ..
                } = tree_responses;
                let sapling_frontier = sapling
                    .commitments()
                    .final_state()
                    .as_ref()
                    .map(|final_state| {
                        read_commitment_tree::<zebra_chain::sapling::tree::Node, _, 32>(
                            final_state.as_slice(),
                        )
                    })
                    .transpose()
                    .map_err(|e| BlockchainSourceError::Unrecoverable(format!("io error: {e}")))?;
                let orchard_frontier = orchard
                    .commitments()
                    .final_state()
                    .as_ref()
                    .map(|final_state| {
                        read_commitment_tree::<zebra_chain::orchard::tree::Node, _, 32>(
                            final_state.as_slice(),
                        )
                    })
                    .transpose()
                    .map_err(|e| BlockchainSourceError::Unrecoverable(format!("io error: {e}")))?;
                let sapling_root = sapling_frontier
                    .map(|tree| {
                        zebra_chain::sapling::tree::Root::try_from(*tree.root().as_ref())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("could not deser: {e}"))
                    })?;
                let orchard_root = orchard_frontier
                    .map(|tree| {
                        zebra_chain::orchard::tree::Root::try_from(tree.root().to_repr())
                            .map(|root| (root, tree.size() as u64))
                    })
                    .transpose()
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("could not deser: {e}"))
                    })?;
                Ok((sapling_root, orchard_root))
            }
        }
    }

    /// Returns the Sapling and Orchard treestate by blockhash.
    async fn get_treestate(
        &self,
        // Sould this be HashOrHeight?
        id: BlockHash,
    ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let hash_or_height: HashOrHeight = HashOrHeight::Hash(zebra_chain::block::Hash(id.into()));
        match self {
            ValidatorConnector::State(state) => {
                let mut state = state.clone();
                let block_header_response = state
                    .read_state_service
                    .ready()
                    .and_then(|service| service.call(ReadRequest::BlockHeader(hash_or_height)))
                    .await
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not fetch header of block {id}"))
                                .to_string(),
                        )
                    })?;
                let (_header, _hash, height) = match block_header_response {
                    ReadResponse::BlockHeader {
                        header,
                        hash,
                        height,
                        ..
                    } => (header, hash, height),
                    unexpected => {
                        unreachable!("Unexpected response from state service: {unexpected:?}")
                    }
                };

                let sapling = match zebra_chain::parameters::NetworkUpgrade::Sapling
                    .activation_height(&state.network.to_zebra_network())
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::SaplingTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch sapling treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|sap_response| {
                    expected_read_response!(sap_response, SaplingTree)
                        .map(|tree| tree.to_rpc_bytes())
                });

                let orchard = match zebra_chain::parameters::NetworkUpgrade::Nu5
                    .activation_height(&state.network.to_zebra_network())
                {
                    Some(activation_height) if height >= activation_height => Some(
                        state
                            .read_state_service
                            .ready()
                            .and_then(|service| {
                                service.call(ReadRequest::OrchardTree(hash_or_height))
                            })
                            .await
                            .map_err(|_e| {
                                BlockchainSourceError::Unrecoverable(
                                    InvalidData(format!(
                                        "could not fetch orchard treestate of block {id}"
                                    ))
                                    .to_string(),
                                )
                            })?,
                    ),
                    _ => None,
                }
                .and_then(|orch_response| {
                    expected_read_response!(orch_response, OrchardTree)
                        .map(|tree| tree.to_rpc_bytes())
                });

                Ok((sapling, orchard))
            }
            ValidatorConnector::Fetch(fetch) => {
                let treestate = fetch
                    .get_treestate(hash_or_height.to_string())
                    .await
                    .map_err(|_e| {
                        BlockchainSourceError::Unrecoverable(
                            InvalidData(format!("could not fetch treestate of block {id}"))
                                .to_string(),
                        )
                    })?;

                let sapling = treestate.sapling.commitments().final_state();

                let orchard = treestate.orchard.commitments().final_state();

                Ok((sapling.clone(), orchard.clone()))
            }
        }
    }

    async fn get_mempool_txids(
        &self,
    ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
        let mempool_fetcher = match self {
            ValidatorConnector::State(state) => &state.mempool_fetcher,
            ValidatorConnector::Fetch(fetch) => fetch,
        };

        let txid_strings = mempool_fetcher
            .get_raw_mempool()
            .await
            .map_err(|e| {
                BlockchainSourceError::Unrecoverable(format!("could not fetch mempool data: {e}"))
            })?
            .transactions;

        let txids: Vec<zebra_chain::transaction::Hash> = txid_strings
            .into_iter()
            .map(|txid_str| {
                zebra_chain::transaction::Hash::from_str(&txid_str).map_err(|e| {
                    BlockchainSourceError::Unrecoverable(format!(
                        "invalid transaction id '{txid_str}': {e}"
                    ))
                })
            })
            .collect::<Result<_, _>>()?;

        Ok(Some(txids))
    }

    async fn get_transaction(
        &self,
        txid: TransactionHash,
    ) -> BlockchainSourceResult<Option<Arc<zebra_chain::transaction::Transaction>>> {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                network: _,
            }) => {
                // Check state for transaction
                let mut read_state_service = read_state_service.clone();
                let mempool_fetcher = mempool_fetcher.clone();

                let zebra_txid: zebra_chain::transaction::Hash =
                    zebra_chain::transaction::Hash::from(txid.0);

                let response = read_state_service
                    .ready()
                    .and_then(|svc| svc.call(zebra_state::ReadRequest::Transaction(zebra_txid)))
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!("state read failed: {e}"))
                    })?;

                if let zebra_state::ReadResponse::Transaction(opt) = response {
                    if let Some(mined_tx) = opt {
                        return Ok(Some((mined_tx).tx.clone()));
                    }
                } else {
                    unreachable!("unmatched response to a `Transaction` read request");
                }

                // Else heck mempool for transaction.
                let mempool_txids = self.get_mempool_txids().await?.ok_or_else(|| {
                    BlockchainSourceError::Unrecoverable(
                        "could not fetch mempool transaction ids: none returned".to_string(),
                    )
                })?;

                if mempool_txids.contains(&zebra_txid) {
                    let serialized_transaction = if let GetTransactionResponse::Raw(
                        serialized_transaction,
                    ) = mempool_fetcher
                        .get_raw_transaction(zebra_txid.to_string(), Some(0))
                        .await
                        .map_err(|e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "could not fetch transaction data: {e}"
                            ))
                        })? {
                        serialized_transaction
                    } else {
                        return Err(BlockchainSourceError::Unrecoverable(
                            "could not fetch transaction data: non-raw response".to_string(),
                        ));
                    };
                    let transaction: zebra_chain::transaction::Transaction =
                        zebra_chain::transaction::Transaction::zcash_deserialize(
                            std::io::Cursor::new(serialized_transaction.as_ref()),
                        )
                        .map_err(|e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "could not deserialize transaction data: {e}"
                            ))
                        })?;
                    Ok(Some(transaction.into()))
                } else {
                    Ok(None)
                }
            }
            ValidatorConnector::Fetch(fetch) => {
                let serialized_transaction =
                    if let GetTransactionResponse::Raw(serialized_transaction) = fetch
                        .get_raw_transaction(txid.to_string(), Some(0))
                        .await
                        .map_err(|e| {
                            BlockchainSourceError::Unrecoverable(format!(
                                "could not fetch transaction data: {e}"
                            ))
                        })?
                    {
                        serialized_transaction
                    } else {
                        return Err(BlockchainSourceError::Unrecoverable(
                            "could not fetch transaction data: non-raw response".to_string(),
                        ));
                    };
                let transaction: zebra_chain::transaction::Transaction =
                    zebra_chain::transaction::Transaction::zcash_deserialize(std::io::Cursor::new(
                        serialized_transaction.as_ref(),
                    ))
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not deserialize transaction data: {e}"
                        ))
                    })?;
                Ok(Some(transaction.into()))
            }
        }
    }

    async fn get_best_block_hash(
        &self,
    ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher,
                network: _,
            }) => {
                match read_state_service.best_tip() {
                    Some((_height, hash)) => Ok(Some(hash)),
                    None => {
                        // try RPC if state read fails:
                        Ok(Some(
                            mempool_fetcher
                                .get_best_blockhash()
                                .await
                                .map_err(|e| {
                                    BlockchainSourceError::Unrecoverable(format!(
                                        "could not fetch best block hash from validator: {e}"
                                    ))
                                })?
                                .0,
                        ))
                    }
                }
            }
            ValidatorConnector::Fetch(fetch) => Ok(Some(
                fetch
                    .get_best_blockhash()
                    .await
                    .map_err(|e| {
                        BlockchainSourceError::Unrecoverable(format!(
                            "could not fetch best block hash from validator: {e}"
                        ))
                    })?
                    .0,
            )),
        }
    }

    async fn nonfinalized_listener(
        &self,
    ) -> Result<
        Option<
            tokio::sync::mpsc::Receiver<(zebra_chain::block::Hash, Arc<zebra_chain::block::Block>)>,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        match self {
            ValidatorConnector::State(State {
                read_state_service,
                mempool_fetcher: _,
                network: _,
            }) => {
                match read_state_service
                    .clone()
                    .call(zebra_state::ReadRequest::NonFinalizedBlocksListener)
                    .await
                {
                    Ok(ReadResponse::NonFinalizedBlocksListener(listener)) => {
                        // NOTE:  This is not Option::unwrap, but a custom zebra-defined NonFinalizedBlocksListener::unwrap.
                        Ok(Some(listener.unwrap()))
                    }
                    Ok(_) => unreachable!(),
                    Err(e) => Err(e),
                }
            }
            ValidatorConnector::Fetch(_fetch) => Ok(None),
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    use zebra_chain::{block::Block, orchard::tree as orchard, sapling::tree as sapling};
    use zebra_state::HashOrHeight;

    /// A test-only mock implementation of BlockchainReader using ordered lists by height.
    #[derive(Clone)]
    #[allow(clippy::type_complexity)]
    pub(crate) struct MockchainSource {
        blocks: Vec<Arc<Block>>,
        roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
        treestates: Vec<(Vec<u8>, Vec<u8>)>,
        hashes: Vec<BlockHash>,
        active_chain_height: Arc<AtomicU32>,
    }

    impl MockchainSource {
        /// Creates a new MockchainSource.
        /// All inputs must be the same length, and ordered by ascending height starting from 0.
        #[allow(clippy::type_complexity)]
        pub(crate) fn new(
            blocks: Vec<Arc<Block>>,
            roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
            treestates: Vec<(Vec<u8>, Vec<u8>)>,
            hashes: Vec<BlockHash>,
        ) -> Self {
            assert!(
                blocks.len() == roots.len()
                    && roots.len() == hashes.len()
                    && hashes.len() == treestates.len(),
                "All input vectors must be the same length"
            );

            // len() returns one-indexed length, height is zero-indexed.
            let tip_height = blocks.len().saturating_sub(1) as u32;
            Self {
                blocks,
                roots,
                treestates,
                hashes,
                active_chain_height: Arc::new(AtomicU32::new(tip_height)),
            }
        }

        /// Creates a new MockchainSource, *with* an active chain height.
        ///
        /// Block will only be served up to the active chain height, with mempool data coming from
        /// the *next block in the chain.
        ///
        /// Blocks must be "mined" to extend the active chain height.
        ///
        /// All inputs must be the same length, and ordered by ascending height starting from 0.
        #[allow(clippy::type_complexity)]
        pub(crate) fn new_with_active_height(
            blocks: Vec<Arc<Block>>,
            roots: Vec<(Option<(sapling::Root, u64)>, Option<(orchard::Root, u64)>)>,
            treestates: Vec<(Vec<u8>, Vec<u8>)>,
            hashes: Vec<BlockHash>,
            active_chain_height: u32,
        ) -> Self {
            assert!(
                blocks.len() == roots.len()
                    && roots.len() == hashes.len()
                    && hashes.len() == treestates.len(),
                "All input vectors must be the same length"
            );

            // len() returns one-indexed length, height is zero-indexed.
            let max_height = blocks.len().saturating_sub(1) as u32;
            assert!(
                active_chain_height <= max_height,
                "active_chain_height must be in 0..=len-1"
            );

            Self {
                blocks,
                roots,
                treestates,
                hashes,
                active_chain_height: Arc::new(AtomicU32::new(active_chain_height)),
            }
        }

        pub(crate) fn mine_blocks(&self, blocks: u32) {
            // len() returns one-indexed length, height is zero-indexed.
            let max_height = self.max_chain_height();
            let _ = self.active_chain_height.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |current| {
                    let target = current.saturating_add(blocks).min(max_height);
                    if target == current {
                        None
                    } else {
                        Some(target)
                    }
                },
            );
        }

        pub(crate) fn max_chain_height(&self) -> u32 {
            // len() returns one-indexed length, height is zero-indexed.
            self.blocks.len().saturating_sub(1) as u32
        }

        pub(crate) fn active_height(&self) -> u32 {
            self.active_chain_height.load(Ordering::SeqCst)
        }

        fn valid_height(&self, height: u32) -> Option<usize> {
            let active_chain_height = self.active_height() as usize;
            let valid_height = height as usize;

            if valid_height <= active_chain_height {
                Some(valid_height)
            } else {
                None
            }
        }

        fn valid_hash(&self, hash: &zebra_chain::block::Hash) -> Option<usize> {
            let active_chain_height = self.active_height() as usize;
            let height_index = self.hashes.iter().position(|h| h.0 == hash.0);

            if height_index.is_some() && height_index.unwrap() <= active_chain_height {
                height_index
            } else {
                None
            }
        }
    }

    #[async_trait]
    impl BlockchainSource for MockchainSource {
        async fn get_block(
            &self,
            id: HashOrHeight,
        ) -> BlockchainSourceResult<Option<Arc<zebra_chain::block::Block>>> {
            match id {
                HashOrHeight::Height(h) => {
                    let Some(height_index) = self.valid_height(h.0) else {
                        return Ok(None);
                    };
                    Ok(Some(Arc::clone(&self.blocks[height_index])))
                }
                HashOrHeight::Hash(hash) => {
                    let Some(hash_index) = self.valid_hash(&hash) else {
                        return Ok(None);
                    };

                    Ok(Some(Arc::clone(&self.blocks[hash_index])))
                }
            }
        }

        async fn get_commitment_tree_roots(
            &self,
            id: BlockHash,
        ) -> BlockchainSourceResult<(
            Option<(zebra_chain::sapling::tree::Root, u64)>,
            Option<(zebra_chain::orchard::tree::Root, u64)>,
        )> {
            let active_chain_height = self.active_height() as usize; // serve up to active tip

            if let Some(height) = self.hashes.iter().position(|h| h == &id) {
                if height <= active_chain_height {
                    Ok(self.roots[height])
                } else {
                    Ok((None, None))
                }
            } else {
                Ok((None, None))
            }
        }

        /// Returns the sapling and orchard treestate by hash
        async fn get_treestate(
            &self,
            id: BlockHash,
        ) -> BlockchainSourceResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
            let active_chain_height = self.active_height() as usize; // serve up to active tip

            if let Some(height) = self.hashes.iter().position(|h| h == &id) {
                if height <= active_chain_height {
                    let (sapling_state, orchard_state) = &self.treestates[height];
                    Ok((Some(sapling_state.clone()), Some(orchard_state.clone())))
                } else {
                    Ok((None, None))
                }
            } else {
                Ok((None, None))
            }
        }

        async fn get_mempool_txids(
            &self,
        ) -> BlockchainSourceResult<Option<Vec<zebra_chain::transaction::Hash>>> {
            let mempool_height = self.active_height() as usize + 1;

            let txids = if mempool_height < self.blocks.len() {
                self.blocks[mempool_height]
                    .transactions
                    .iter()
                    .filter(|tx| !tx.is_coinbase()) // <-- exclude coinbase
                    .map(|tx| tx.hash())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            Ok(Some(txids))
        }

        async fn get_transaction(
            &self,
            txid: TransactionHash,
        ) -> BlockchainSourceResult<Option<Arc<zebra_chain::transaction::Transaction>>> {
            let zebra_txid: zebra_chain::transaction::Hash =
                zebra_chain::transaction::Hash::from(txid.0);

            let active_chain_height = self.active_height() as usize;
            let mempool_height = active_chain_height + 1;

            for height in 0..=active_chain_height {
                if height > self.max_chain_height() as usize {
                    break;
                }
                if let Some(found) = self.blocks[height]
                    .transactions
                    .iter()
                    .find(|transaction| transaction.hash() == zebra_txid)
                {
                    return Ok(Some(Arc::clone(found)));
                }
            }

            if mempool_height < self.blocks.len() {
                if let Some(found) = self.blocks[mempool_height]
                    .transactions
                    .iter()
                    .find(|transaction| transaction.hash() == zebra_txid)
                {
                    return Ok(Some(Arc::clone(found)));
                }
            }

            Ok(None)
        }

        async fn get_best_block_hash(
            &self,
        ) -> BlockchainSourceResult<Option<zebra_chain::block::Hash>> {
            let active_chain_height = self.active_height() as usize;

            if self.blocks.is_empty() || active_chain_height > self.max_chain_height() as usize {
                return Ok(None);
            }

            Ok(Some(self.blocks[active_chain_height].hash()))
        }

        async fn nonfinalized_listener(
            &self,
        ) -> Result<
            Option<
                tokio::sync::mpsc::Receiver<(
                    zebra_chain::block::Hash,
                    Arc<zebra_chain::block::Block>,
                )>,
            >,
            Box<dyn Error + Send + Sync>,
        > {
            Ok(None)
        }
    }
}
