//! Zingo-Indexer config.

use std::path::Path;

use crate::error::IndexerError;

/// Config information required for Zingo-Indexer.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Sets the TcpIngestor's status.
    pub tcp_active: bool,
    /// TcpIngestors listen port
    pub listen_port: Option<u16>,
    /// Sets the NymIngestor's and NymDispatchers status.
    pub nym_active: bool,
    /// Nym conf path used for micnet client conf.
    pub nym_conf_path: Option<String>,
    /// LightWalletD listen port [DEPRECATED].
    /// Used by nym_poc and zingo-testutils.
    pub lightwalletd_port: u16,
    /// Full node / validator listen port.
    pub zebrad_port: u16,
    /// Full node Username.
    pub node_user: Option<String>,
    /// full node Password.
    pub node_password: Option<String>,
    /// Maximum requests allowed in the request queue.
    pub max_queue_size: u16,
    /// Maximum workers allowed in the worker pool
    pub max_worker_pool_size: u16,
    /// Minimum number of workers held in the workerpool when idle.
    pub idle_worker_pool_size: u16,
}

impl IndexerConfig {
    /// Performs checks on config data.
    ///
    /// - Checks that at least 1 of nym or tpc is active.
    /// - Checks listen port is given is tcp is active.
    /// - Checks nym_conf_path is given if nym is active and holds a valid utf8 string.
    pub fn check_config(&self) -> Result<(), IndexerError> {
        if (!self.tcp_active) && (!self.nym_active) {
            return Err(IndexerError::ConfigError(
                "Cannot start server with no ingestors selected, at least one of either nym or tcp must be set to active in conf.".to_string(),
            ));
        }
        if self.tcp_active && self.listen_port.is_none() {
            return Err(IndexerError::ConfigError(
                "TCP is active but no address provided.".to_string(),
            ));
        }
        if let Some(path_str) = self.nym_conf_path.clone() {
            if Path::new(&path_str).to_str().is_none() {
                return Err(IndexerError::ConfigError(
                    "Invalid nym conf path syntax or non-UTF-8 characters in path.".to_string(),
                ));
            }
        } else {
            if self.nym_active {
                return Err(IndexerError::ConfigError(
                    "NYM is active but no conf path provided.".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(not(feature = "nym_poc"))]
impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            tcp_active: true,
            listen_port: Some(8080),
            nym_active: true,
            nym_conf_path: Some("/tmp/indexer/nym".to_string()),
            lightwalletd_port: 9067,
            zebrad_port: 18232,
            node_user: Some("xxxxxx".to_string()),
            node_password: Some("xxxxxx".to_string()),
            max_queue_size: 1024,
            max_worker_pool_size: 32,
            idle_worker_pool_size: 4,
        }
    }
}

#[cfg(feature = "nym_poc")]
impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            tcp_active: true,
            listen_port: Some(8088),
            nym_active: false,
            nym_conf_path: None,
            lightwalletd_port: 8080,
            zebrad_port: 18232,
            node_user: Some("xxxxxx".to_string()),
            node_password: Some("xxxxxx".to_string()),
            max_queue_size: 1024,
            max_worker_pool_size: 32,
            idle_worker_pool_size: 4,
        }
    }
}
