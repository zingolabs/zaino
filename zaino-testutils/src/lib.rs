//! Zaino Testing Utilities
//!
//! This crate provides a comprehensive testing framework for Zaino, the Zcash blockchain indexer.
//! It enables easy setup and orchestration of complete test environments including validators,
//! indexers, and lightclients with **type-safe configuration**.
//!
//! ## Architecture Overview
//!
//! The testing framework is built around the **TestConfigBuilder** concept, where you specify
//! *what* services you want (validator, indexer, clients) and *how* they should be configured,
//! with compile-time guarantees that prevent invalid combinations.
//!
//! ### Key Components
//!
//! - **[`TestConfigBuilder`]**: Type-safe configuration builder that prevents invalid backend combinations
//! - **[`TestManager`]**: Orchestrator that launches and manages all services
//! - **[`LocalNet`]**: Validator network abstraction (Zcashd or Zebra)
//! - **[`Clients`]**: Managed zingolib lightclients for wallet testing
//!
//! ### Backend Types (Type-Safe)
//!
//! - **Local Zebra**: [`TestConfigBuilder::local_zebra()`] - Direct state access (StateService)
//! - **Remote Zebra**: [`TestConfigBuilder::remote_zebra()`] - JSON-RPC to Zebra (FetchService)
//! - **Remote Zcashd**: [`TestConfigBuilder::remote_zcashd()`] - JSON-RPC to Zcashd (FetchService)
//! - **Optional Services**: JSON-RPC server, lightclients, chain cache loading
//!
//! ## How TestManager Works
//!
//! The [`TestManager`] follows a **configuration-first** approach:
//!
//! 1. **Configuration Building**: Define what services you need using [`TestConfigBuilder`] methods
//! 2. **Resource Allocation**: Automatically allocates ports, directories, and network resources
//! 3. **Service Orchestration**: Launches services in correct order with proper configuration
//! 4. **Configuration Translation**: Converts test specifications into production config types
//! 5. **Lifecycle Management**: Handles startup, cleanup, and inter-service communication
//!
//! ### Configuration Flow
//!
//! ```text
//! TestConfigBuilder -> TestManager -> Production Services
//!       ↑                ↓
//!   Type-safe        Real service configs
//!   test configs     (IndexerConfig, etc.)
//! ```
//!
//! ## Type Safety Benefits
//!
//! The new design prevents invalid configurations at compile time:
//!
//! ```rust
//! // ✅ Valid: LocalZebra automatically uses StateService  
//! let config = TestConfigBuilder::local_zebra();
//!
//! // ✅ Valid: RemoteZcashd with appropriate auth
//! let config = TestConfigBuilder::remote_zcashd()
//!     .with_zcashd_password_auth("user".into(), "pass".into());
//!
//! // ❌ Invalid: This panics at runtime (by design)
//! // TestConfigBuilder::remote_zcashd().with_zebra_cookie_auth(path)
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
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
//! use zaino_testutils::{TestConfigBuilder, TestManager, ZEBRD_CHAIN_CACHE_DIR};
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

// Re-export main types for convenience
pub use config::{
    TestConfigBuilder, TestingFlags,
};
pub use manager::TestManager;
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
    use zaino_commons::config::*;
    use zainodlib::config::default_ephemeral_cookie_path;

    #[tokio::test] 
    async fn test_full_integration() {
        // Test the full integration with validator + indexer + clients  
        let builder = TestConfigBuilder::full_stack_local_zebra();
        
        // Verify the configuration
        assert!(builder.enable_indexer());
        assert!(builder.enable_lightclients());
        assert!(matches!(builder.indexer_config().backend, BackendConfig::LocalZebra { .. }));
    }

    #[tokio::test]
    async fn test_json_server_scenario() {
        // Test JSON server setup with authentication
        let builder = TestConfigBuilder::json_server_tests_with_auth();
        
        assert!(builder.enable_indexer());
        assert!(builder.indexer_config().server.json_rpc.is_some());
        
        // Should have cookie auth enabled on backend
        match &builder.indexer_config().backend {
            BackendConfig::RemoteZebra { auth, .. } => {
                assert!(matches!(auth, ZebradAuth::Cookie(_)));
            }
            _ => panic!("Expected RemoteZebra backend"),
        }
    }

    #[tokio::test] 
    async fn test_type_safety() {
        // Demonstrate type safety improvements
        
        // This creates a valid local zebra (state) configuration
        let local_config = TestConfigBuilder::local_zebra();
        assert!(matches!(local_config.indexer_config().backend, BackendConfig::LocalZebra { .. }));
        
        // This creates a valid remote zcashd (fetch) configuration  
        let zcashd_config = TestConfigBuilder::remote_zcashd();
        assert!(matches!(zcashd_config.indexer_config().backend, BackendConfig::RemoteZcashd { .. }));
        
        // Type-safe auth methods
        let zebra_with_auth = TestConfigBuilder::remote_zebra()
            .with_zebra_cookie_auth(default_ephemeral_cookie_path());
        
        match &zebra_with_auth.indexer_config().backend {
            BackendConfig::RemoteZebra { auth, .. } => {
                assert!(matches!(auth, ZebradAuth::Cookie(_)));
            }
            _ => panic!("Expected RemoteZebra backend"),
        }
        
        // Note: The following would cause a runtime panic (by design):
        // TestConfigBuilder::remote_zcashd().with_zebra_cookie_auth(path) 
        // Because Zcashd backends don't support Zebra auth types
    }

    #[tokio::test]
    async fn test_validator_only_modes() {
        // Test validator-only configurations (no indexer)
        let validator_only = TestConfigBuilder::validator_only_zebra();
        assert!(!validator_only.enable_indexer());
        assert!(!validator_only.enable_lightclients());
        
        let zcashd_only = TestConfigBuilder::validator_only_zcashd();
        assert!(!zcashd_only.enable_indexer());
        assert!(matches!(zcashd_only.indexer_config().backend, BackendConfig::RemoteZcashd { .. }));
    }
}