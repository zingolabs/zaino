//! Purpose-built configuration system for specialized test managers.
//!
//! This module provides configuration types designed specifically for the new
//! trait-based test manager architecture, replacing the monolithic TestConfigBuilder
//! with purpose-built configs for each test scenario.

use crate::manager::traits::{LaunchManager, TestConfiguration};
use crate::validator::ValidatorKind;
use std::path::PathBuf;
use zaino_commons::config::Network;
use zainodlib::config::IndexerConfig;

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

/// Configuration for fetch service tests (validator + FetchService + optional clients).
#[derive(Debug, Clone)]
pub struct FetchServiceTestConfig {
    /// Base configuration.
    pub base: TestConfig,
    /// Whether to enable wallet clients.
    pub with_clients: bool,
}

/// Configuration for state service comparison tests (validator + dual services + optional clients).
#[derive(Debug, Clone)]
pub struct StateServiceComparisonTestConfig {
    /// Base configuration.
    pub base: TestConfig,
    /// Whether to enable wallet clients.
    pub with_clients: bool,
}

/// Configuration for JSON server comparison tests (zcashd + zaino JSON server + dual FetchServices + optional clients).
#[derive(Debug, Clone)]
pub struct JsonServerComparisonTestConfig {
    /// Base configuration.
    pub base: TestConfig,
    /// Whether to enable cookie authentication.
    pub enable_cookie_auth: bool,
    /// Whether to enable wallet clients.
    pub with_clients: bool,
}

/// Configuration for chain cache tests (validator + JsonRpSeeConnector + chain caching + optional clients).
#[derive(Debug, Clone)]
pub struct ChainCacheTestConfig {
    /// Base configuration.
    pub base: TestConfig,
    /// Whether to enable wallet clients.
    pub with_clients: bool,
}

/// Configuration for local cache tests (validator + JsonRpSeeConnector + BlockCache).
#[derive(Debug, Clone)]
pub struct LocalCacheTestConfig {
    /// Base configuration.
    pub base: TestConfig,
}

/// Configuration for test vector generation tests (validator + StateService + clients).
#[derive(Debug, Clone)]
pub struct TestVectorTestConfig {
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

impl TestConfiguration for FetchServiceTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl TestConfiguration for StateServiceComparisonTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl TestConfiguration for JsonServerComparisonTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl TestConfiguration for ChainCacheTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl TestConfiguration for LocalCacheTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl TestConfiguration for TestVectorTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl LaunchManager<crate::manager::tests::service::ServiceTestManager> for ServiceTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::service::ServiceTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig, ValidatorKind};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration based on kind
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        Ok(crate::manager::tests::service::ServiceTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
        })
    }
}

impl LaunchManager<crate::manager::tests::fetch_service::FetchServiceTestManager> for FetchServiceTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::fetch_service::FetchServiceTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let _infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration based on kind
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: _infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Initialize clients if requested
        let clients = if self.with_clients {
            // For now, use a default port - this will need to be configured properly
            Some(crate::clients::Clients::launch(8232).await?)
        } else {
            None
        };

        Ok(crate::manager::tests::fetch_service::FetchServiceTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
            clients,
        })
    }
}

impl LaunchManager<crate::manager::tests::state_service_comparison::StateServiceComparisonTestManager> for StateServiceComparisonTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::state_service_comparison::StateServiceComparisonTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig, ValidatorKind};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration based on kind
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Initialize clients if requested
        let clients = if self.with_clients {
            // For now, use a default port - this will need to be configured properly
            Some(crate::clients::Clients::launch(8232).await?)
        } else {
            None
        };

        Ok(crate::manager::tests::state_service_comparison::StateServiceComparisonTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
            clients,
        })
    }
}

impl LaunchManager<crate::manager::tests::json_server_comparison::JsonServerComparisonTestManager> for JsonServerComparisonTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::json_server_comparison::JsonServerComparisonTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let _infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // JSON server tests always use zcashd for compatibility baseline
        let validator_config = ValidatorConfig::ZcashdConfig(ZcashdConfig {
            zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
            zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
            rpc_listen_port: Some(ports.validator_rpc.port()),
            activation_heights: ActivationHeights::default(),
            miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
            chain_cache: self.base.chain_cache.clone(),
        });

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Set up cookie directory if cookie auth is enabled
        let cookie_dir = if self.enable_cookie_auth {
            Some(ports.data_dir.join("cookies"))
        } else {
            None
        };

        // Initialize clients if requested
        let clients = if self.with_clients {
            Some(crate::clients::Clients::launch(ports.zaino_grpc.map(|addr| addr.port()).unwrap_or(8232)).await?)
        } else {
            None
        };

        Ok(crate::manager::tests::json_server_comparison::JsonServerComparisonTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
            cookie_auth_enabled: self.enable_cookie_auth,
            clients,
            cookie_dir,
        })
    }
}

