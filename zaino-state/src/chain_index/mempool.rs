//! Holds Zaino's mempool implementation.

use std::{collections::HashSet, sync::Arc};

use crate::{
    broadcast::{Broadcast, BroadcastSubscriber},
    chain_index::source::{BlockchainSource, BlockchainSourceError},
    error::{MempoolError, StatusError},
    status::{AtomicStatus, StatusType},
    BlockHash,
};
use tracing::{info, warn};
use zaino_fetch::jsonrpsee::response::GetMempoolInfoResponse;
use zebra_chain::{block::Hash, transaction::SerializedTransaction};

/// Mempool key
///
/// Holds txid.
///
/// TODO: Update to hold zebra_chain::Transaction::Hash ( or internal version )
#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct MempoolKey {
    /// currently txid (as string) - see above TODO, could be stronger type
    pub txid: String,
}

/// Mempool value.
///
/// Holds zebra_chain::transaction::SerializedTransaction.
#[derive(Debug, Clone, PartialEq)]
pub struct MempoolValue {
    /// Stores bytes that are guaranteed to be deserializable into a Transaction (zebra_chain enum).
    /// Sorts in lexicographic order of the transaction's serialized data.
    pub serialized_tx: Arc<SerializedTransaction>,
}

/// Zcash mempool, uses dashmap for efficient serving of mempool tx.
#[derive(Debug)]
pub struct Mempool<T: BlockchainSource> {
    /// Zcash chain fetch service.
    fetcher: T,
    /// Wrapper for a dashmap of mempool transactions.
    state: Broadcast<MempoolKey, MempoolValue>,
    /// The hash of the chain tip for which this mempool is currently serving.
    mempool_chain_tip: tokio::sync::watch::Sender<BlockHash>,
    /// Mempool sync handle.
    sync_task_handle: Option<std::sync::Mutex<tokio::task::JoinHandle<()>>>,
    /// mempool status.
    status: AtomicStatus,
}

