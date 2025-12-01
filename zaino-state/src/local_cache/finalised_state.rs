//! Compact Block Cache finalised state implementation.

use lmdb::{Cursor, Database, Environment, Transaction};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{fs, sync::Arc};
use tracing::{error, info, warn};

use zebra_chain::{
    block::{Hash, Height},
    parameters::NetworkKind,
};
use zebra_state::{HashOrHeight, ReadStateService};

use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
use zaino_proto::proto::compact_formats::CompactBlock;

use crate::{
    config::BlockCacheConfig,
    error::FinalisedStateError,
    local_cache::fetch_block_from_node,
    status::{AtomicStatus, StatusType},
};

/// Wrapper for `Height`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DbHeight(pub Height);

impl DbHeight {
    /// Converts `[DbHeight]` to 4-byte **big-endian** bytes.
    /// Used when storing as an LMDB key.
    fn to_be_bytes(self) -> [u8; 4] {
        self.0 .0.to_be_bytes()
    }

    /// Parse a 4-byte **big-endian** array into a `[DbHeight]`.
    fn from_be_bytes(bytes: &[u8]) -> Result<Self, FinalisedStateError> {
        let arr: [u8; 4] = bytes
            .try_into()
            .map_err(|_| FinalisedStateError::Custom("Invalid height key length".to_string()))?;
        Ok(DbHeight(Height(u32::from_be_bytes(arr))))
    }
}

/// Wrapper for `Hash`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct DbHash(pub Hash);

/// Wrapper for `CompactBlock`.
#[derive(Debug, Clone, PartialEq)]
struct DbCompactBlock(pub CompactBlock);

/// Custom `Serialize` implementation using Prost's `encode_to_vec()`.
impl Serialize for DbCompactBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let bytes = self.0.encode_to_vec();
        serializer.serialize_bytes(&bytes)
    }
}

/// Custom `Deserialize` implementation using Prost's `decode()`.
impl<'de> Deserialize<'de> for DbCompactBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes: Vec<u8> = serde::de::Deserialize::deserialize(deserializer)?;
        CompactBlock::decode(&*bytes)
            .map(DbCompactBlock)
            .map_err(serde::de::Error::custom)
    }
}

/// A Zaino database request.
#[derive(Debug)]
struct DbRequest {
    hash_or_height: HashOrHeight,
    response_channel: tokio::sync::oneshot::Sender<Result<CompactBlock, FinalisedStateError>>,
}

impl DbRequest {
    /// Creates a new [`DbRequest`].
    fn new(
        hash_or_height: HashOrHeight,
        response_channel: tokio::sync::oneshot::Sender<Result<CompactBlock, FinalisedStateError>>,
    ) -> Self {
        Self {
            hash_or_height,
            response_channel,
        }
    }
}

/// Fanalised part of the chain, held in an LMDB database.
#[derive(Debug)]
pub struct FinalisedState {
    /// JsonRPC client based chain fetch service.
    fetcher: JsonRpSeeConnector,
    /// Optional ReadStateService based chain fetch service.
    state: Option<ReadStateService>,
    /// LMDB Database Environmant.
    database: Arc<Environment>,
    /// LMDB Database containing `<block_height, block_hash>`.
    heights_to_hashes: Database,
    /// LMDB Database containing `<block_hash, compact_block>`.
    hashes_to_blocks: Database,
    /// Database reader request sender.
    request_sender: tokio::sync::mpsc::Sender<DbRequest>,
    /// Database reader task handle.
    read_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Database writer task handle.
    write_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// Non-finalised state status.
    status: AtomicStatus,
    /// BlockCache config data.
    config: BlockCacheConfig,
}

