//! Compact Block Cache non-finalised state implementation.

use std::collections::HashSet;

use tracing::{error, info, warn};
use zaino_fetch::jsonrpc::connector::JsonRpcConnector;
use zaino_proto::proto::compact_formats::CompactBlock;
use zebra_chain::block::{Hash, Height};
use zebra_state::HashOrHeight;

use crate::{
    broadcast::{Broadcast, BroadcastSubscriber},
    config::BlockCacheConfig,
    error::NonFinalisedStateError,
    local_cache::fetch_block_from_node,
    status::{AtomicStatus, StatusType},
};

/// Non-finalised part of the chain (last 100 blocks), held in memory to ease the handling of reorgs.
///
/// NOTE: We hold the last 102 blocks to ensure there are no gaps in the block cache.
#[derive(Debug)]
pub struct NonFinalisedState {
    /// Chain fetch service.
    fetcher: JsonRpcConnector,
    /// Broadcast containing `<block_height, block_hash>`.
    heights_to_hashes: Broadcast<Height, Hash>,
    /// Broadcast containing `<block_hash, compact_block>`.
    hashes_to_blocks: Broadcast<Hash, CompactBlock>,
    /// Sync task handle.
    sync_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Used to send blocks to the finalised state.
    block_sender: tokio::sync::mpsc::Sender<(Height, Hash, CompactBlock)>,
    /// Non-finalised state status.
    status: AtomicStatus,
    /// BlockCache config data.
    config: BlockCacheConfig,
}

