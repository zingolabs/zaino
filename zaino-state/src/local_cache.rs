//! Holds Zaino's local compact block cache implementation.

use std::any::type_name;

#[allow(deprecated)]
use crate::{
    config::BlockCacheConfig, error::BlockCacheError, status::StatusType, StateServiceSubscriber,
};

pub mod finalised_state;
pub mod non_finalised_state;

use finalised_state::{FinalisedState, FinalisedStateSubscriber};
use non_finalised_state::{NonFinalisedState, NonFinalisedStateSubscriber};
use tracing::info;
use zaino_fetch::{
    chain::block::FullBlock,
    jsonrpsee::{
        connector::{JsonRpSeeConnector, RpcRequestError},
        error::TransportError,
        response::{GetBlockError, GetBlockResponse},
    },
};
use zaino_proto::proto::compact_formats::{ChainMetadata, CompactBlock, CompactOrchardAction};
use zebra_chain::{
    block::{Hash, Height},
    parameters::Network,
};
use zebra_rpc::methods::{GetBlock, GetBlockTransaction};
use zebra_state::{HashOrHeight, ReadStateService};

/// Zaino's internal compact block cache.
///
/// Used by the FetchService for efficiency.
#[derive(Debug)]
pub struct BlockCache {
    fetcher: JsonRpSeeConnector,
    state: Option<ReadStateService>,
    non_finalised_state: NonFinalisedState,
    /// The state below the last 100 blocks, determined
    /// to be probabalistically nonreorgable
    pub finalised_state: Option<FinalisedState>,
    config: BlockCacheConfig,
}

impl BlockCache {
    /// Spawns a new [`BlockCache`].
    ///
    /// Inputs:
    /// - fetcher: JsonRPC client.
    /// - state: Zebra ReadStateService.
    /// - config: Block cache configuration data.
    pub async fn spawn(
        fetcher: &JsonRpSeeConnector,
        state: Option<&ReadStateService>,
        config: BlockCacheConfig,
    ) -> Result<Self, BlockCacheError> {
        info!("Launching Local Block Cache..");
        let (channel_tx, channel_rx) = tokio::sync::mpsc::channel(100);

        let db_size = config.storage.database.size;
        let finalised_state = match db_size {
            zaino_common::DatabaseSize::Gb(0) => None,
            zaino_common::DatabaseSize::Gb(_) => {
                Some(FinalisedState::spawn(fetcher, state, channel_rx, config.clone()).await?)
            }
        };

        let non_finalised_state =
            NonFinalisedState::spawn(fetcher, state, channel_tx, config.clone()).await?;

        Ok(BlockCache {
            fetcher: fetcher.clone(),
            state: state.cloned(),
            non_finalised_state,
            finalised_state,
            config,
        })
    }

    /// Returns a [`BlockCacheSubscriber`].
    pub fn subscriber(&self) -> BlockCacheSubscriber {
        let finalised_state_subscriber = self
            .finalised_state
            .as_ref()
            .map(FinalisedState::subscriber);
        BlockCacheSubscriber {
            fetcher: self.fetcher.clone(),
            state: self.state.clone(),
            non_finalised_state: self.non_finalised_state.subscriber(),
            finalised_state: finalised_state_subscriber,
            config: self.config.clone(),
        }
    }

    /// Returns the status of the block cache.
    pub fn status(&self) -> StatusType {
        let non_finalised_state_status = self.non_finalised_state.status();
        let finalised_state_status = match self.config.storage.database.size {
            zaino_common::DatabaseSize::Gb(0) => StatusType::Ready,
            zaino_common::DatabaseSize::Gb(_) => match &self.finalised_state {
                Some(finalised_state) => finalised_state.status(),
                None => return StatusType::Offline,
            },
        };

        non_finalised_state_status.combine(finalised_state_status)
    }

    /// Sets the block cache to close gracefully.
    pub fn close(&mut self) {
        self.non_finalised_state.close();
        if self.finalised_state.is_some() {
            self.finalised_state
                .take()
                .expect("error taking Option<(Some)finalised_state> in block_cache::close")
                .close();
        }
    }
}

/// A subscriber to a [`BlockCache`].
#[derive(Debug, Clone)]
pub struct BlockCacheSubscriber {
    fetcher: JsonRpSeeConnector,
    state: Option<ReadStateService>,
    /// the last 100 blocks, stored separately as it could
    /// be changed by reorgs
    pub non_finalised_state: NonFinalisedStateSubscriber,
    /// The state below the last 100 blocks, determined
    /// to be probabalistically nonreorgable
    pub finalised_state: Option<FinalisedStateSubscriber>,
    config: BlockCacheConfig,
}