impl FinalisedState {
    /// Spawns a new [`Self`] and syncs the FinalisedState to the servers finalised state.
    ///
    /// Inputs:
    /// - fetcher: Json RPC client.
    /// - db_path: File path of the db.
    /// - db_size: Max size of the db in gb.
    /// - block_reciever: Channel that recieves new blocks to add to the db.
    /// - status_signal: Used to send error status signals to outer processes.
    pub async fn spawn(
        fetcher: &JsonRpSeeConnector,
        state: Option<&ReadStateService>,
        block_receiver: tokio::sync::mpsc::Receiver<(Height, Hash, CompactBlock)>,
        config: BlockCacheConfig,
    ) -> Result<Self, FinalisedStateError> {
        info!("Launching Finalised State..");
        let db_size_bytes = config.storage.database.size.to_byte_count();
        let db_path_dir = match config.network.to_zebra_network().kind() {
            NetworkKind::Mainnet => "live",
            NetworkKind::Testnet => "test",
            NetworkKind::Regtest => "local",
        };
        let db_path = config.storage.database.path.join(db_path_dir);
        if !db_path.exists() {
            fs::create_dir_all(&db_path)?;
        }
        let database = Arc::new(
            Environment::new()
                .set_max_dbs(2)
                .set_map_size(db_size_bytes)
                .open(&db_path)?,
        );

        let heights_to_hashes = match database.open_db(Some("heights_to_hashes")) {
            Ok(db) => db,
            Err(lmdb::Error::NotFound) => {
                database.create_db(Some("heights_to_hashes"), lmdb::DatabaseFlags::empty())?
            }
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        };
        let hashes_to_blocks = match database.open_db(Some("hashes_to_blocks")) {
            Ok(db) => db,
            Err(lmdb::Error::NotFound) => {
                database.create_db(Some("hashes_to_blocks"), lmdb::DatabaseFlags::empty())?
            }
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        };

        let (request_tx, request_rx) = tokio::sync::mpsc::channel(124);

        let mut finalised_state = FinalisedState {
            fetcher: fetcher.clone(),
            state: state.cloned(),
            database,
            heights_to_hashes,
            hashes_to_blocks,
            request_sender: request_tx,
            read_task_handle: None,
            write_task_handle: None,
            status: AtomicStatus::new(StatusType::Spawning),
            config,
        };

        finalised_state.sync_db_from_reorg().await?;
        finalised_state.spawn_writer(block_receiver).await?;
        finalised_state.spawn_reader(request_rx).await?;

        finalised_state.status.store(StatusType::Ready);

        Ok(finalised_state)
    }