impl LaunchManager<crate::manager::tests::chain_cache::ChainCacheTestManager> for ChainCacheTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::chain_cache::ChainCacheTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let _infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration based on kind
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: _infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Initialize clients if requested
        let clients = if self.with_clients {
            Some(crate::clients::Clients::launch(8232).await?)
        } else {
            None
        };

        Ok(crate::manager::tests::chain_cache::ChainCacheTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
            clients,
        })
    }
}

impl LaunchManager<crate::manager::tests::local_cache::LocalCacheTestManager> for LocalCacheTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::local_cache::LocalCacheTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let _infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration based on kind
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: _infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        Ok(crate::manager::tests::local_cache::LocalCacheTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
        })
    }
}

impl LaunchManager<crate::manager::tests::test_vectors::TestVectorGeneratorTestManager> for TestVectorTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::test_vectors::TestVectorGeneratorTestManager, Box<dyn std::error::Error>>
    {
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig};
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let ports = TestPorts::allocate().await?;

        // Convert network type
        let _infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration based on kind
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: _infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Test vector generation always requires clients
        let clients = crate::clients::Clients::launch(8232).await?;

        Ok(crate::manager::tests::test_vectors::TestVectorGeneratorTestManager {
            local_net,
            ports,
            network: self.base.network,
            chain_cache: self.base.chain_cache,
            clients,
        })
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
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::wallet::WalletTestManager, Box<dyn std::error::Error>> {
        use crate::clients::Clients;
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig, ValidatorKind};
        use zainodlib::indexer::start_indexer;
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let mut ports = TestPorts::allocate().await?;

        // Allocate additional ports for zaino services
        if let Some(grpc_port) = portpicker::pick_unused_port() {
            ports.zaino_grpc = Some(std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                grpc_port,
            ));
        }

        // Convert network type
        let infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Prepare indexer configuration
        let mut indexer_config = self.indexer;
        indexer_config.network = self.base.network;

        // Update backend configuration to point to our validator
        use zaino_commons::config::{
            BackendConfig, DatabaseConfig, ZcashdAuth, ZebraStateConfig, ZebradAuth,
        };
        indexer_config.backend = match self.base.validator_kind {
            ValidatorKind::Zcashd => BackendConfig::RemoteZcashd {
                rpc_address: ports.validator_rpc,
                auth: ZcashdAuth::Disabled,
            },
            ValidatorKind::Zebra => BackendConfig::LocalZebra {
                rpc_address: ports.validator_rpc,
                auth: ZebradAuth::Disabled,
                zebra_state: ZebraStateConfig {
                    cache_dir: ports.zebra_db.clone(),
                    ephemeral: false,
                    ..Default::default()
                },
                indexer_rpc_address: ports.validator_grpc,
                zebra_database: DatabaseConfig::default(),
            },
        };

        // Update server addresses
        if let Some(grpc_addr) = ports.zaino_grpc {
            indexer_config.server.grpc.listen_address = grpc_addr;
        }

        // Launch indexer
        let indexer_handle = start_indexer(indexer_config.clone()).await?;

        // Create clients if enabled
        let clients = if self.enable_clients {
            if let Some(grpc_addr) = ports.zaino_grpc {
                Clients::launch(grpc_addr.port()).await?
            } else {
                return Err("Zaino gRPC port not allocated but required for clients".into());
            }
        } else {
            return Err("Clients disabled but required for WalletTestManager".into());
        };

        Ok(crate::manager::tests::wallet::WalletTestManager {
            local_net,
            ports,
            network: self.base.network,
            indexer_config,
            indexer_handle,
            clients,
        })
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

