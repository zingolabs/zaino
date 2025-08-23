//! Zaino Testing Utilities
//!
//! This crate provides a comprehensive testing framework for Zaino, the Zcash blockchain indexer.
//! It enables easy setup and orchestration of complete test environments including validators,
//! indexers, and lightclients with **specialized test managers** for different testing scenarios.
//!
//! ## Architecture Overview
//!
//! The testing framework is built around **specialized test managers** that provide type-safe,
//! scenario-specific testing environments. Each manager implements exactly the traits needed
//! for its test scenario, preventing runtime errors and reducing boilerplate.
//!
//! ### Key Components
//!
//! - **[`TestManagerBuilder`]**: Ergonomic facade for creating specialized test managers
//! - **Specialized Managers**: Type-safe managers for specific test scenarios  
//! - **[`LocalNet`]**: Validator network abstraction (Zcashd or Zebra)
//! - **[`Clients`]**: Managed zingolib lightclients for wallet testing
//! - **Service Factories**: Eliminate boilerplate service creation
//!
//! ### Test Manager Types
//!
//! - **[`ServiceTestManager`]**: Validator + service creation factories
//! - **[`WalletTestManager`]**: Validator + indexer + clients (always available)
//! - **[`JsonServerTestManager`]**: Validator + indexer + JSON server + optional clients
//!
//! ### Trait System
//!
//! - **[`WithValidator`]**: Core validator operations (all managers)
//! - **[`WithClients`]**: Wallet operations (wallet + JSON server managers)
//! - **[`WithIndexer`]**: Indexer state access (wallet + JSON server managers)  
//! - **[`WithServiceFactories`]**: Service creation helpers (service + wallet managers)
//!
//! ## Type Safety Benefits
//!
//! The trait-based design prevents invalid operations at compile time:
//!
//! ```rust
//! use zaino_testutils::{TestManagerBuilder, WithClients};
//!
//! // ✅ Valid: WalletTestManager always has clients
//! let manager = TestManagerBuilder::for_wallet_tests().await?;
//! let faucet = manager.faucet(); // No Option unwrapping!
//!
//! // ❌ Invalid: ServiceTestManager doesn't have clients
//! let service_manager = TestManagerBuilder::for_service_tests().await?;
//! // service_manager.faucet(); // Compile error - trait not implemented
//! ```
//!
//! ## Usage Examples
//!
//! ### Basic Validator-Only Test
//!
//! ```no_run
//! use zaino_testutils::{TestConfigBuilder, TestManager};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Just run a Zebra validator (no indexer)
//!     let config = TestConfigBuilder::validator_only_zebra();
//!     let test_manager = TestManager::launch(config).await?;
//!
//!     // Generate some blocks
//!     test_manager.generate_blocks_with_delay(10).await;
//!     
//!     Ok(())
//! }
//! ```
//!
//! ### Full Stack Integration Test
//!
//! ```no_run
//! use zaino_testutils::{TestConfigBuilder, TestManager};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     // Run validator + indexer + lightclients (type-safe)
//!     let config = TestConfigBuilder::full_stack_local_zebra();
//!     let mut test_manager = TestManager::launch(config).await?;
//!
//!     // Wait for everything to be ready
//!     test_manager.wait_for_validator_ready().await?;
//!
//!     // Use the lightclients
//!     if let Some(clients) = &test_manager.clients {
//!         let faucet_addr = clients.get_faucet_address("unified").await;
//!         println!("Faucet address: {}", faucet_addr);
//!     }
//!
//!     test_manager.close().await;
//!     Ok(())
//! }
//! ```
//!
//! ### JSON Server Test Environment
//!
//! ```no_run
//! use zaino_testutils::{TestConfigBuilder, TestManager};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Remote Zebra with Zaino's JSON-RPC server (with cookie auth)
//!     let config = TestConfigBuilder::json_server_tests_with_auth();
//!     let test_manager = TestManager::launch(config).await?;
//!
//!     // Test JSON-RPC calls to both zebra and zaino
//!     // ... your test logic here ...
//!     
//!     Ok(())
//! }
//! ```
//!
//! ### State Service Testing
//!
//! ```no_run
//! use zaino_testutils::{TestConfigBuilder, TestManager};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Direct Zebra state access (faster, more accurate)
//!     let config = TestConfigBuilder::state_tests();
//!     let test_manager = TestManager::launch(config).await?;
//!
//!     // Get the BackendConfig for additional services
//!     let backend_config = test_manager.backend_config();
//!     
//!     Ok(())
//! }
//! ```
//!
//! ### Custom Configuration
//!
//! ```no_run
//! use zaino_testutils::{TestConfigBuilder, TestManager};
//! use zaino_commons::config::Network;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = TestConfigBuilder::remote_zebra()
//!         .with_network(Network::Mainnet)
//!         .with_sync_and_db()
//!         .customize_config(|config| {
//!             // Direct IndexerConfig customization
//!             config.storage.cache.shard_amount = Some(4);
//!         });
//!
//!     let test_manager = TestManager::launch(config).await?;
//!     Ok(())
//! }
//! ```
//!
//! ### Chain Cache Loading
//!
//! ```no_run
//! use std::path::PathBuf;
//! use zaino_testutils::{TestConfigBuilder, TestManager, ZEBRD_CHAIN_CACHE_DIR, Validator};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     if let Some(cache_dir) = ZEBRD_CHAIN_CACHE_DIR.clone() {
//!         let config = TestConfigBuilder::remote_zebra()
//!             .with_chain_cache(cache_dir);
//!         let test_manager = TestManager::launch(config).await?;
//!         
//!         // Start with pre-synced blockchain state
//!         println!("Chain height: {:?}", test_manager.local_net.get_chain_height().await);
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Module Organization
//!
//! - [`config`]: Type-safe test configuration builders and specifications
//! - [`manager`]: Main orchestration logic and service management  
//! - [`validator`]: Validator launching (Zcashd/Zebrd) and LocalNet abstraction
//! - [`clients`]: Lightclient creation and management
//! - [`ports`]: Port allocation and network resource management
//! - [`binaries`]: Test binary paths and constants
//!
//! ## Design Philosophy
//!
//! The framework follows these principles:
//!
//! - **Topology-First**: Specify *what* you want to test, not *how* to configure it
//! - **Production Fidelity**: Uses real production config types under the hood
//! - **Incremental Customization**: Start simple, add complexity only where needed
//! - **Resource Management**: Automatic cleanup and lifecycle management
//! - **Type Safety**: Compile-time prevention of invalid configurations