impl BlockCacheSubscriber {
    /// Returns a Compact Block from the [`BlockCache`].
    pub async fn get_compact_block(
        &self,
        hash_or_height: String,
    ) -> Result<CompactBlock, BlockCacheError> {
        let hash_or_height: HashOrHeight = hash_or_height.parse()?;

        if self
            .non_finalised_state
            .contains_hash_or_height(hash_or_height)
            .await
        {
            // Fetch from non-finalised state.
            self.non_finalised_state
                .get_compact_block(hash_or_height)
                .await
                .map_err(Into::into)
        } else {
            match &self.finalised_state {
                // Fetch from finalised state.
                Some(finalised_state) => finalised_state
                    .get_compact_block(hash_or_height)
                    .await
                    .map_err(Into::into),
                // Fetch from Validator.
                None => {
                    let (_, block) = fetch_block_from_node(
                        self.state.as_ref(),
                        Some(&self.config.network.to_zebra_network()),
                        &self.fetcher,
                        hash_or_height,
                    )
                    .await
                    .map_err(|e| BlockCacheError::Custom(e.to_string()))?;
                    Ok(block)
                }
            }
        }
    }

    /// Returns a compact block holding only action nullifiers.
    ///
    /// NOTE: Currently this only returns Orchard nullifiers to follow Lightwalletd functionality but Sapling could be added if required by wallets.
    pub async fn get_compact_block_nullifiers(
        &self,
        hash_or_height: String,
    ) -> Result<CompactBlock, BlockCacheError> {
        self.get_compact_block(hash_or_height)
            .await
            .map(compact_block_to_nullifiers)
    }

    /// Returns the height of the latest block in the [`BlockCache`].
    pub async fn get_chain_height(&self) -> Result<Height, BlockCacheError> {
        self.non_finalised_state
            .get_chain_height()
            .await
            .map_err(BlockCacheError::NonFinalisedStateError)
    }

    /// Returns the status of the [`BlockCache`]..
    pub fn status(&self) -> StatusType {
        let non_finalised_state_status = self.non_finalised_state.status();
        let finalised_state_status = match self.config.storage.database.size {
            zaino_common::DatabaseSize::Gb(0) => StatusType::Ready,
            zaino_common::DatabaseSize::Gb(_) => match &self.finalised_state {
                Some(finalised_state) => finalised_state.status(),
                None => return StatusType::Offline,
            },
        };

        non_finalised_state_status.combine(finalised_state_status)
    }
}

/// Fetches CompactBlock from the validator.
///
/// Uses 2 calls as z_get_block verbosity=1 is required to fetch txids from zcashd.
pub(crate) async fn fetch_block_from_node(
    state: Option<&ReadStateService>,
    network: Option<&Network>,
    fetcher: &JsonRpSeeConnector,
    hash_or_height: HashOrHeight,
) -> Result<(Hash, CompactBlock), RpcRequestError<GetBlockError>> {
    if let (Some(state), Some(network)) = (state, network) {
        match try_state_path(state, network, hash_or_height).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                eprintln!("StateService fallback triggered due to: {e}");
            }
        }
    }
    try_fetcher_path(fetcher, hash_or_height).await
}

