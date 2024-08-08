//! Zingo-Indexer config.

use crate::error::IndexerError;

/// Config information required for Zingo-Indexer.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub tcp_active: bool,
    pub listen_port: Option<u16>,
    pub nym_active: bool,
    pub nym_conf_path: Option<String>,
    pub lightwalletd_port: u16,
    pub zebrad_port: u16,
    pub max_queue_size: u16,
    pub max_worker_pool_size: u16,
    pub idle_worker_pool_size: u16,
}

impl IndexerConfig {
    pub fn check_config(&self) -> Result<(), IndexerError> {
        if !(self.tcp_active && self.nym_active) {
            return Err(IndexerError::ConfigError(
                "Cannot start server with no ingestors selected, at least one of either nym or tcp must be set to active in conf.".to_string(),
            ));
        }
        if self.tcp_active && self.listen_port.is_none() {
            return Err(IndexerError::ConfigError(
                "TCP is active but no address provided.".to_string(),
            ));
        }
        if self.nym_active && self.nym_conf_path.is_none() {
            return Err(IndexerError::ConfigError(
                "NYM is active but no conf path provided.".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            tcp_active: true,
            listen_port: Some(8080),
            nym_active: true,
            nym_conf_path: Some("/tmp/nym_server".to_string()),
            lightwalletd_port: 9067,
            zebrad_port: 18232,
            max_queue_size: 100,
            max_worker_pool_size: 16,
            idle_worker_pool_size: 2,
        }
    }
}