impl NonFinalisedState {
    /// Spawns a new [`NonFinalisedState`].
    pub async fn spawn(
        fetcher: &JsonRpcConnector,
        block_sender: tokio::sync::mpsc::Sender<(Height, Hash, CompactBlock)>,
        config: BlockCacheConfig,
    ) -> Result<Self, NonFinalisedStateError> {
        info!("Launching Non-Finalised State..");
        let mut non_finalised_state = NonFinalisedState {
            fetcher: fetcher.clone(),
            heights_to_hashes: Broadcast::new(config.map_capacity, config.map_shard_amount),
            hashes_to_blocks: Broadcast::new(config.map_capacity, config.map_shard_amount),
            sync_task_handle: None,
            block_sender,
            status: AtomicStatus::new(StatusType::Spawning.into()),
            config,
        };

        non_finalised_state.wait_on_server().await?;

        let chain_height = fetcher.get_blockchain_info().await?.blocks.0;
        // We do not fetch pre sapling activation.
        for height in chain_height.saturating_sub(99).max(
            non_finalised_state
                .config
                .network
                .sapling_activation_height()
                .0,
        )..=chain_height
        {
            loop {
                match fetch_block_from_node(
                    &non_finalised_state.fetcher,
                    HashOrHeight::Height(Height(height)),
                )
                .await
                {
                    Ok((hash, block)) => {
                        non_finalised_state
                            .heights_to_hashes
                            .insert(Height(height), hash, None);
                        non_finalised_state
                            .hashes_to_blocks
                            .insert(hash, block, None);
                        break;
                    }
                    Err(e) => {
                        non_finalised_state.update_status_and_notify(StatusType::RecoverableError);
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }

        non_finalised_state.sync_task_handle = Some(non_finalised_state.serve().await?);

        Ok(non_finalised_state)
    }

    async fn serve(&self) -> Result<tokio::task::JoinHandle<()>, NonFinalisedStateError> {
        let non_finalised_state = Self {
            fetcher: self.fetcher.clone(),
            heights_to_hashes: self.heights_to_hashes.clone(),
            hashes_to_blocks: self.hashes_to_blocks.clone(),
            sync_task_handle: None,
            block_sender: self.block_sender.clone(),
            status: self.status.clone(),
            config: self.config.clone(),
        };

        let sync_handle = tokio::spawn(async move {
            let mut best_block_hash: Hash;
            let mut check_block_hash: Hash;

            loop {
                match non_finalised_state.fetcher.get_blockchain_info().await {
                    Ok(chain_info) => {
                        best_block_hash = chain_info.best_block_hash;
                        non_finalised_state.status.store(StatusType::Ready.into());
                        break;
                    }
                    Err(e) => {
                        non_finalised_state.update_status_and_notify(StatusType::RecoverableError);
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }

            loop {
                if non_finalised_state.status.load() == StatusType::Closing as usize {
                    non_finalised_state.update_status_and_notify(StatusType::Closing);
                    return;
                }

                match non_finalised_state.fetcher.get_blockchain_info().await {
                    Ok(chain_info) => {
                        check_block_hash = chain_info.best_block_hash;
                    }
                    Err(e) => {
                        non_finalised_state.update_status_and_notify(StatusType::RecoverableError);
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                }

                if check_block_hash != best_block_hash {
                    best_block_hash = check_block_hash;
                    non_finalised_state.status.store(StatusType::Syncing.into());
                    non_finalised_state
                        .heights_to_hashes
                        .notify(non_finalised_state.status.clone().into());
                    non_finalised_state
                        .hashes_to_blocks
                        .notify(non_finalised_state.status.clone().into());
                    loop {
                        match non_finalised_state.fill_from_reorg().await {
                            Ok(_) => break,
                            Err(NonFinalisedStateError::Critical(e)) => {
                                non_finalised_state
                                    .update_status_and_notify(StatusType::CriticalError);
                                error!("{e}");
                                return;
                            }
                            Err(e) => {
                                non_finalised_state
                                    .update_status_and_notify(StatusType::RecoverableError);
                                warn!("{e}");
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        }
                    }
                }
                non_finalised_state.status.store(StatusType::Ready.into());
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });

        Ok(sync_handle)
    }

    /// Looks back through the chain to find reorg height and repopulates block cache.
    ///
    /// Newly mined blocks are treated as a reorg at chain_height[-0].
    async fn fill_from_reorg(&self) -> Result<(), NonFinalisedStateError> {
        let mut reorg_height = *self
            .heights_to_hashes
            .get_state()
            .iter()
            .max_by_key(|entry| entry.key().0)
            .ok_or_else(|| {
                NonFinalisedStateError::MissingData(
                    "Failed to find the maximum height in the non-finalised state.".to_string(),
                )
            })?
            .key();

        let mut reorg_hash = self.heights_to_hashes.get(&reorg_height).ok_or_else(|| {
            NonFinalisedStateError::MissingData(format!(
                "Missing hash for height: {}",
                reorg_height.0
            ))
        })?;

        let mut check_hash = match self
            .fetcher
            .get_block(reorg_height.0.to_string(), Some(1))
            .await?
        {
            zaino_fetch::jsonrpc::response::GetBlockResponse::Object { hash, .. } => hash.0,
            _ => {
                return Err(NonFinalisedStateError::Custom(
                    "Unexpected block response type".to_string(),
                ))
            }
        };

        // Find reorg height.
        //
        // Here this is the latest height at which the internal block hash matches the server block hash.
        while reorg_hash != check_hash.into() {
            match reorg_height.previous() {
                Ok(height) => reorg_height = height,
                // Underflow error meaning reorg_height = start of chain.
                // This means the whole non-finalised state is old.
                Err(_) => {
                    self.heights_to_hashes.clear();
                    self.hashes_to_blocks.clear();
                    break;
                }
            };

            reorg_hash = self.heights_to_hashes.get(&reorg_height).ok_or_else(|| {
                NonFinalisedStateError::MissingData(format!(
                    "Missing hash for height: {}",
                    reorg_height.0
                ))
            })?;

            check_hash = match self
                .fetcher
                .get_block(reorg_height.0.to_string(), Some(1))
                .await?
            {
                zaino_fetch::jsonrpc::response::GetBlockResponse::Object { hash, .. } => hash.0,
                _ => {
                    return Err(NonFinalisedStateError::Custom(
                        "Unexpected block response type".to_string(),
                    ))
                }
            };
        }

        // Refill from max(reorg_height[+1], sapling_activation_height).
        //
        // reorg_height + 1 is used here as reorg_height represents the last "valid" block known.
        let validator_height = self.fetcher.get_blockchain_info().await?.blocks.0;
        for block_height in ((reorg_height.0 + 1)
            .max(self.config.network.sapling_activation_height().0))
            ..=validator_height
        {
            // Either pop the reorged block or pop the oldest block in non-finalised state.
            // If we pop the oldest (valid) block we send it to the finalised state to be saved to disk.
            if self.heights_to_hashes.contains_key(&Height(block_height)) {
                if let Some(hash) = self.heights_to_hashes.get(&Height(block_height)) {
                    self.hashes_to_blocks.remove(&hash, None);
                    self.heights_to_hashes.remove(&Height(block_height), None);
                }
            } else {
                let pop_height = *self
                    .heights_to_hashes
                    .get_state()
                    .iter()
                    .min_by_key(|entry| entry.key().0)
                    .ok_or_else(|| {
                        NonFinalisedStateError::MissingData(
                            "Failed to find the minimum height in the non-finalised state."
                                .to_string(),
                        )
                    })?
                    .key();
                // Only pop block if it is outside of the non-finalised state block range.
                if pop_height.0 < (validator_height.saturating_sub(100)) {
                    if let Some(hash) = self.heights_to_hashes.get(&pop_height) {
                        // Send to FinalisedState if db is active.
                        if !self.config.no_db {
                            if let Some(block) = self.hashes_to_blocks.get(&hash) {
                                if self
                                    .block_sender
                                    .send((pop_height, *hash, block.as_ref().clone()))
                                    .await
                                    .is_err()
                                {
                                    self.status.store(StatusType::CriticalError.into());
                                    return Err(NonFinalisedStateError::Critical(
                                        "Critical error in database. Closing NonFinalisedState"
                                            .to_string(),
                                    ));
                                }
                            }
                        }
                        self.hashes_to_blocks.remove(&hash, None);
                        self.heights_to_hashes.remove(&pop_height, None);
                    }
                }
            }
            loop {
                match fetch_block_from_node(
                    &self.fetcher,
                    HashOrHeight::Height(Height(block_height)),
                )
                .await
                {
                    Ok((hash, block)) => {
                        self.heights_to_hashes
                            .insert(Height(block_height), hash, None);
                        self.hashes_to_blocks.insert(hash, block, None);
                        info!(
                            "Block at height [{}] with hash [{}] successfully committed to non-finalised state.",
                            block_height, hash,
                        );
                        break;
                    }
                    Err(e) => {
                        self.update_status_and_notify(StatusType::RecoverableError);
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Waits for server to sync with p2p network.
    pub async fn wait_on_server(&self) -> Result<(), NonFinalisedStateError> {
        // If no_db is active wait for server to sync with p2p network.
        if self.config.no_db && !self.config.network.is_regtest() && !self.config.no_sync {
            self.status.store(StatusType::Syncing.into());
            loop {
                let blockchain_info = self.fetcher.get_blockchain_info().await?;
                if (blockchain_info.blocks.0 as i64 - blockchain_info.estimated_height.0 as i64)
                    .abs()
                    <= 10
                {
                    break;
                } else {
                    info!(" - Validator syncing with network. Validator chain height: {}, Estimated Network chain height: {}",
                        &blockchain_info.blocks.0,
                        &blockchain_info.estimated_height.0
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
            }
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Returns a [`NonFinalisedStateSubscriber`].
    pub fn subscriber(&self) -> NonFinalisedStateSubscriber {
        NonFinalisedStateSubscriber {
            heights_to_hashes: self.heights_to_hashes.subscriber(),
            hashes_to_blocks: self.hashes_to_blocks.subscriber(),
            status: self.status.clone(),
        }
    }

    /// Returns the status of the non-finalised state.
    pub fn status(&self) -> StatusType {
        self.status.load().into()
    }

    /// Updates the status of the non-finalised state and notifies subscribers.
    fn update_status_and_notify(&self, status: StatusType) {
        self.status.store(status.into());
        self.heights_to_hashes.notify(self.status.clone().into());
        self.hashes_to_blocks.notify(self.status.clone().into());
    }

    /// Sets the non-finalised state to close gracefully.
    pub fn close(&mut self) {
        self.update_status_and_notify(StatusType::Closing);
        if let Some(handle) = self.sync_task_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for NonFinalisedState {
    fn drop(&mut self) {
        self.update_status_and_notify(StatusType::Closing);
        if let Some(handle) = self.sync_task_handle.take() {
            handle.abort();
        }
    }
}

/// A subscriber to a [`NonFinalisedState`].
#[derive(Debug, Clone)]
pub struct NonFinalisedStateSubscriber {
    heights_to_hashes: BroadcastSubscriber<Height, Hash>,
    hashes_to_blocks: BroadcastSubscriber<Hash, CompactBlock>,
    status: AtomicStatus,
}

impl NonFinalisedStateSubscriber {
    /// Returns a Compact Block from the non-finalised state.
    pub async fn get_compact_block(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<CompactBlock, NonFinalisedStateError> {
        let hash = match hash_or_height {
            HashOrHeight::Hash(hash) => hash,
            HashOrHeight::Height(height) => {
                *self.heights_to_hashes.get(&height).ok_or_else(|| {
                    NonFinalisedStateError::MissingData(format!(
                        "Height not found in non-finalised state: {}",
                        height.0
                    ))
                })?
            }
        };

        self.hashes_to_blocks
            .get(&hash)
            .map(|block| block.as_ref().clone())
            .ok_or_else(|| {
                NonFinalisedStateError::MissingData(format!("Block not found for hash: {}", hash))
            })
    }

    /// Returns the height of the latest block in the non-finalised state.
    pub async fn get_chain_height(&self) -> Result<Height, NonFinalisedStateError> {
        let (height, _) = *self
            .heights_to_hashes
            .get_filtered_state(&HashSet::new())
            .iter()
            .max_by_key(|(height, ..)| height.0)
            .ok_or_else(|| {
                NonFinalisedStateError::MissingData("Non-finalised state is empty.".into())
            })?;

        Ok(height)
    }

    /// Predicate checks for presence of Hash..  or Height?
    pub async fn contains_hash_or_height(&self, hash_or_height: HashOrHeight) -> bool {
        match hash_or_height {
            HashOrHeight::Height(height) => self.heights_to_hashes.contains_key(&height),
            HashOrHeight::Hash(hash) => self.hashes_to_blocks.contains_key(&hash),
        }
    }

    /// Returns the status of the NonFinalisedState.
    pub fn status(&self) -> StatusType {
        self.status.load().into()
    }
}
