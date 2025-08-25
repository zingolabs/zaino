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
        traits::{WithValidator, WithServiceFactories, ConfigurableBuilder, LaunchManager},
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
    fn local_net(&self) -> &LocalNet {
        &self.local_net
    }

    fn local_net_mut(&mut self) -> &mut LocalNet {
        &mut self.local_net
    }

    fn validator_rpc_address(&self) -> SocketAddr {
        self.ports.validator_rpc
    }
    
    fn validator_grpc_address(&self) -> SocketAddr {
        self.ports.validator_grpc
    }
    
    fn network(&self) -> &Network {
        &self.network
    }
}

impl WithServiceFactories for ServiceTestManager {
    fn create_fetch_service(&self) -> FetchServiceBuilder {
        FetchServiceBuilder::new(
            self.validator_rpc_address(),
            self.network.clone(),
            self.ports.zaino_db.clone()
        )
    }

    fn create_state_service(&self) -> StateServiceBuilder {
        StateServiceBuilder::new(
            self.validator_rpc_address(),
            self.validator_grpc_address(),
            self.network.clone(),
            self.ports.zaino_db.clone()
        )
    }

    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>> {
        use zaino_fetch::jsonrpsee::connector::JsonRpSeeConnector;
        
        let url = format!("http://{}", self.validator_rpc_address()).parse()?;
        let connector = JsonRpSeeConnector::new(url, None)?; // No auth for test validators
        Ok(connector)
    }

    fn create_block_cache(&self) -> BlockCacheBuilder {
        // Create a basic connector for the cache (we need it for initialization)
        let connector = self.create_json_connector()
            .expect("Failed to create connector for block cache");
        
        BlockCacheBuilder::new(
            connector,
            self.network.clone(),
            self.ports.zaino_db.clone()
        )
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
        let config = self.build_config();
        config.launch_manager().await
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