impl LaunchManager<crate::manager::tests::json_server::JsonServerTestManager>
    for JsonServerTestConfig
{
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::json_server::JsonServerTestManager, Box<dyn std::error::Error>>
    {
        use crate::clients::Clients;
        use crate::ports::TestPorts;
        use crate::validator::{LocalNet, ValidatorConfig, ValidatorKind};
        use zainodlib::indexer::start_indexer;
        use zingo_infra_services::{
            network::{ActivationHeights, Network as InfraNetwork},
            validator::{Validator as _, ZcashdConfig, ZebradConfig},
        };

        // Allocate ports and directories
        let mut ports = TestPorts::allocate().await?;

        // Allocate additional ports for zaino services
        if let Some(grpc_port) = portpicker::pick_unused_port() {
            ports.zaino_grpc = Some(std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                grpc_port,
            ));
        }

        // Allocate JSON-RPC port for JSON server tests
        if let Some(json_port) = portpicker::pick_unused_port() {
            ports.zaino_json = Some(std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                json_port,
            ));
        }

        // Convert network type
        let infra_network = match self.base.network {
            zaino_commons::config::Network::Regtest => InfraNetwork::Regtest,
            zaino_commons::config::Network::Testnet => InfraNetwork::Testnet,
            zaino_commons::config::Network::Mainnet => InfraNetwork::Mainnet,
        };

        // Create validator configuration
        let validator_config = match self.base.validator_kind {
            ValidatorKind::Zcashd => ValidatorConfig::ZcashdConfig(ZcashdConfig {
                zcashd_bin: crate::binaries::ZCASHD_BIN.clone(),
                zcash_cli_bin: crate::binaries::ZCASH_CLI_BIN.clone(),
                rpc_listen_port: Some(ports.validator_rpc.port()),
                activation_heights: ActivationHeights::default(),
                miner_address: Some(testvectors::REG_O_ADDR_FROM_ABANDONART),
                chain_cache: self.base.chain_cache.clone(),
            }),
            ValidatorKind::Zebra => {
                ValidatorConfig::ZebrdConfig(ZebradConfig {
                    zebrad_bin: crate::binaries::ZEBRD_BIN.clone(),
                    network_listen_port: None, // Auto-select
                    rpc_listen_port: Some(ports.validator_rpc.port()),
                    indexer_listen_port: Some(ports.validator_grpc.port()),
                    activation_heights: ActivationHeights::default(),
                    miner_address: testvectors::REG_O_ADDR_FROM_ABANDONART,
                    chain_cache: self.base.chain_cache.clone(),
                    network: infra_network,
                })
            }
        };

        // Launch validator
        let local_net = LocalNet::launch(validator_config)
            .await
            .map_err(|e| format!("Failed to launch validator: {}", e))?;

        // Prepare indexer configuration
        let mut indexer_config = self.indexer;
        indexer_config.network = self.base.network;

        // Update backend configuration to point to our validator
        use zaino_commons::config::{
            BackendConfig, DatabaseConfig, ZcashdAuth, ZebraStateConfig, ZebradAuth,
        };
        indexer_config.backend = match self.base.validator_kind {
            ValidatorKind::Zcashd => BackendConfig::RemoteZcashd {
                rpc_address: ports.validator_rpc,
                auth: ZcashdAuth::Disabled,
            },
            ValidatorKind::Zebra => BackendConfig::LocalZebra {
                rpc_address: ports.validator_rpc,
                auth: ZebradAuth::Disabled,
                zebra_state: ZebraStateConfig {
                    cache_dir: ports.zebra_db.clone(),
                    ephemeral: false,
                    ..Default::default()
                },
                indexer_rpc_address: ports.validator_grpc,
                zebra_database: DatabaseConfig::default(),
            },
        };

        // Update server addresses
        if let Some(grpc_addr) = ports.zaino_grpc {
            indexer_config.server.grpc.listen_address = grpc_addr;
        }

        // Enable JSON-RPC server for JSON server tests
        if let Some(json_addr) = ports.zaino_json {
            use zaino_commons::config::{JsonRpcAuth, JsonRpcConfig};

            // Configure JSON-RPC server based on auth settings
            let json_auth = match &self.json_auth {
                JsonRpcAuthConfig::None => JsonRpcAuth::Disabled,
                JsonRpcAuthConfig::Cookie(cookie_dir) => {
                    // Create cookie directory if it doesn't exist
                    std::fs::create_dir_all(cookie_dir)?;
                    use zaino_commons::config::CookieAuth;
                    JsonRpcAuth::Cookie(CookieAuth {
                        path: cookie_dir.clone(),
                    })
                }
                JsonRpcAuthConfig::Password { .. } => {
                    // Note: JsonRpcAuth doesn't seem to have a Password variant based on the enum above
                    // This suggests that password auth might not be supported or is different
                    return Err("Password authentication not supported for JSON-RPC server".into());
                }
            };

            indexer_config.server.json_rpc = Some(JsonRpcConfig {
                listen_address: json_addr,
                auth: json_auth,
            });
        }

        // Launch indexer
        let indexer_handle = start_indexer(indexer_config.clone()).await?;

        // Create clients if enabled
        let clients = if self.enable_clients {
            if let Some(grpc_addr) = ports.zaino_grpc {
                Some(Clients::launch(grpc_addr.port()).await?)
            } else {
                return Err("Zaino gRPC port not allocated but required for clients".into());
            }
        } else {
            None
        };

        // Extract cookie directory from auth config for storage in manager
        let json_server_cookie_dir = match &self.json_auth {
            JsonRpcAuthConfig::Cookie(path) => Some(path.clone()),
            _ => None,
        };

        Ok(crate::manager::tests::json_server::JsonServerTestManager {
            local_net,
            ports,
            network: self.base.network,
            indexer_config,
            indexer_handle,
            json_server_cookie_dir,
            clients,
        })
    }
}
