//! Holds Zaino's mempool implementation.

use std::collections::HashSet;

use crate::{
    broadcast::{Broadcast, BroadcastSubscriber},
    error::{MempoolError, StatusError},
    status::{AtomicStatus, StatusType},
};
use tracing::{info, warn};
use zaino_fetch::jsonrpc::connector::JsonRpcConnector;
use zebra_chain::block::Hash;
use zebra_rpc::methods::GetRawTransaction;

/// Mempool key
///
/// Holds txid.
#[derive(Debug, Clone, PartialEq, Hash, Eq)]
pub struct MempoolKey(pub String);

/// Mempool value.
///
/// NOTE: Currently holds a copy of txid,
///       this could be updated to store the corresponding transaction as the value,
///       this would enable the serving of mempool transactions directly, significantly increasing efficiency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MempoolValue(pub GetRawTransaction);

/// Zcash mempool, uses dashmap for efficient serving of mempool tx.
#[derive(Debug)]
pub struct Mempool {
    /// Zcash chain fetch service.
    fetcher: JsonRpcConnector,
    /// Wrapper for a dashmap of mempool transactions.
    state: Broadcast<MempoolKey, MempoolValue>,
    /// Mempool sync handle.
    sync_task_handle: Option<tokio::task::JoinHandle<()>>,
    /// mempool status.
    status: AtomicStatus,
}

