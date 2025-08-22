//! Zaino Testing Utilities
//!
//! This crate provides a comprehensive testing framework for Zaino, the Zcash blockchain indexer.
//! It enables easy setup and orchestration of complete test environments including validators,
//! indexers, and lightclients.
//!
//! ## Architecture Overview
//!
//! The testing framework is built around the **Test Environment** concept, where you specify
//! *what* services you want (validator, indexer, clients) and *how* they should be configured,
//! rather than manually setting up individual components.
//!
//! ### Key Components
//!
//! - **[`TestEnvironment`]**: High-level specification of test topology (what services to run)
//! - **[`TestManager`]**: Orchestrator that launches and manages all services
//! - **[`LocalNet`]**: Validator network abstraction (Zcashd or Zebrd)
//! - **[`Clients`]**: Managed zingolib lightclients for wallet testing
//!
//! ### Service Types
//!
//! - **Validators**: [`ValidatorKind::Zcashd`] or [`ValidatorKind::Zebrd`]
//! - **Backend Modes**: [`BackendMode::Fetch`] (JSON-RPC) or [`BackendMode::State`] (direct state access)
//! - **Optional Services**: JSON-RPC server, lightclients, chain cache loading
//!
//! ## How TestManager Works
//!
//! The [`TestManager`] follows a **topology-first** approach:
//!
//! 1. **Environment Specification**: Define what services you need using [`TestEnvironment`] builders
//! 2. **Resource Allocation**: Automatically allocates ports, directories, and network resources
//! 3. **Service Orchestration**: Launches services in correct order with proper configuration
//! 4. **Configuration Translation**: Converts test specifications into production config types
//! 5. **Lifecycle Management**: Handles startup, cleanup, and inter-service communication
//!
//! ### Configuration Flow
//!
//! ```text
//! TestEnvironment -> TestManager -> Production Configs (IndexerConfig, etc.)
//!       ↑                ↓
//!   Test-focused    Real service configs
//!   specifications   with proper types
//! ```
//!
//! ## Usage Examples
//!
//! ### Basic Validator-Only Test
//!
//! ```no_run
//! use zaino_testutils::{TestEnvironment, TestManager, ValidatorKind};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Just run a Zebrd validator
//!     let env = TestEnvironment::validator_only(ValidatorKind::Zebrd);
//!     let test_manager = TestManager::launch(env).await?;
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
//! use zaino_testutils::{TestEnvironment, TestManager, ValidatorKind, BackendMode};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Run validator + indexer + lightclients
//!     let env = TestEnvironment::full_stack(ValidatorKind::Zebrd, BackendMode::Fetch);
//!     let mut test_manager = TestManager::launch(env).await?;
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
//! use zaino_testutils::{TestEnvironment, TestManager, ValidatorKind};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Zcashd with Zaino's JSON-RPC server (with cookie auth)
//!     let env = TestEnvironment::json_server_tests(ValidatorKind::Zcashd, true);
//!     let test_manager = TestManager::launch(env).await?;
//!
//!     // Test JSON-RPC calls to both zcashd and zaino
//!     // ... your test logic here ...
//!     
//!     Ok(())
//! }
//! ```
//!
//! ### State Service Testing
//!
//! ```no_run
//! use zaino_testutils::{TestEnvironment, TestManager, ValidatorKind};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Direct Zebra state access (faster, more accurate)
//!     let env = TestEnvironment::state_tests(ValidatorKind::Zebrd);
//!     let test_manager = TestManager::launch(env).await?;
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
//! use zaino_testutils::{TestEnvironment, TestManager, ValidatorKind, BackendMode};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let env = TestEnvironment::basic_tests(ValidatorKind::Zebrd, BackendMode::Fetch)
//!         .with_database_size(100 * 1024 * 1024)  // 100MB DB
//!         .with_cache_capacity(1000)               // 1000 block cache
//!         .customize_storage(|storage| {
//!             // Custom storage tweaks
//!             storage.cache.shard_amount = Some(4);
//!         });
//!
//!     let test_manager = TestManager::launch(env).await?;
//!     Ok(())
//! }
//! ```
//!
//! ### Chain Cache Loading
//!
//! ```no_run
//! use std::path::PathBuf;
//! use zaino_testutils::{TestEnvironment, TestManager, ValidatorKind, ZEBRD_CHAIN_CACHE_DIR};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     if let Some(cache_dir) = ZEBRD_CHAIN_CACHE_DIR.clone() {
//!         let env = TestEnvironment::chain_cache_tests(ValidatorKind::Zebrd, cache_dir);
//!         let test_manager = TestManager::launch(env).await?;
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
//! - [`environment`]: Test environment specifications and builder patterns
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
    use config::{TestConfigBuilder};

    #[tokio::test] 
    async fn test_full_integration() {
        // Test the full integration with validator + indexer + clients
        let env = TestEnvironment::full_stack(ValidatorKind::Zebrd, BackendMode::Fetch);
        
        // This would normally launch everything but we'll just test the builder
        assert!(env.indexer.is_some());
        assert!(env.clients.is_some());
        assert_eq!(env.validator.kind, ValidatorKind::Zebrd);
    }

    #[tokio::test]
    async fn test_json_server_scenario() {
        // Test JSON server setup
        let env = TestEnvironment::json_server_tests(ValidatorKind::Zcashd, true);
        
        assert!(env.indexer.is_some());
        assert!(env.indexer.as_ref().unwrap().enable_json_server);
        // Should have cookie auth enabled
        assert!(!matches!(env.auth.validator_auth, zaino_commons::config::JsonRpcAuth::Disabled));
    }
}