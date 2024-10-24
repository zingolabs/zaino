//! Zingo-Indexer mempool state functionality.

use std::{collections::HashSet, time::SystemTime};
use tokio::sync::{Mutex, RwLock};

use crate::{chain::error::MempoolError, jsonrpc::connector::JsonRpcConnector};

/// Mempool state information.
pub struct Mempool {
    /// Txids currently in the mempool.
    txids: RwLock<Vec<String>>,
    /// Txids that have already been added to Zingo-Indexer's mempool.
    txids_seen: Mutex<HashSet<String>>,
    /// System time when the mempool was last updated.
    last_sync_time: Mutex<SystemTime>,
    /// Blockchain data, used to check when a new block has been mined.
    best_block_hash: RwLock<Option<zebra_chain::block::Hash>>,
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new()
    }
}

impl Mempool {
    /// Returns an empty mempool.
    pub fn new() -> Self {
        Mempool {
            txids: RwLock::new(Vec::new()),
            txids_seen: Mutex::new(HashSet::new()),
            last_sync_time: Mutex::new(SystemTime::now()),
            best_block_hash: RwLock::new(None),
        }
    }

    /// Updates the mempool, returns true if the current block in the mempool has been mined.
    pub async fn update(&self, zebrad_uri: &http::Uri) -> Result<bool, MempoolError> {
        self.update_last_sync_time().await?;
        let mined = self.check_and_update_best_block_hash(zebrad_uri).await?;
        if mined {
            self.reset_txids().await?;
            self.update_txids(zebrad_uri).await?;
            Ok(true)
        } else {
            self.update_txids(zebrad_uri).await?;
            Ok(false)
        }
    }

    /// Updates the txids in the mempool.
    async fn update_txids(&self, zebrad_uri: &http::Uri) -> Result<(), MempoolError> {
        let node_txids = JsonRpcConnector::new(
            zebrad_uri.clone(),
            Some("xxxxxx".to_string()),
            Some("xxxxxx".to_string()),
        )
        .await?
        .get_raw_mempool()
        .await?
        .transactions;
        let mut txids_seen = self.txids_seen.lock().await;
        let mut txids = self.txids.write().await;
        for txid in node_txids {
            if !txids_seen.contains(&txid) {
                txids.push(txid.clone());
            }
            txids_seen.insert(txid);
        }
        Ok(())
    }

    /// Updates the system last sync time.
    async fn update_last_sync_time(&self) -> Result<(), MempoolError> {
        let mut last_sync_time = self.last_sync_time.lock().await;
        *last_sync_time = SystemTime::now();
        Ok(())
    }

    /// Updates the mempool blockchain info, returns true if the current block in the mempool has been mined.
    async fn check_and_update_best_block_hash(
        &self,
        zebrad_uri: &http::Uri,
    ) -> Result<bool, MempoolError> {
        let node_best_block_hash = JsonRpcConnector::new(
            zebrad_uri.clone(),
            Some("xxxxxx".to_string()),
            Some("xxxxxx".to_string()),
        )
        .await?
        .get_blockchain_info()
        .await?
        .best_block_hash;

        let mut last_best_block_hash = self.best_block_hash.write().await;

        if let Some(ref last_hash) = *last_best_block_hash {
            if node_best_block_hash == *last_hash {
                return Ok(false);
            }
        }

        *last_best_block_hash = Some(node_best_block_hash);
        Ok(true)
    }

    /// Clears the txids currently held in the mempool.
    async fn reset_txids(&self) -> Result<(), MempoolError> {
        let mut txids = self.txids.write().await;
        txids.clear();
        let mut txids_seen = self.txids_seen.lock().await;
        txids_seen.clear();
        Ok(())
    }

    /// Returns the txids currently in the mempool.
    pub async fn get_mempool_txids(&self) -> Result<Vec<String>, MempoolError> {
        let txids = self.txids.read().await;
        Ok(txids.clone())
    }

    /// Returns the hash of the block currently in the mempool.
    pub async fn get_best_block_hash(
        &self,
    ) -> Result<Option<zebra_chain::block::Hash>, MempoolError> {
        let best_block_hash = self.best_block_hash.read().await;
        Ok(*best_block_hash)
    }
}