impl Mempool {
    /// Spawns a new [`Mempool`].
    pub async fn spawn(
        fetcher: &JsonRpcConnector,
        capacity_and_shard_amount: Option<(usize, usize)>,
    ) -> Result<Self, MempoolError> {
        // Wait for mempool in validator to come online.
        loop {
            match fetcher.get_raw_mempool().await {
                Ok(_) => {
                    break;
                }
                Err(_) => {
                    info!(" - Waiting for Validator mempool to come online..");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }

        info!("Launching Mempool..");
        let mut mempool = Mempool {
            fetcher: fetcher.clone(),
            state: match capacity_and_shard_amount {
                Some((capacity, shard_amount)) => {
                    Broadcast::new(Some(capacity), Some(shard_amount))
                }
                None => Broadcast::new(None, None),
            },
            sync_task_handle: None,
            status: AtomicStatus::new(StatusType::Spawning.into()),
        };

        loop {
            match mempool.get_mempool_transactions().await {
                Ok(mempool_transactions) => {
                    mempool.status.store(StatusType::Ready.into());
                    mempool
                        .state
                        .insert_filtered_set(mempool_transactions, mempool.status.clone().into());
                    break;
                }
                Err(e) => {
                    mempool.state.notify(mempool.status.clone().into());
                    warn!("{e}");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
            };
        }

        mempool.sync_task_handle = Some(mempool.serve().await?);

        Ok(mempool)
    }

    async fn serve(&self) -> Result<tokio::task::JoinHandle<()>, MempoolError> {
        let mempool = Self {
            fetcher: self.fetcher.clone(),
            state: self.state.clone(),
            sync_task_handle: None,
            status: self.status.clone(),
        };

        let state = self.state.clone();
        let status = self.status.clone();
        status.store(StatusType::Spawning.into());

        let sync_handle = tokio::spawn(async move {
            let mut best_block_hash: Hash;
            let mut check_block_hash: Hash;

            loop {
                match mempool.fetcher.get_blockchain_info().await {
                    Ok(chain_info) => {
                        best_block_hash = chain_info.best_block_hash;
                        break;
                    }
                    Err(e) => {
                        state.notify(status.clone().into());
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                }
            }

            loop {
                match mempool.fetcher.get_blockchain_info().await {
                    Ok(chain_info) => {
                        check_block_hash = chain_info.best_block_hash;
                    }
                    Err(e) => {
                        status.store(StatusType::RecoverableError.into());
                        state.notify(status.clone().into());
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                }

                if check_block_hash != best_block_hash {
                    best_block_hash = check_block_hash;
                    status.store(StatusType::Syncing.into());
                    state.notify(status.clone().into());
                    state.clear();
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }

                match mempool.get_mempool_transactions().await {
                    Ok(mempool_transactions) => {
                        status.store(StatusType::Ready.into());
                        state.insert_filtered_set(mempool_transactions, status.clone().into());
                    }
                    Err(e) => {
                        status.store(StatusType::RecoverableError.into());
                        state.notify(status.clone().into());
                        warn!("{e}");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                };

                if status.load() == StatusType::Closing as usize {
                    state.notify(status.into());
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

        for txid in self.fetcher.get_raw_mempool().await?.transactions {
            let transaction = self
                .fetcher
                .get_raw_transaction(txid.clone(), Some(1))
                .await?;
            transactions.push((MempoolKey(txid), MempoolValue(transaction.into())));
        }

        Ok(transactions)
    }

    /// Returns a [`MempoolSubscriber`].
    pub fn subscriber(&self) -> MempoolSubscriber {
        MempoolSubscriber {
            subscriber: self.state.subscriber(),
            seen_txids: HashSet::new(),
            status: self.status.clone(),
        }
    }

    /// Returns the status of the mempool.
    pub fn status(&self) -> StatusType {
        self.status.load().into()
    }

    /// Sets the mempool to close gracefully.
    pub fn close(&mut self) {
        self.status.store(StatusType::Closing.into());
        self.state.notify(self.status());
        if let Some(handle) = self.sync_task_handle.take() {
            handle.abort();
        }
    }
}

impl Drop for Mempool {
    fn drop(&mut self) {
        self.status.store(StatusType::Closing.into());
        self.state.notify(StatusType::Closing);
        if let Some(handle) = self.sync_task_handle.take() {
            handle.abort();
        }
    }
}

/// A subscriber to a [`Mempool`].
#[derive(Debug, Clone)]
pub struct MempoolSubscriber {
    subscriber: BroadcastSubscriber<MempoolKey, MempoolValue>,
    seen_txids: HashSet<MempoolKey>,
    status: AtomicStatus,
}

impl MempoolSubscriber {
    /// Returns all tx currently in the mempool.
    pub async fn get_mempool(&self) -> Vec<(MempoolKey, MempoolValue)> {
        self.subscriber.get_filtered_state(&HashSet::new())
    }

    /// Returns all tx currently in the mempool filtered by [`exclude_list`].
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
            .map(|(mempool_key, _)| mempool_key.0.clone())
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
                txids_to_exclude.insert(MempoolKey(matching_txids[0].clone()));
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

        let streamer_handle = tokio::spawn(async move {
            let mempool_result: Result<(), MempoolError> = async {
                loop {
                    let (mempool_status, mempool_updates) = subscriber.wait_on_update().await?;
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
                            return Err(MempoolError::StatusError(StatusError(
                                StatusType::Closing,
                            )));
                        }
                        StatusType::RecoverableError => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            continue;
                        }
                        status => {
                            return Err(MempoolError::StatusError(StatusError(status)));
                        }
                    }
                    if subscriber.status.load() == StatusType::Closing as usize {
                        return Err(MempoolError::StatusError(StatusError(StatusType::Closing)));
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
                            .send(Err(StatusError(StatusType::RecoverableError)))
                            .await;
                    }
                }
            }
        });

        Ok((channel_rx, streamer_handle))
    }

    /// Returns the status of the mempool.
    pub fn status(&self) -> StatusType {
        self.status.load().into()
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
    async fn wait_on_update(
        &mut self,
    ) -> Result<(StatusType, Vec<(MempoolKey, MempoolValue)>), MempoolError> {
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
            status => Err(MempoolError::StatusError(StatusError(status))),
        }
    }

    /// Clears the subscribers seen_txids.
    fn clear_seen(&mut self) {
        self.seen_txids.clear();
    }
}