impl<T: BlockchainSource> Mempool<T> {
    /// Spawns a new [`Mempool`].
    pub async fn spawn(
        fetcher: T,
        capacity_and_shard_amount: Option<(usize, usize)>,
    ) -> Result<Self, MempoolError> {
        // Wait for mempool in validator to come online.
        loop {
            match fetcher.get_mempool_txids().await {
                Ok(_) => {
                    break;
                }
                Err(_) => {
                    info!(" - Waiting for Validator mempool to come online..");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }

        let best_block_hash: BlockHash = match fetcher.get_best_block_hash().await {
            Ok(block_hash_opt) => match block_hash_opt {
                Some(hash) => hash.into(),
                None => {
                    return Err(MempoolError::Critical(
                        "Error in mempool: Error connecting with validator".to_string(),
                    ))
                }
            },
            Err(_e) => {
                return Err(MempoolError::Critical(
                    "Error in mempool: Error connecting with validator".to_string(),
                ))
            }
        };

        let (chain_tip_sender, _chain_tip_reciever) = tokio::sync::watch::channel(best_block_hash);

        info!("Launching Mempool..");
        let mut mempool = Mempool {
            fetcher: fetcher.clone(),
            state: match capacity_and_shard_amount {
                Some((capacity, shard_amount)) => {
                    Broadcast::new(Some(capacity), Some(shard_amount))
                }
                None => Broadcast::new(None, None),
            },
            mempool_chain_tip: chain_tip_sender,
            sync_task_handle: None,
            status: AtomicStatus::new(StatusType::Spawning),
        };

        loop {
            match mempool.get_mempool_transactions().await {
                Ok(mempool_transactions) => {
                    mempool.status.store(StatusType::Ready);
                    mempool
                        .state
                        .insert_filtered_set(mempool_transactions, mempool.status.load());
                    break;
                }
                Err(e) => {
                    mempool.state.notify(mempool.status.load());
                    warn!("{e}");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
            };
        }

        mempool.sync_task_handle = Some(std::sync::Mutex::new(mempool.serve().await?));

        Ok(mempool)
    }

    async fn serve(&self) -> Result<tokio::task::JoinHandle<()>, MempoolError> {
        let mempool = Self {
            fetcher: self.fetcher.clone(),
            state: self.state.clone(),
            mempool_chain_tip: self.mempool_chain_tip.clone(),
            sync_task_handle: None,
            status: self.status.clone(),
        };

        let state = self.state.clone();
        let status = self.status.clone();
        status.store(StatusType::Spawning);

        let sync_handle = tokio::spawn(async move {
            let mut best_block_hash: Hash;
            let mut check_block_hash: Hash;

            // Initialise tip.
            loop {
                match mempool.fetcher.get_best_block_hash().await {
                    Ok(block_hash_opt) => match block_hash_opt {
                        Some(hash) => {
                            mempool.mempool_chain_tip.send_replace(hash.into());
                            best_block_hash = hash;
                            break;
                        }
                        None => {
                            mempool.status.store(StatusType::RecoverableError);
                            state.notify(status.load());
                            warn!("error fetching best_block_hash from validator");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                    },
                    Err(e) => {
                        mempool.status.store(StatusType::RecoverableError);
                        state.notify(status.load());
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                }
            }

            // Main loop
            loop {
                // Check chain tip.
                match mempool.fetcher.get_best_block_hash().await {
                    Ok(block_hash_opt) => match block_hash_opt {
                        Some(hash) => {
                            check_block_hash = hash;
                        }
                        None => {
                            mempool.status.store(StatusType::RecoverableError);
                            state.notify(status.load());
                            warn!("error fetching best_block_hash from validator");
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                    },
                    Err(e) => {
                        state.notify(status.load());
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                }

                // If chain tip has changed reset mempool.
                if check_block_hash != best_block_hash {
                    status.store(StatusType::Syncing);
                    state.notify(status.load());
                    state.clear();

                    mempool
                        .mempool_chain_tip
                        .send_replace(check_block_hash.into());
                    best_block_hash = check_block_hash;

                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }

                match mempool.get_mempool_transactions().await {
                    Ok(mempool_transactions) => {
                        status.store(StatusType::Ready);
                        state.insert_filtered_set(mempool_transactions, status.load());
                    }
                    Err(e) => {
                        status.store(StatusType::RecoverableError);
                        state.notify(status.load());
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                };

                if status.load() == StatusType::Closing {
                    state.notify(status.load());
                    return;
                }

                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });

        Ok(sync_handle)
    }

    /// Returns all transactions in the mempool.
    async fn get_mempool_transactions(
        &self,
    ) -> Result<Vec<(MempoolKey, MempoolValue)>, MempoolError> {
        let mut transactions = Vec::new();

        let txids = self.fetcher.get_mempool_txids().await?.ok_or_else(|| {
            MempoolError::BlockchainSourceError(BlockchainSourceError::Unrecoverable(
                "could not fetch mempool data: mempool txid list was None".to_string(),
            ))
        })?;

        for txid in txids {
            let transaction = self
                .fetcher
                .get_transaction(txid.0.into())
                .await?
                .ok_or_else(|| {
                    MempoolError::BlockchainSourceError(
                        crate::chain_index::source::BlockchainSourceError::Unrecoverable(format!(
                            "could not fetch mempool data: transaction not found for txid {txid}"
                        )),
                    )
                })?;

            transactions.push((
                MempoolKey {
                    txid: txid.to_string(),
                },
                MempoolValue {
                    serialized_tx: Arc::new(transaction.into()),
                },
            ));
        }

        Ok(transactions)
    }

    /// Returns a [`MempoolSubscriber`].
    pub fn subscriber(&self) -> MempoolSubscriber {
        MempoolSubscriber {
            subscriber: self.state.subscriber(),
            seen_txids: HashSet::new(),
            mempool_chain_tip: self.mempool_chain_tip.subscribe(),
            status: self.status.clone(),
        }
    }

    /// Returns the current tx count
    pub async fn size(&self) -> Result<usize, MempoolError> {
        Ok(self
            .fetcher
            .get_mempool_txids()
            .await?
            .map_or(0, |v| v.len()))
    }

    /// Returns information about the mempool. Used by the `getmempoolinfo` RPC.
    /// Computed from local Broadcast state.
    pub async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, MempoolError> {
        let map = self.state.get_state();

        let size = map.len() as u64;

        let mut bytes: u64 = 0;
        let mut key_heap_bytes: u64 = 0;

        for entry in map.iter() {
            // payload bytes are exact (we store SerializedTransaction)
            bytes =
                bytes.saturating_add(Self::tx_serialized_len_bytes(&entry.value().serialized_tx));

            // heap used by the key txid (String)
            key_heap_bytes = key_heap_bytes.saturating_add(entry.key().txid.capacity() as u64);
        }

        let usage = bytes.saturating_add(key_heap_bytes);

        Ok(GetMempoolInfoResponse { size, bytes, usage })
    }

    #[inline]
    fn tx_serialized_len_bytes(tx: &SerializedTransaction) -> u64 {
        tx.as_ref().len() as u64
    }

    // TODO knock this out if possible
    // private fields in remaining references
    //
    /// Returns the status of the mempool.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Sets the mempool to close gracefully.
    pub fn close(&self) {
        self.status.store(StatusType::Closing);
        self.state.notify(self.status.load());
        if let Some(ref handle) = self.sync_task_handle {
            if let Ok(handle) = handle.lock() {
                handle.abort();
            }
        }
    }
}

impl<T: BlockchainSource> Drop for Mempool<T> {
    fn drop(&mut self) {
        self.status.store(StatusType::Closing);
        self.state.notify(StatusType::Closing);
        if let Some(handle) = self.sync_task_handle.take() {
            if let Ok(handle) = handle.lock() {
                handle.abort();
            }
        }
    }
}

/// A subscriber to a [`Mempool`].
#[derive(Debug, Clone)]
pub struct MempoolSubscriber {
    subscriber: BroadcastSubscriber<MempoolKey, MempoolValue>,
    seen_txids: HashSet<MempoolKey>,
    mempool_chain_tip: tokio::sync::watch::Receiver<BlockHash>,
    status: AtomicStatus,
}

impl MempoolSubscriber {
    /// Returns all tx currently in the mempool.
    pub async fn get_mempool(&self) -> Vec<(MempoolKey, MempoolValue)> {
        self.subscriber.get_filtered_state(&HashSet::new())
    }

    /// Returns all tx currently in the mempool filtered by `exclude_list`.
    ///
    /// The transaction IDs in the Exclude list can be shortened to any number of bytes to make the request
    /// more bandwidth-efficient; if two or more transactions in the mempool
    /// match a shortened txid, they are all sent (none is excluded). Transactions
    /// in the exclude list that don't exist in the mempool are ignored.
    pub async fn get_filtered_mempool(
        &self,
        exclude_list: Vec<String>,
    ) -> Vec<(MempoolKey, MempoolValue)> {
        let mempool_tx = self.subscriber.get_filtered_state(&HashSet::new());

        let mempool_txids: HashSet<String> = mempool_tx
            .iter()
            .map(|(mempool_key, _)| mempool_key.txid.clone())
            .collect();

        let mut txids_to_exclude: HashSet<MempoolKey> = HashSet::new();
        for exclude_txid in &exclude_list {
            // Convert to big endian (server format).
            let server_exclude_txid: String = exclude_txid
                .chars()
                .collect::<Vec<_>>()
                .chunks(2)
                .rev()
                .map(|chunk| chunk.iter().collect::<String>())
                .collect();
            let matching_txids: Vec<&String> = mempool_txids
                .iter()
                .filter(|txid| txid.starts_with(&server_exclude_txid))
                .collect();

            if matching_txids.len() == 1 {
                txids_to_exclude.insert(MempoolKey {
                    txid: matching_txids[0].clone(),
                });
            }
        }

        mempool_tx
            .into_iter()
            .filter(|(mempool_key, _)| !txids_to_exclude.contains(mempool_key))
            .collect()
    }

    /// Returns a stream of mempool txids, closes the channel when a new block has been mined.
    pub async fn get_mempool_stream(
        &mut self,
        expected_chain_tip: Option<BlockHash>,
    ) -> Result<
        (
            tokio::sync::mpsc::Receiver<Result<(MempoolKey, MempoolValue), StatusError>>,
            tokio::task::JoinHandle<()>,
        ),
        MempoolError,
    > {
        let mut subscriber = self.clone();
        subscriber.seen_txids.clear();
        let (channel_tx, channel_rx) = tokio::sync::mpsc::channel(32);

        if let Some(expected_chain_tip_hash) = expected_chain_tip {
            if expected_chain_tip_hash != *self.mempool_chain_tip.borrow() {
                return Err(MempoolError::IncorrectChainTip {
                    expected_chain_tip: expected_chain_tip_hash,
                    current_chain_tip: *self.mempool_chain_tip.borrow(),
                });
            }
        }

        let streamer_handle = tokio::spawn(async move {
            let mempool_result: Result<(), MempoolError> = async {
                loop {
                    let (mempool_status, mempool_updates) = subscriber
                        .wait_on_mempool_updates(expected_chain_tip)
                        .await?;
                    match mempool_status {
                        StatusType::Ready => {
                            for (mempool_key, mempool_value) in mempool_updates {
                                loop {
                                    match channel_tx
                                        .try_send(Ok((mempool_key.clone(), mempool_value.clone())))
                                    {
                                        Ok(_) => break,
                                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                            tokio::time::sleep(std::time::Duration::from_millis(
                                                100,
                                            ))
                                            .await;
                                            continue;
                                        }
                                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                            return Ok(());
                                        }
                                    }
                                }
                            }
                        }
                        StatusType::Syncing => {
                            return Ok(());
                        }
                        StatusType::Closing => {
                            return Err(MempoolError::StatusError(StatusError {
                                server_status: StatusType::Closing,
                            }));
                        }
                        StatusType::RecoverableError => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                        status => {
                            return Err(MempoolError::StatusError(StatusError {
                                server_status: status,
                            }));
                        }
                    }
                    if subscriber.status.load() == StatusType::Closing {
                        return Err(MempoolError::StatusError(StatusError {
                            server_status: StatusType::Closing,
                        }));
                    }
                }
            }
            .await;

            if let Err(mempool_error) = mempool_result {
                warn!("Error in mempool stream: {:?}", mempool_error);
                match mempool_error {
                    MempoolError::StatusError(error_status) => {
                        let _ = channel_tx.send(Err(error_status)).await;
                    }
                    _ => {
                        let _ = channel_tx
                            .send(Err(StatusError {
                                server_status: StatusType::RecoverableError,
                            }))
                            .await;
                    }
                }
            }
        });

        Ok((channel_rx, streamer_handle))
    }

    /// Returns true if mempool contains the given txid.
    pub async fn contains_txid(&self, txid: &MempoolKey) -> bool {
        self.subscriber.contains_key(txid)
    }

    /// Returns transaction by txid if in the mempool, else returns none.
    pub async fn get_transaction(&self, txid: &MempoolKey) -> Option<Arc<MempoolValue>> {
        self.subscriber.get(txid)
    }

    /// Returns information about the mempool. Used by the `getmempoolinfo` RPC.
    /// Computed from local Broadcast state.
    pub async fn get_mempool_info(&self) -> Result<GetMempoolInfoResponse, MempoolError> {
        let mempool_transactions: Vec<(MempoolKey, MempoolValue)> =
            self.subscriber.get_filtered_state(&HashSet::new());

        let size: u64 = mempool_transactions.len() as u64;

        let mut bytes: u64 = 0;
        let mut key_heap_bytes: u64 = 0;

        for (mempool_key, mempool_value) in mempool_transactions.iter() {
            // payload bytes are exact (we store SerializedTransaction)
            bytes =
                bytes.saturating_add(mempool_value.serialized_tx.as_ref().as_ref().len() as u64);

            // heap used by the key String (txid)
            key_heap_bytes = key_heap_bytes.saturating_add(mempool_key.txid.capacity() as u64);
        }

        let usage: u64 = bytes.saturating_add(key_heap_bytes);

        Ok(GetMempoolInfoResponse { size, bytes, usage })
    }

    // TODO noted here too
    /// Returns the status of the mempool.
    pub fn status(&self) -> StatusType {
        self.status.load()
    }

    /// Returns all tx currently in the mempool and updates seen_txids.
    fn get_mempool_and_update_seen(&mut self) -> Vec<(MempoolKey, MempoolValue)> {
        let mempool_updates = self.subscriber.get_filtered_state(&HashSet::new());
        for (mempool_key, _) in mempool_updates.clone() {
            self.seen_txids.insert(mempool_key);
        }
        mempool_updates
    }

    /// Returns txids not yet seen by the subscriber and updates seen_txids.
    fn get_mempool_updates_and_update_seen(&mut self) -> Vec<(MempoolKey, MempoolValue)> {
        let mempool_updates = self.subscriber.get_filtered_state(&self.seen_txids);
        for (mempool_key, _) in mempool_updates.clone() {
            self.seen_txids.insert(mempool_key);
        }
        mempool_updates
    }

    /// Waits on update from mempool and updates the mempool, returning either the new mempool or the mempool updates, along with the mempool status.
    async fn wait_on_mempool_updates(
        &mut self,
        expected_chain_tip: Option<BlockHash>,
    ) -> Result<(StatusType, Vec<(MempoolKey, MempoolValue)>), MempoolError> {
        if expected_chain_tip.is_some()
            && expected_chain_tip.unwrap() != *self.mempool_chain_tip.borrow()
        {
            self.clear_seen();
            return Ok((StatusType::Syncing, self.get_mempool_and_update_seen()));
        }

        let update_status = self.subscriber.wait_on_notifier().await?;
        match update_status {
            StatusType::Ready => Ok((
                StatusType::Ready,
                self.get_mempool_updates_and_update_seen(),
            )),
            StatusType::Syncing => {
                self.clear_seen();
                Ok((StatusType::Syncing, self.get_mempool_and_update_seen()))
            }
            StatusType::Closing => Ok((StatusType::Closing, Vec::new())),
            status => Err(MempoolError::StatusError(StatusError {
                server_status: status,
            })),
        }
    }

    /// Clears the subscribers seen_txids.
    fn clear_seen(&mut self) {
        self.seen_txids.clear();
    }

    /// Get the chain tip that the mempool is atop
    pub fn mempool_chain_tip(&self) -> BlockHash {
        *self.mempool_chain_tip.borrow()
    }
}
