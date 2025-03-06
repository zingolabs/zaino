//! Holds Zaino's local compact block cache implementation.

use crate::{config::BlockCacheConfig, error::BlockCacheError, status::StatusType};

pub mod finalised_state;
pub mod non_finalised_state;

use finalised_state::{FinalisedState, FinalisedStateSubscriber};
use non_finalised_state::{NonFinalisedState, NonFinalisedStateSubscriber};
use tracing::info;
use zaino_fetch::{
    chain::block::FullBlock,
    jsonrpc::{connector::JsonRpcConnector, response::GetBlockResponse},
};
use zaino_proto::proto::compact_formats::{
    ChainMetadata, CompactBlock, CompactOrchardAction, CompactTx,
};
use zebra_chain::block::{Hash, Height};
use zebra_state::HashOrHeight;

/// Zaino's internal compact block cache.
///
/// Used by the FetchService for efficiency.
#[derive(Debug)]
pub struct BlockCache {
    fetcher: JsonRpcConnector,
    non_finalised_state: NonFinalisedState,
    /// The state below the last 100 blocks, determined
    /// to be probabalistically nonreorgable
    pub finalised_state: Option<FinalisedState>,
    config: BlockCacheConfig,
}

impl BlockCache {
    /// Spawns a new [`BlockCache`].
    pub async fn spawn(
        fetcher: &JsonRpcConnector,
        config: BlockCacheConfig,
    ) -> Result<Self, BlockCacheError> {
        info!("Launching Local Block Cache..");
        let (channel_tx, channel_rx) = tokio::sync::mpsc::channel(100);

        let finalised_state = if !config.no_db {
            Some(FinalisedState::spawn(fetcher, channel_rx, config.clone()).await?)
        } else {
            None
        };

        let non_finalised_state =
            NonFinalisedState::spawn(fetcher, channel_tx, config.clone()).await?;

        Ok(BlockCache {
            fetcher: fetcher.clone(),
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
            non_finalised_state: self.non_finalised_state.subscriber(),
            finalised_state: finalised_state_subscriber,
            config: self.config.clone(),
        }
    }

    /// Returns the status of the block cache.
    pub fn status(&self) -> StatusType {
        let non_finalised_state_status = self.non_finalised_state.status();
        let finalised_state_status = match self.config.no_db {
            true => StatusType::Ready,
            false => match &self.finalised_state {
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
    fetcher: JsonRpcConnector,
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
                .map_err(BlockCacheError::NonFinalisedStateError)
        } else {
            match &self.finalised_state {
                // Fetch from finalised state.
                Some(finalised_state) => finalised_state
                    .get_compact_block(hash_or_height)
                    .await
                    .map_err(BlockCacheError::FinalisedStateError),
                // Fetch from Validator.
                None => {
                    let (_, block) = fetch_block_from_node(&self.fetcher, hash_or_height).await?;
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
        match self.get_compact_block(hash_or_height).await {
            Ok(block) => Ok(CompactBlock {
                proto_version: block.proto_version,
                height: block.height,
                hash: block.hash,
                prev_hash: block.prev_hash,
                time: block.time,
                header: block.header,
                vtx: block
                    .vtx
                    .into_iter()
                    .map(|tx| CompactTx {
                        index: tx.index,
                        hash: tx.hash,
                        fee: tx.fee,
                        spends: tx.spends,
                        outputs: Vec::new(),
                        actions: tx
                            .actions
                            .into_iter()
                            .map(|action| CompactOrchardAction {
                                nullifier: action.nullifier,
                                cmx: Vec::new(),
                                ephemeral_key: Vec::new(),
                                ciphertext: Vec::new(),
                            })
                            .collect(),
                    })
                    .collect(),
                chain_metadata: Some(ChainMetadata {
                    sapling_commitment_tree_size: 0,
                    orchard_commitment_tree_size: 0,
                }),
            }),
            Err(e) => Err(e),
        }
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
        let finalised_state_status = match self.config.no_db {
            true => StatusType::Ready,
            false => match &self.finalised_state {
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
    fetcher: &JsonRpcConnector,
    hash_or_height: HashOrHeight,
) -> Result<(Hash, CompactBlock), BlockCacheError> {
    let (hash, tx, trees) = fetcher
        .get_block(hash_or_height.to_string(), Some(1))
        .await
        .map_err(BlockCacheError::from)
        .and_then(|response| match response {
            GetBlockResponse::Raw(_) => Err(BlockCacheError::Custom(
                "Found transaction of `Raw` type, expected only `Hash` types.".to_string(),
            )),
            GetBlockResponse::Object {
                hash, tx, trees, ..
            } => Ok((hash, tx, trees)),
        })?;
    fetcher
        .get_block(hash.0.to_string(), Some(0))
        .await
        .map_err(BlockCacheError::from)
        .and_then(|response| match response {
            GetBlockResponse::Object { .. } => Err(BlockCacheError::Custom(
                "Found transaction of `Object` type, expected only `Hash` types.".to_string(),
            )),
            GetBlockResponse::Raw(block_hex) => Ok((
                hash.0,
                FullBlock::parse_from_hex(block_hex.as_ref(), Some(display_txids_to_server(tx)?))?
                    .into_compact(
                        u32::try_from(trees.sapling())?,
                        u32::try_from(trees.orchard())?,
                    )?,
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
