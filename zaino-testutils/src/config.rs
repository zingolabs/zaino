//! Purpose-built configuration system for specialized test managers.
//!
//! This module provides configuration types designed specifically for the new
//! trait-based test manager architecture, replacing the monolithic TestConfigBuilder
//! with purpose-built configs for each test scenario.

use std::path::PathBuf;
use zaino_commons::config::{IndexerConfig, Network};
use crate::validator::ValidatorKind;
use crate::manager::traits::{LaunchManager, TestConfiguration};

/// Base configuration shared by all specialized test managers.
#[derive(Debug, Clone)]
pub struct TestConfig {
    /// Network type (Regtest, Testnet, Mainnet).
    pub network: Network,
    /// Validator type (Zebra or Zcashd).
    pub validator_kind: ValidatorKind,
    /// Optional chain cache directory for faster startup.
    pub chain_cache: Option<PathBuf>,
}

impl TestConfiguration for TestConfig {
    fn network(&self) -> &Network {
        &self.network
    }
    
    fn validator_kind(&self) -> ValidatorKind {
        self.validator_kind
    }
}

/// Configuration for service tests (validator + service factories only).
#[derive(Debug, Clone)]
pub struct ServiceTestConfig {
    /// Base configuration.
    pub base: TestConfig,
}

impl TestConfiguration for ServiceTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }
    
    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl LaunchManager<crate::manager::tests::service::ServiceTestManager> for ServiceTestConfig {
    async fn launch_manager(self) -> Result<crate::manager::tests::service::ServiceTestManager, Box<dyn std::error::Error>> {
        todo!("Launch ServiceTestManager from ServiceTestConfig")
    }
}

/// Configuration for wallet tests (validator + indexer + clients).
#[derive(Debug, Clone)]
pub struct WalletTestConfig {
    /// Base configuration.
    pub base: TestConfig,
    /// Indexer configuration.
    pub indexer: IndexerConfig,
    /// Whether to enable lightclients (usually true for wallet tests).
    pub enable_clients: bool,
}

impl TestConfiguration for WalletTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }
    
    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl LaunchManager<crate::manager::tests::wallet::WalletTestManager> for WalletTestConfig {
    async fn launch_manager(self) -> Result<crate::manager::tests::wallet::WalletTestManager, Box<dyn std::error::Error>> {
        todo!("Launch WalletTestManager from WalletTestConfig")
    }
}

/// JSON-RPC authentication configuration.
#[derive(Debug, Clone)]
pub enum JsonRpcAuthConfig {
    /// No authentication.
    None,
    /// Cookie-based authentication.
    Cookie(PathBuf),
    /// Password-based authentication.
    Password { username: String, password: String },
}

/// Configuration for JSON server tests (validator + indexer + JSON server).
#[derive(Debug, Clone)]
pub struct JsonServerTestConfig {
    /// Base configuration.
    pub base: TestConfig,
    /// Indexer configuration.
    pub indexer: IndexerConfig,
    /// JSON-RPC authentication configuration.
    pub json_auth: JsonRpcAuthConfig,
    /// Whether to enable lightclients (optional for JSON server tests).
    pub enable_clients: bool,
}

impl TestConfiguration for JsonServerTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }
    
    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl LaunchManager<crate::manager::tests::json_server::JsonServerTestManager> for JsonServerTestConfig {
    async fn launch_manager(self) -> Result<crate::manager::tests::json_server::JsonServerTestManager, Box<dyn std::error::Error>> {
        todo!("Launch JsonServerTestManager from JsonServerTestConfig")
    }
}