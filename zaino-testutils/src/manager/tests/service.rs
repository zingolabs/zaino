//! Service test manager for validator + service factory tests.
//!
//! This manager provides validator operations and service creation factories,
//! designed for tests that need to create services manually with custom
//! configurations.

use std::net::SocketAddr;
use std::path::PathBuf;
use zaino_commons::config::Network;
use crate::{
    validator::{LocalNet, ValidatorKind},
    ports::TestPorts,
    config::{ServiceTestConfig, TestConfig},
    manager::{
        traits::{WithValidator, WithServiceFactories, ConfigurableBuilder, TestConfiguration},
        factories::{FetchServiceBuilder, StateServiceBuilder, BlockCacheBuilder},
    },
};
use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;

/// Test manager for service tests (validator + service factories).
///
/// This manager provides validator operations and service creation factories,
/// but does not run any indexer or provide lightclients. It's designed for
/// tests that need to create services manually.
#[derive(Debug)]
pub struct ServiceTestManager {
    local_net: LocalNet,
    ports: TestPorts,
    network: Network,
    chain_cache: Option<PathBuf>,
}

impl WithValidator for ServiceTestManager {
    fn validator_rpc_address(&self) -> SocketAddr {
        todo!("Return validator RPC address from ports")
    }
    
    fn validator_grpc_address(&self) -> SocketAddr {
        todo!("Return validator gRPC address from ports")
    }
    
    fn network(&self) -> &Network {
        &self.network
    }

    async fn generate_blocks(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement block generation using local_net")
    }

    async fn generate_blocks_with_delay(&self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement block generation with delays for sync")
    }

    async fn wait_for_validator_ready(&self) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Implement validator readiness check")
    }

    async fn close(&mut self) {
        todo!("Implement validator cleanup")
    }
}

impl WithServiceFactories for ServiceTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        todo!("Create pre-configured FetchServiceBuilder")
    }

    fn create_state_service(&self) -> StateServiceBuilder {
        todo!("Create pre-configured StateServiceBuilder")
    }

    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        todo!("Create authenticated JSON-RPC connector")
    }

    fn create_block_cache(&self) -> BlockCacheBuilder {
        todo!("Create pre-configured BlockCacheBuilder")
    }
}

/// Builder for ServiceTestManager.
#[derive(Debug, Clone)]
pub struct ServiceTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
}

impl Default for ServiceTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            chain_cache: None,
        }
    }
}

impl ConfigurableBuilder for ServiceTestsBuilder {
    type Manager = ServiceTestManager;
    type Config = ServiceTestConfig;

    fn build_config(&self) -> Self::Config {
        ServiceTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: self.chain_cache.clone(),
            },
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        todo!("Launch ServiceTestManager from builder configuration")
    }

    fn validator(mut self, kind: ValidatorKind) -> Self {
        self.validator_kind = kind;
        self
    }

    fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }

    fn chain_cache(mut self, path: PathBuf) -> Self {
        self.chain_cache = Some(path);
        self
    }
}