#![warn(missing_docs)]
#![forbid(unsafe_code)]

/// Test binary paths and constants.
pub mod binaries;

/// Lightclient creation and management.  
pub mod clients;

/// Test environment specifications and builders.
pub mod config;

/// Test environment orchestration and management.
pub mod manager;

/// Port allocation and network configuration.
pub mod ports;

/// Validator launching and management.
pub mod validator;

// Re-export configuration types
pub use config::{
    TestConfig, ServiceTestConfig, WalletTestConfig, JsonServerTestConfig, JsonRpcAuthConfig,
};
pub use manager::{
    TestManagerBuilder,
    tests::{
        service::{ServiceTestManager, ServiceTestsBuilder},
        wallet::{WalletTestManager, WalletTestsBuilder},
        json_server::{JsonServerTestManager, JsonServerTestsBuilder},
    },
    traits::{
        WithValidator, WithClients, WithIndexer, WithServiceFactories,
    },
    factories::{
        FetchServiceBuilder, StateServiceBuilder, BlockCacheBuilder,
    },
};
pub use validator::{LocalNet, ValidatorConfig};

// Re-export commonly used external types
pub use zingo_infra_services as services;
pub use zingo_infra_services::validator::Validator;
pub use zingolib::{
    get_base_address_macro, lightclient::LightClient, testutils::lightclient::from_inputs,
};

// Re-export commonly used constants
pub use binaries::*;

#[cfg(test)]
mod integration_tests {
    use super::*;
    use zaino_commons::config::Network;
    use crate::validator::ValidatorKind;

    #[tokio::test] 
    async fn test_service_manager_creation() {
        // Test that we can create service managers with the new API
        let builder = ServiceTestsBuilder::default();
        let config = builder.build_config();
        
        // Verify the configuration structure
        assert_eq!(config.validator_kind(), ValidatorKind::Zebra);
        assert_eq!(config.network(), &Network::Regtest);
    }

    #[tokio::test]
    async fn test_wallet_manager_creation() {
        // Test that we can create wallet managers with the new API
        let builder = WalletTestsBuilder::default();
        let config = builder.build_config();
        
        // Verify the configuration structure
        assert_eq!(config.validator_kind(), ValidatorKind::Zebra);
        assert_eq!(config.network(), &Network::Regtest);
        assert!(config.enable_clients); // Should be true by default for wallet tests
    }

    #[tokio::test] 
    async fn test_json_server_manager_creation() {
        // Test that we can create JSON server managers with the new API
        let builder = JsonServerTestsBuilder::default();
        let config = builder.build_config();
        
        // Verify the configuration structure
        assert_eq!(config.validator_kind(), ValidatorKind::Zebra);
        assert_eq!(config.network(), &Network::Regtest);
        assert!(!config.enable_clients); // Should be false by default for JSON server tests
    }

    #[tokio::test]
    async fn test_builder_customization() {
        // Test builder customization methods
        let builder = ServiceTestsBuilder::default()
            .zcashd()
            .testnet();
        let config = builder.build_config();
        
        assert_eq!(config.validator_kind(), ValidatorKind::Zcashd);
        assert_eq!(config.network(), &Network::Testnet);
    }
}