    async fn spawn_writer(
        &mut self,
        mut block_receiver: tokio::sync::mpsc::Receiver<(Height, Hash, CompactBlock)>,
    ) -> Result<(), FinalisedStateError> {
        let finalised_state = Self {
            fetcher: self.fetcher.clone(),
            state: self.state.clone(),
            database: Arc::clone(&self.database),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            request_sender: self.request_sender.clone(),
            read_task_handle: None,
            write_task_handle: None,
            status: self.status.clone(),
            config: self.config.clone(),
        };

        let writer_handle = tokio::spawn(async move {
            while let Some((height, mut hash, mut compact_block)) = block_receiver.recv().await {
                let mut retry_attempts = 3;

                loop {
                    match finalised_state.insert_block((height, hash, compact_block.clone())) {
                        Ok(_) => {
                            info!(
                                "Block at height [{}] with hash [{}] successfully committed to finalised state.",
                                height.0, hash
                            );
                            break;
                        }
                        Err(FinalisedStateError::LmdbError(lmdb::Error::KeyExist)) => {
                            match finalised_state.get_hash(height.0) {
                                Ok(db_hash) => {
                                    if db_hash != hash {
                                        if finalised_state.delete_block(height).is_err() {
                                            finalised_state.status.store(StatusType::CriticalError);
                                            return;
                                        };
                                        continue;
                                    } else {
                                        info!(
                                            "Block at height {} already exists, skipping.",
                                            height.0
                                        );
                                        break;
                                    }
                                }
                                Err(_) => {
                                    finalised_state.status.store(StatusType::CriticalError);
                                    return;
                                }
                            }
                        }
                        Err(FinalisedStateError::LmdbError(db_err)) => {
                            error!("LMDB error inserting block {}: {:?}", height.0, db_err);
                            finalised_state.status.store(StatusType::CriticalError);
                            return;
                        }
                        Err(e) => {
                            warn!(
                                "Unknown error inserting block {}: {:?}. Retrying...",
                                height.0, e
                            );

                            if retry_attempts == 0 {
                                error!(
                                    "Failed to insert block {} after multiple retries.",
                                    height.0
                                );
                                finalised_state.status.store(StatusType::CriticalError);
                                return;
                            }

                            retry_attempts -= 1;

                            match fetch_block_from_node(
                                finalised_state.state.as_ref(),
                                Some(&finalised_state.config.network.to_zebra_network()),
                                &finalised_state.fetcher,
                                HashOrHeight::Height(height),
                            )
                            .await
                            {
                                Ok((new_hash, new_compact_block)) => {
                                    warn!(
                                        "Re-fetched block at height {}, retrying insert.",
                                        height.0
                                    );
                                    hash = new_hash;
                                    compact_block = new_compact_block;
                                }
                                Err(fetch_err) => {
                                    error!(
                                        "Failed to fetch block {} from validator: {:?}",
                                        height.0, fetch_err
                                    );
                                    finalised_state.status.store(StatusType::CriticalError);
                                    return;
                                }
                            }

                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            }
        });

        self.write_task_handle = Some(writer_handle);
        Ok(())
    }

    async fn spawn_reader(
        &mut self,
        mut request_receiver: tokio::sync::mpsc::Receiver<DbRequest>,
    ) -> Result<(), FinalisedStateError> {
        let finalised_state = Self {
            fetcher: self.fetcher.clone(),
            state: self.state.clone(),
            database: Arc::clone(&self.database),
            heights_to_hashes: self.heights_to_hashes,
            hashes_to_blocks: self.hashes_to_blocks,
            request_sender: self.request_sender.clone(),
            read_task_handle: None,
            write_task_handle: None,
            status: self.status.clone(),
            config: self.config.clone(),
        };

        let reader_handle = tokio::spawn(async move {
            while let Some(DbRequest {
                hash_or_height,
                response_channel,
            }) = request_receiver.recv().await
            {
                let response = match finalised_state.get_block(hash_or_height) {
                    Ok(block) => Ok(block),
                    Err(_) => {
                        warn!("Failed to fetch block from DB, re-fetching from validator.");
                        match fetch_block_from_node(
                            finalised_state.state.as_ref(),
                            Some(&finalised_state.config.network.to_zebra_network()),
                            &finalised_state.fetcher,
                            hash_or_height,
                        )
                        .await
                        {
                            Ok((hash, block)) => {
                                match finalised_state.insert_block((
                                    Height(block.height as u32),
                                    hash,
                                    block.clone(),
                                )) {
                                    Ok(_) => Ok(block),
                                    Err(_) => {
                                        warn!("Failed to insert missing block into DB, serving from validator.");
                                        Ok(block)
                                    }
                                }
                            }
                            Err(_) => Err(FinalisedStateError::Custom(format!(
                                "Block {hash_or_height:?} not found in finalised state or validator."
                            ))),
                        }
                    }
                };

                if response_channel.send(response).is_err() {
                    warn!("Failed to send response for request: {:?}", hash_or_height);
                }
            }
        });

        self.read_task_handle = Some(reader_handle);
        Ok(())
    }

    /// Syncs database with the server, and waits for server to sync with P2P network.
    ///
    /// Checks for reorg before syncing:
    /// - Searches from ZainoDB tip backwards looking for the last valid block in the database and sets `reorg_height` to the last VALID block.
    /// - Re-populated the database from the NEXT block in the chain (`reorg_height + 1`).
    async fn sync_db_from_reorg(&self) -> Result<(), FinalisedStateError> {
        let network = self.config.network.to_zebra_network();

        let mut reorg_height = self.get_db_height().unwrap_or(Height(0));
        // let reorg_height_int = reorg_height.0.saturating_sub(100);
        // reorg_height = Height(reorg_height_int);

        let mut reorg_hash = self.get_hash(reorg_height.0).unwrap_or(Hash([0u8; 32]));

        let mut check_hash = match self
            .fetcher
            .get_block(reorg_height.0.to_string(), Some(1))
            .await?
        {
            zaino_fetch::jsonrpsee::response::GetBlockResponse::Object(block) => block.hash.0,
            _ => {
                return Err(FinalisedStateError::Custom(
                    "Unexpected block response type".to_string(),
                ))
            }
        };

        // Find reorg height.
        //
        // Here this is the latest height at which the internal block hash matches the server block hash.
        while reorg_hash != check_hash {
            match reorg_height.previous() {
                Ok(height) => reorg_height = height,
                // Underflow error meaning reorg_height = start of chain.
                // This means the whole finalised state is old or corrupt.
                Err(_) => {
                    {
                        let mut txn = self.database.begin_rw_txn()?;
                        txn.clear_db(self.heights_to_hashes)?;
                        txn.clear_db(self.hashes_to_blocks)?;
                        txn.commit()?;
                    }
                    break;
                }
            };

            reorg_hash = self.get_hash(reorg_height.0).unwrap_or(Hash([0u8; 32]));

            check_hash = match self
                .fetcher
                .get_block(reorg_height.0.to_string(), Some(1))
                .await?
            {
                zaino_fetch::jsonrpsee::response::GetBlockResponse::Object(block) => block.hash.0,
                _ => {
                    return Err(FinalisedStateError::Custom(
                        "Unexpected block response type".to_string(),
                    ))
                }
            };
        }

        // Refill from max(reorg_height[+1], sapling_activation_height) to current server (finalised state) height.
        let mut sync_height = self
            .fetcher
            .get_blockchain_info()
            .await?
            .blocks
            .0
            .saturating_sub(99);
        for block_height in ((reorg_height.0 + 1).max(
            self.config
                .network
                .to_zebra_network()
                .sapling_activation_height()
                .0,
        ))..=sync_height
        {
            if self.get_hash(block_height).is_ok() {
                self.delete_block(Height(block_height))?;
            }
            loop {
                match fetch_block_from_node(
                    self.state.as_ref(),
                    Some(&network),
                    &self.fetcher,
                    HashOrHeight::Height(Height(block_height)),
                )
                .await
                {
                    Ok((hash, block)) => {
                        self.insert_block((Height(block_height), hash, block))?;
                        info!(
                            "Block at height {} successfully inserted in finalised state.",
                            block_height
                        );
                        break;
                    }
                    Err(e) => {
                        self.status.store(StatusType::RecoverableError);
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
            }
        }

        // Wait for server to sync to with p2p network and sync new blocks.
        if !self.config.network.to_zebra_network().is_regtest() {
            self.status.store(StatusType::Syncing);
            loop {
                let blockchain_info = self.fetcher.get_blockchain_info().await?;
                let server_height = blockchain_info.blocks.0;
                for block_height in (sync_height + 1)..(server_height - 99) {
                    if self.get_hash(block_height).is_ok() {
                        self.delete_block(Height(block_height))?;
                    }
                    loop {
                        match fetch_block_from_node(
                            self.state.as_ref(),
                            Some(&network),
                            &self.fetcher,
                            HashOrHeight::Height(Height(block_height)),
                        )
                        .await
                        {
                            Ok((hash, block)) => {
                                self.insert_block((Height(block_height), hash, block))?;
                                info!(
                                    "Block at height {} successfully inserted in finalised state.",
                                    block_height
                                );
                                break;
                            }
                            Err(e) => {
                                self.status.store(StatusType::RecoverableError);
                                warn!("{e}");
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            }
                        }
                    }
                }
                sync_height = server_height - 99;
                if (blockchain_info.blocks.0 as i64 - blockchain_info.estimated_height.0 as i64)
                    .abs()
                    <= 10
                {
                    break;
                } else {
                    info!(" - Validator syncing with network. ZainoDB chain height: {}, Validator chain height: {}, Estimated Network chain height: {}",
                            &sync_height,
                            &blockchain_info.blocks.0,
                            &blockchain_info.estimated_height.0
                        );
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    continue;
                }
            }
        }

        self.status.store(StatusType::Ready);

        Ok(())
    }

    /// Inserts a block into the finalised state.
    fn insert_block(&self, block: (Height, Hash, CompactBlock)) -> Result<(), FinalisedStateError> {
        let (height, hash, compact_block) = block;
        // let height_key = serde_json::to_vec(&DbHeight(height))?;
        let height_key = DbHeight(height).to_be_bytes();
        let hash_key = serde_json::to_vec(&DbHash(hash))?;
        let block_value = serde_json::to_vec(&DbCompactBlock(compact_block))?;

        let mut txn = self.database.begin_rw_txn()?;
        if let Err(database_err) = txn
            .put(
                self.heights_to_hashes,
                &height_key,
                &hash_key,
                lmdb::WriteFlags::NO_OVERWRITE,
            )
            .and_then(|()| {
                txn.put(
                    self.hashes_to_blocks,
                    &hash_key,
                    &block_value,
                    lmdb::WriteFlags::NO_OVERWRITE,
                )
            })
        {
            txn.abort();
            return Err(FinalisedStateError::LmdbError(database_err));
        }
        txn.commit()?;
        Ok(())
    }

    /// Deletes a block from the finalised state.
    fn delete_block(&self, height: Height) -> Result<(), FinalisedStateError> {
        let hash = self.get_hash(height.0)?;
        // let height_key = serde_json::to_vec(&DbHeight(height))?;
        let height_key = DbHeight(height).to_be_bytes();
        let hash_key = serde_json::to_vec(&DbHash(hash))?;

        let mut txn = self.database.begin_rw_txn()?;
        txn.del(self.heights_to_hashes, &height_key, None)?;
        txn.del(self.hashes_to_blocks, &hash_key, None)?;
        txn.commit()?;
        Ok(())
    }

    /// Retrieves a CompactBlock by Height or Hash.
    ///
    /// NOTE: It may be more efficient to implement a `get_block_range` method and batch database read calls.
    fn get_block(&self, height_or_hash: HashOrHeight) -> Result<CompactBlock, FinalisedStateError> {
        let txn = self.database.begin_ro_txn()?;

        let hash_key = match height_or_hash {
            HashOrHeight::Height(height) => {
                // let height_key = serde_json::to_vec(&DbHeight(height))?;
                let height_key = DbHeight(height).to_be_bytes();
                let hash_bytes: &[u8] = txn.get(self.heights_to_hashes, &height_key)?;
                hash_bytes.to_vec()
            }
            HashOrHeight::Hash(hash) => serde_json::to_vec(&DbHash(hash))?,
        };

        let block_bytes: &[u8] = txn.get(self.hashes_to_blocks, &hash_key)?;
        let block: DbCompactBlock = serde_json::from_slice(block_bytes)?;
        Ok(block.0)
    }

    /// Retrieves a Hash by Height.
    fn get_hash(&self, height: u32) -> Result<Hash, FinalisedStateError> {
        let txn = self.database.begin_ro_txn()?;

        // let height_key = serde_json::to_vec(&DbHeight(Height(height)))?;
        let height_key = DbHeight(Height(height)).to_be_bytes();

        let hash_bytes: &[u8] = match txn.get(self.heights_to_hashes, &height_key) {
            Ok(bytes) => bytes,
            Err(lmdb::Error::NotFound) => {
                return Err(FinalisedStateError::Custom(format!(
                    "No hash found for height {height}"
                )));
            }
            Err(e) => return Err(FinalisedStateError::LmdbError(e)),
        };

        let hash: Hash = serde_json::from_slice(hash_bytes)?;
        Ok(hash)
    }

    /// Fetches the highest stored height from LMDB.
    pub fn get_db_height(&self) -> Result<Height, FinalisedStateError> {
        let txn = self.database.begin_ro_txn()?;
        let mut cursor = txn.open_ro_cursor(self.heights_to_hashes)?;

        if let Some((height_bytes, _)) = cursor.iter().last() {
            // let height: DbHeight = serde_json::from_slice(height_bytes)?;
            let height = DbHeight::from_be_bytes(height_bytes)?;
            Ok(height.0)
        } else {
            Ok(Height(0))
        }
    }

    /// Returns a [`FinalisedStateSubscriber`].
    pub fn subscriber(&self) -> FinalisedStateSubscriber {
        FinalisedStateSubscriber {
            request_sender: self.request_sender.clone(),
            status: self.status.clone(),
        }
    }

    /// Returns the status of the finalised state.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Sets the finalised state to close gracefully.
    pub fn close(&mut self) {
        self.status.store(StatusType::Closing);
        if let Some(handle) = self.read_task_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.write_task_handle.take() {
            handle.abort();
        }

        if let Err(e) = self.database.sync(true) {
            error!("Error syncing LMDB before shutdown: {:?}", e);
        }
    }
}

impl Drop for FinalisedState {
    fn drop(&mut self) {
        self.status.store(StatusType::Closing);
        if let Some(handle) = self.read_task_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.write_task_handle.take() {
            handle.abort();
        }

        if let Err(e) = self.database.sync(true) {
            error!("Error syncing LMDB before shutdown: {:?}", e);
        }
    }
}

/// A subscriber to a [`crate::test_dependencies::chain_index::non_finalised_state::NonFinalizedState`].
#[derive(Debug, Clone)]
pub struct FinalisedStateSubscriber {
    request_sender: tokio::sync::mpsc::Sender<DbRequest>,
    status: AtomicStatus,
}

impl FinalisedStateSubscriber {
    /// Returns a Compact Block from the non-finalised state.
    pub async fn get_compact_block(
        &self,
        hash_or_height: HashOrHeight,
    ) -> Result<CompactBlock, FinalisedStateError> {
        let (channel_tx, channel_rx) = tokio::sync::oneshot::channel();
        if self
            .request_sender
            .send(DbRequest::new(hash_or_height, channel_tx))
            .await
            .is_err()
        {
            return Err(FinalisedStateError::Custom(
                "Error sending request to db reader".to_string(),
            ));
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(30), channel_rx).await;
        match result {
            Ok(Ok(compact_block)) => compact_block,
            Ok(Err(_)) => Err(FinalisedStateError::Custom(
                "Error receiving block from db reader".to_string(),
            )),
            Err(_) => Err(FinalisedStateError::Custom(
                "Timeout while waiting for compact block".to_string(),
            )),
        }
    }

    /// Returns the status of the FinalisedState..
    pub fn status(&self) -> StatusType {
        self.status.load()
    }
}