#[allow(deprecated)]
async fn try_state_path(
    state: &ReadStateService,
    network: &Network,
    hash_or_height: HashOrHeight,
) -> Result<(Hash, CompactBlock), BlockCacheError> {
    let (hash, tx, trees) =
        StateServiceSubscriber::get_block_inner(state, network, hash_or_height, Some(1))
            .await
            .map_err(|e| {
                eprintln!("{e}");
                BlockCacheError::Custom("Error retrieving block from ReadStateService".to_string())
            })
            .and_then(|response| match response {
                GetBlock::Raw(_) => Err(BlockCacheError::Custom(
                    "Found transaction of `Raw` type, expected only `Hash` types.".to_string(),
                )),
                GetBlock::Object(block_obj) => {
                    Ok((block_obj.hash(), block_obj.tx().clone(), block_obj.trees()))
                }
            })?;

    StateServiceSubscriber::get_block_inner(state, network, hash_or_height, Some(0))
        .await
        .map_err(|_| {
            BlockCacheError::Custom("Error retrieving raw block from ReadStateService".to_string())
        })
        .and_then(|response| match response {
            GetBlock::Object { .. } => Err(BlockCacheError::Custom(
                "Found transaction of `Object` type, expected only `Hash` types.".to_string(),
            )),
            GetBlock::Raw(block_hex) => {
                let txid_strings = tx
                    .iter()
                    .filter_map(|t| {
                        if let GetBlockTransaction::Hash(h) = t {
                            Some(h.to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<String>>();

                Ok((
                    hash,
                    FullBlock::parse_from_hex(
                        block_hex.as_ref(),
                        Some(display_txids_to_server(txid_strings)?),
                    )?
                    .into_compact(
                        u32::try_from(trees.sapling())?,
                        u32::try_from(trees.orchard())?,
                    )?,
                ))
            }
        })
}

async fn try_fetcher_path(
    fetcher: &JsonRpSeeConnector,
    hash_or_height: HashOrHeight,
) -> Result<(Hash, CompactBlock), RpcRequestError<GetBlockError>> {
    let (hash, tx, trees) = fetcher
        .get_block(hash_or_height.to_string(), Some(1))
        .await
        .and_then(|response| match response {
            GetBlockResponse::Raw(_) => {
                Err(RpcRequestError::Transport(TransportError::BadNodeData(
                    Box::new(std::io::Error::other("unexpected raw block response")),
                    type_name::<GetBlockError>(),
                )))
            }
            GetBlockResponse::Object(block) => Ok((block.hash, block.tx, block.trees)),
        })?;

    fetcher
        .get_block(hash.0.to_string(), Some(0))
        .await
        .and_then(|response| match response {
            GetBlockResponse::Object { .. } => {
                Err(RpcRequestError::Transport(TransportError::BadNodeData(
                    Box::new(std::io::Error::other("unexpected object block response")),
                    type_name::<GetBlockError>(),
                )))
            }
            GetBlockResponse::Raw(block_hex) => Ok((
                hash.0,
                FullBlock::parse_from_hex(
                    block_hex.as_ref(),
                    Some(display_txids_to_server(tx).map_err(|e| {
                        RpcRequestError::Transport(TransportError::BadNodeData(
                            Box::new(e),
                            type_name::<GetBlockError>(),
                        ))
                    })?),
                )
                .map_err(|e| {
                    RpcRequestError::Transport(TransportError::BadNodeData(
                        Box::new(e),
                        type_name::<GetBlockError>(),
                    ))
                })?
                .into_compact(
                    u32::try_from(trees.sapling()).map_err(|e| {
                        RpcRequestError::Transport(TransportError::BadNodeData(
                            Box::new(e),
                            type_name::<GetBlockError>(),
                        ))
                    })?,
                    u32::try_from(trees.orchard()).map_err(|e| {
                        RpcRequestError::Transport(TransportError::BadNodeData(
                            Box::new(e),
                            type_name::<GetBlockError>(),
                        ))
                    })?,
                )
                .map_err(|e| {
                    RpcRequestError::Transport(TransportError::BadNodeData(
                        Box::new(e),
                        type_name::<GetBlockError>(),
                    ))
                })?,
            )),
        })
}

/// Takes a vec of big endian hex encoded txids and returns them as a vec of little endian raw bytes.
pub(crate) fn display_txids_to_server(txids: Vec<String>) -> Result<Vec<Vec<u8>>, BlockCacheError> {
    txids
        .iter()
        .map(|txid| {
            txid.as_bytes()
                .chunks(2)
                .map(|chunk| {
                    let hex_pair = std::str::from_utf8(chunk).map_err(BlockCacheError::from)?;
                    u8::from_str_radix(hex_pair, 16).map_err(BlockCacheError::from)
                })
                .rev()
                .collect::<Result<Vec<u8>, _>>()
        })
        .collect::<Result<Vec<Vec<u8>>, _>>()
}

/// Strips the ouputs and from all transactions, retains only
/// the nullifier from all orcard actions, and clears the chain
/// metadata from the block
pub(crate) fn compact_block_to_nullifiers(mut block: CompactBlock) -> CompactBlock {
    for ctransaction in &mut block.vtx {
        ctransaction.outputs = Vec::new();
        for caction in &mut ctransaction.actions {
            *caction = CompactOrchardAction {
                nullifier: caction.nullifier.clone(),
                ..Default::default()
            }
        }
    }

    block.chain_metadata = Some(ChainMetadata {
        sapling_commitment_tree_size: 0,
        orchard_commitment_tree_size: 0,
    });
    block
}
