# Zaino Test Managers Guide

This guide provides comprehensive documentation for Zaino's specialized test manager architecture, designed to replace the monolithic TestManager with type-safe, purpose-built test environments.

## Table of Contents

1. [Architecture Overview](#architecture-overview)
2. [Available Test Managers](#available-test-managers)
3. [Core Traits System](#core-traits-system)
4. [Quick Start Guide](#quick-start-guide)
5. [Advanced Usage](#advanced-usage)
6. [Creating Custom Test Managers](#creating-custom-test-managers)
7. [Service Builders](#service-builders)
8. [Migration Guide](#migration-guide)
9. [Best Practices](#best-practices)
10. [Troubleshooting](#troubleshooting)

## Architecture Overview

The new test manager architecture replaces the monolithic `TestManager` with **specialized managers** designed for specific testing scenarios. Each manager implements exactly the traits needed for its use case, providing **type safety** and **clear intent**.

### Key Benefits

- **Type Safety**: Compile-time prevention of calling wallet methods on service-only managers
- **Clear Intent**: `WalletTestsBuilder::default().launch()` vs cryptic 10-parameter calls  
- **No Boilerplate**: Service creation reduced from 50+ lines to 3-5 lines
- **Extensibility**: Easy to add new test scenarios without breaking existing code

### Design Principles

1. **Purpose-First**: Each manager designed for specific test scenarios
2. **Trait Composition**: Managers implement only needed capabilities
3. **Builder Pattern**: Fluent configuration with sensible defaults
4. **Production Fidelity**: Uses real production config types under the hood

## Available Test Managers

### 1. ServiceTestManager
**Purpose**: Basic service testing with validator and service factories
**When to Use**: Testing service creation, basic validator operations, state/fetch service comparisons

```rust
use zaino_testutils::{ServiceTestsBuilder, WithValidator, WithServiceFactories};

// Quick start
let manager = ServiceTestsBuilder::default().launch().await?;

// Customized
let manager = ServiceTestsBuilder::default()
    .zcashd()
    .testnet()
    .chain_cache(cache_path)
    .launch().await?;

// Service creation
let (fetch_service, _) = manager.create_fetch_service().build().await?;
let (state_service, _) = manager.create_state_service().build().await?;
```

**Available Methods**:
- All `WithValidator` trait methods (generate_blocks, wait_for_validator_ready, etc.)
- All `WithServiceFactories` trait methods (create_fetch_service, create_state_service, etc.)

### 2. WalletTestManager  
**Purpose**: Full-stack wallet testing with validator, indexer, and clients
**When to Use**: Testing wallet functionality, transaction creation, client operations

```rust
use zaino_testutils::{WalletTestsBuilder, WithValidator, WithClients, WithIndexer};

// Quick start with defaults
let manager = WalletTestsBuilder::default().launch().await?;

// Custom configuration
let manager = WalletTestsBuilder::default()
    .zebra()
    .mainnet()
    .customize_indexer(|config| {
        config.storage.cache.shard_amount = Some(8);
    })
    .launch().await?;

// Wallet operations (no Option unwrapping!)
let faucet_addr = manager.faucet().get_address("unified").await;
manager.prepare_for_shielding(100).await?;
```

**Available Methods**:
- All `WithValidator` trait methods
- All `WithClients` trait methods (faucet, recipient, sync_clients, etc.)
- All `WithIndexer` trait methods (access to indexer state)

### 3. JsonServerTestManager
**Purpose**: JSON-RPC server testing with optional client integration
**When to Use**: Testing JSON-RPC endpoints, server authentication, API compatibility

```rust
use zaino_testutils::{JsonServerTestsBuilder, JsonRpcAuthConfig};

// With cookie authentication
let manager = JsonServerTestsBuilder::default()
    .with_cookie_auth(cookie_dir)
    .with_clients(true)
    .launch().await?;

// Password authentication (when supported)
let manager = JsonServerTestsBuilder::default()
    .with_password_auth("user", "pass")
    .launch().await?;
```

**Available Methods**:
- All `WithValidator` trait methods
- All `WithClients` trait methods (when clients enabled)
- JSON server specific operations

### 4. StateServiceComparisonTestManager
**Purpose**: Compare FetchService vs StateService behavior
**When to Use**: Behavioral comparison tests, verifying service consistency

```rust
use zaino_testutils::StateServiceComparisonTestsBuilder;

let manager = StateServiceComparisonTestsBuilder::default()
    .with_clients(true)
    .launch().await?;

// Create both services for comparison
let (fetch_service, state_service) = manager.create_dual_services_with_network().await?;
```

### 5. JsonServerComparisonTestManager
**Purpose**: Compare zcashd baseline vs zaino JSON server
**When to Use**: JSON-RPC compatibility testing, regression testing against zcashd

```rust
use zaino_testutils::JsonServerComparisonTestsBuilder;

let manager = JsonServerComparisonTestsBuilder::default()
    .with_cookie_auth()
    .launch().await?;

// Always uses Zcashd for compatibility baseline
let (zcashd_service, zaino_service) = manager.create_comparison_services().await?;
```

### 6. FetchServiceTestManager
**Purpose**: Standalone FetchService functionality testing
**When to Use**: Testing FetchService in isolation, configuration testing

```rust
use zaino_testutils::FetchServiceTestsBuilder;

let manager = FetchServiceTestsBuilder::default()
    .with_clients(true)
    .launch().await?;

// Flexible FetchService creation
let (service, subscriber) = manager.create_fetch_service_configured(
    true,  // enable_sync
    true,  // enable_db  
    false  // no_sync
).await?;
```

### 7. ChainCacheTestManager
**Purpose**: Chain caching via JSON-RPC connector testing
**When to Use**: Testing chain cache functionality, RPC authentication

```rust
use zaino_testutils::ChainCacheTestsBuilder;

let manager = ChainCacheTestsBuilder::default()
    .with_clients(true)
    .chain_cache(cache_dir)
    .launch().await?;

// Separate health check and URL building
manager.check_validator_health().await?;
let url = manager.build_validator_url(Some("user"), Some("pass"))?;
```

### 8. LocalCacheTestManager
**Purpose**: Local block cache functionality via BlockCache
**When to Use**: Testing block caching, finalised vs non-finalised state

```rust
use zaino_testutils::LocalCacheTestsBuilder;

let manager = LocalCacheTestsBuilder::default().launch().await?;

// Block cache creation with configuration
let (connector, cache, subscriber) = manager.create_block_cache_setup(
    false, // no_sync
    false  // no_db
).await?;
```

### 9. TestVectorGeneratorTestManager
**Purpose**: Test vector generation and transaction parsing validation
**When to Use**: Creating test data for unit tests, validating parsing

```rust
use zaino_testutils::{TestVectorGeneratorTestsBuilder, TransactionOperation};

let manager = TestVectorGeneratorTestsBuilder::default()
    .mainnet() // For real network test vectors
    .launch().await?;

// State service for test vector generation
let (state_service, subscriber) = manager.create_state_service_for_vectors(None).await?;

// Transaction scenario execution
manager.execute_transaction_scenario(vec![
    TransactionOperation::Shield,
    TransactionOperation::MineBlock,
    TransactionOperation::SendToUnified(250_000),
], Some(addresses)).await?;
```

## Core Traits System

The trait system provides consistent interfaces across different managers while maintaining type safety.

### WithValidator

Core validator operations available on **all managers**.

```rust
pub trait WithValidator {
    fn local_net(&self) -> &LocalNet;
    fn validator_rpc_address(&self) -> SocketAddr;
    fn validator_grpc_address(&self) -> SocketAddr;
    fn network(&self) -> &Network;
    
    // Async methods
    async fn generate_blocks(&self, count: u32) -> Result<(), Box<dyn std::error::Error>>;
    async fn generate_blocks_with_delay(&self, count: u32) -> Result<(), Box<dyn std::error::Error>>;
    async fn wait_for_validator_ready(&self) -> Result<(), Box<dyn std::error::Error>>;
    async fn close(&mut self);
}
```

### WithClients

Wallet operations available on **managers with clients enabled**.

```rust
pub trait WithClients {
    fn clients(&self) -> &Clients;
    fn faucet(&self) -> &LightClient;
    fn recipient(&self) -> &LightClient;
    
    // Async methods
    async fn sync_clients(&mut self) -> Result<(), Box<dyn std::error::Error>>;
    async fn get_faucet_address(&mut self, addr_type: ClientAddressType) -> String;
    async fn get_recipient_address(&mut self, addr_type: ClientAddressType) -> String;
    async fn prepare_for_shielding(&mut self, blocks: u32) -> Result<(), Box<dyn std::error::Error>>;
}
```

### WithIndexer

Indexer state access for **managers with running indexers**.

```rust
pub trait WithIndexer {
    fn indexer_handle(&self) -> &JoinHandle<Result<(), IndexerError>>;
    fn indexer_config(&self) -> &IndexerConfig;
}
```

### WithServiceFactories

Service creation helpers for **managers that create services**.

```rust
pub trait WithServiceFactories {
    fn create_fetch_service(&self) -> FetchServiceBuilder;
    fn create_state_service(&self) -> StateServiceBuilder;
    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Box<dyn std::error::Error>>;
    fn create_block_cache(&self) -> BlockCacheBuilder;
}
```

## Quick Start Guide

### Basic Service Testing

```rust
use zaino_testutils::{ServiceTestsBuilder, WithValidator, WithServiceFactories};

#[tokio::test]
async fn test_basic_service_creation() -> Result<(), Box<dyn std::error::Error>> {
    // Launch test environment
    let manager = ServiceTestsBuilder::default().launch().await?;
    
    // Generate some blocks
    manager.generate_blocks_with_delay(10).await?;
    
    // Create and test a service
    let (fetch_service, _) = manager
        .create_fetch_service()
        .with_network(zaino_commons::config::Network::Regtest)
        .build()
        .await?;
    
    // Your test logic here...
    Ok(())
}
```

### Wallet Testing

```rust
use zaino_testutils::{WalletTestsBuilder, WithValidator, WithClients, ClientAddressType};

#[tokio::test]
async fn test_wallet_operations() -> Result<(), Box<dyn std::error::Error>> {
    // Launch wallet test environment
    let mut manager = WalletTestsBuilder::default().launch().await?;
    
    // Prepare environment for wallet operations
    manager.prepare_for_shielding(100).await?;
    
    // Get wallet addresses (no Option unwrapping!)
    let faucet_addr = manager.get_faucet_address(ClientAddressType::Unified).await;
    let recipient_addr = manager.get_recipient_address(ClientAddressType::Transparent).await;
    
    // Shield and send operations
    manager.faucet().quick_shield().await?;
    
    // Your wallet test logic here...
    Ok(())
}
```

### Service Comparison Testing

```rust
use zaino_testutils::StateServiceComparisonTestsBuilder;

#[tokio::test]
async fn test_service_behavior_comparison() -> Result<(), Box<dyn std::error::Error>> {
    let manager = StateServiceComparisonTestsBuilder::default()
        .with_clients(true)
        .launch().await?;
        
    // Create both services for comparison  
    let (fetch_service, fetch_subscriber) = manager.create_fetch_service().build().await?;
    let (state_service, state_subscriber) = manager.create_state_service().build().await?;
    
    // Compare behaviors...
    Ok(())
}
```

## Advanced Usage

### Custom Network Configuration

```rust
let manager = WalletTestsBuilder::default()
    .testnet()
    .chain_cache(PathBuf::from("/path/to/cache"))
    .customize_indexer(|config| {
        config.storage.cache.shard_amount = Some(16);
        config.server.grpc.worker_limit = Some(32);
    })
    .launch().await?;
```

### Multi-Service Testing

```rust
let manager = ServiceTestsBuilder::default().launch().await?;

// Create multiple services concurrently
let fetch_builder = manager.create_fetch_service().with_sync(true);
let state_builder = manager.create_state_service().with_cache_dir(custom_dir);

let (fetch_service, _) = fetch_builder.build().await?;
let (state_service, _) = state_builder.build().await?;

// Test interactions between services...
```

### Generic Functions with Traits

```rust
// Function works with any manager that has validator + clients
async fn test_wallet_scenario<T>(manager: &mut T) -> Result<(), Box<dyn std::error::Error>>
where
    T: WithValidator + WithClients,
{
    // Generate blocks
    manager.generate_blocks_with_delay(50).await?;
    
    // Sync wallets  
    manager.sync_clients().await?;
    
    // Your test logic...
    Ok(())
}

// Can be called with any compatible manager
test_wallet_scenario(&mut wallet_manager).await?;
test_wallet_scenario(&mut json_server_manager).await?; // if clients enabled
```

## Creating Custom Test Managers

When the existing specialized managers don't fit your use case, you can create custom managers following the same patterns.

### Step 1: Define Your Manager Struct

```rust
// In zaino-testutils/src/manager/tests/my_custom.rs

use crate::{
    config::{MyCustomTestConfig, TestConfig},
    manager::traits::{ConfigurableBuilder, LaunchManager, WithValidator},
    ports::TestPorts,
    validator::{LocalNet, ValidatorKind},
};

/// Custom test manager for [specific purpose]
/// 
/// **Purpose**: [Clear description of what this tests]
/// **Scope**: [What components are included]
/// **Use Case**: [When to use this manager]
#[derive(Debug)]
pub struct MyCustomTestManager {
    /// Local validator network
    pub local_net: LocalNet,
    /// Test ports and directories  
    pub ports: TestPorts,
    /// Network configuration
    pub network: Network,
    /// Custom fields for your specific use case
    pub my_custom_service: Option<MyService>,
}
```

### Step 2: Implement Required Traits

```rust
impl WithValidator for MyCustomTestManager {
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

// Implement other traits as needed (WithClients, WithServiceFactories, etc.)
```

### Step 3: Add Custom Methods

```rust
impl MyCustomTestManager {
    /// Custom method specific to your test scenario
    pub async fn create_my_custom_setup(&self) -> Result<MyCustomSetup, Box<dyn std::error::Error>> {
        // Your custom setup logic
        todo!()
    }
    
    /// Pattern: Separate health checks and configuration  
    pub async fn check_my_service_health(&self) -> Result<(), Box<dyn std::error::Error>> {
        // Health check logic
        todo!()
    }
}
```

### Step 4: Create Builder

```rust
/// Builder for MyCustomTestManager
#[derive(Debug, Clone)]
pub struct MyCustomTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    my_custom_option: bool,
}

impl Default for MyCustomTestsBuilder {
    fn default() -> Self {
        Self {
            validator_kind: ValidatorKind::Zebra,
            network: Network::Regtest,
            my_custom_option: false,
        }
    }
}

impl ConfigurableBuilder for MyCustomTestsBuilder {
    type Manager = MyCustomTestManager;
    type Config = MyCustomTestConfig;

    fn build_config(&self) -> Self::Config {
        MyCustomTestConfig {
            base: TestConfig {
                network: self.network.clone(),
                validator_kind: self.validator_kind,
                chain_cache: None,
            },
            my_custom_option: self.my_custom_option,
        }
    }

    async fn launch(self) -> Result<Self::Manager, Box<dyn std::error::Error>> {
        let config = self.build_config();
        config.launch_manager().await
    }

    // Standard builder methods
    fn validator(mut self, kind: ValidatorKind) -> Self {
        self.validator_kind = kind;
        self
    }

    fn network(mut self, network: Network) -> Self {
        self.network = network;
        self
    }
}

impl MyCustomTestsBuilder {
    /// Custom builder method
    pub fn with_my_option(mut self, enable: bool) -> Self {
        self.my_custom_option = enable;
        self
    }
}
```

### Step 5: Add Configuration Support

```rust
// In zaino-testutils/src/config.rs

/// Configuration for my custom tests
#[derive(Debug, Clone)]
pub struct MyCustomTestConfig {
    /// Base configuration
    pub base: TestConfig,
    /// Custom option
    pub my_custom_option: bool,
}

impl TestConfiguration for MyCustomTestConfig {
    fn network(&self) -> &Network {
        &self.base.network
    }

    fn validator_kind(&self) -> ValidatorKind {
        self.base.validator_kind
    }
}

impl LaunchManager<crate::manager::tests::my_custom::MyCustomTestManager> for MyCustomTestConfig {
    async fn launch_manager(
        self,
    ) -> Result<crate::manager::tests::my_custom::MyCustomTestManager, Box<dyn std::error::Error>>
    {
        // Launch validator and set up your custom manager
        // Follow the pattern from existing LaunchManager implementations
        todo!()
    }
}
```

### Step 6: Add Module Exports

```rust
// In zaino-testutils/src/manager/tests.rs
pub mod my_custom;
pub use my_custom::{MyCustomTestManager, MyCustomTestsBuilder};

// In zaino-testutils/src/lib.rs  
pub use config::MyCustomTestConfig;
pub use manager::tests::my_custom::{MyCustomTestManager, MyCustomTestsBuilder};
```

## Service Builders

Service builders provide a fluent interface for creating services with consistent configuration patterns.

### FetchServiceBuilder

```rust
let (fetch_service, subscriber) = manager
    .create_fetch_service()
    .with_network(Network::Testnet)
    .with_sync(true)
    .with_db(true)
    .with_auth(true)
    .with_data_dir(custom_dir)
    .build()
    .await?;
```

### StateServiceBuilder

```rust
let (state_service, subscriber) = manager
    .create_state_service()
    .with_validator_rpc_address(custom_rpc)
    .with_validator_grpc_address(custom_grpc)
    .with_network(Network::Mainnet)
    .with_cache_dir(cache_path)
    .build()
    .await?;
```

### BlockCacheBuilder  

```rust
let block_cache = manager
    .create_block_cache()
    .with_network(Network::Regtest)
    .with_cache_dir(cache_dir)
    .build()
    .await?;
```

## Migration Guide

### From Monolithic TestManager

**Old Pattern:**
```rust
let test_manager = TestManager::launch(
    &ValidatorKind::Zebra,
    &BackendType::Fetch, 
    None,
    None,
    true,
    false,
    false,
    true,
    true,
    true
).await.unwrap();
```

**New Pattern:**
```rust
let manager = ServiceTestsBuilder::default()
    .zebra()
    .launch().await?;
```

### From Helper Functions

**Old Pattern:**
```rust
async fn create_test_manager_and_services(...) -> (TestManager, FetchService, StateService) {
    // 50+ lines of boilerplate
}
```

**New Pattern:**
```rust
let manager = StateServiceComparisonTestsBuilder::default().launch().await?;
let (fetch_service, state_service) = manager.create_dual_services().await?;
```

### Service Creation

**Old Pattern:**
```rust
let fetch_service = FetchService::spawn(FetchServiceConfig::new(
    test_manager.zebrad_rpc_listen_address,
    false, None, None, None, None, None, None, None,
    test_manager.local_net.data_dir().path().to_path_buf().join("zaino"),
    None,
    Network::new_regtest(/* 20 lines */),
    true, true,
)).await.unwrap();
```

**New Pattern:**
```rust
let (fetch_service, _) = manager.create_fetch_service().build().await?;
```

## Best Practices

### 1. Choose the Right Manager

- Use **ServiceTestManager** for basic service testing
- Use **WalletTestManager** for full-stack wallet operations
- Use **JsonServerTestManager** for API testing
- Use **ComparisonTestManagers** for behavioral validation
- Use **specialized managers** for focused testing scenarios

### 2. Leverage Type Safety

```rust
// Good: Type-safe access
let manager = WalletTestsBuilder::default().launch().await?;
let address = manager.faucet().get_address("unified").await; // Always works

// Avoid: Optional unwrapping (old pattern)  
// let address = test_manager.clients.unwrap().faucet.get_address("unified").await;
```

### 3. Use Builders Effectively

```rust
// Good: Fluent configuration with clear intent
let manager = JsonServerTestsBuilder::default()
    .zcashd()
    .testnet()
    .with_cookie_auth(cookie_dir)
    .with_clients(true)
    .launch().await?;

// Good: Progressive customization
let builder = WalletTestsBuilder::default().mainnet();
let builder = if use_cache { builder.chain_cache(cache_path) } else { builder };
let manager = builder.launch().await?;
```

### 4. Generic Programming with Traits

```rust
// Write functions that work with multiple manager types
async fn test_basic_validator_operations<T>(manager: &T) 
where T: WithValidator 
{
    manager.generate_blocks(10).await?;
    manager.wait_for_validator_ready().await?;
}

// Works with any manager
test_basic_validator_operations(&service_manager).await?;
test_basic_validator_operations(&wallet_manager).await?;
```

### 5. Error Handling Patterns

```rust
// Good: Propagate errors with context
let manager = ServiceTestsBuilder::default()
    .launch().await
    .map_err(|e| format!("Failed to launch test environment: {}", e))?;

// Good: Use Result<(), Box<dyn std::error::Error>> for test functions
#[tokio::test]  
async fn my_test() -> Result<(), Box<dyn std::error::Error>> {
    let manager = ServiceTestsBuilder::default().launch().await?;
    // Test logic...
    Ok(())
}
```

## Troubleshooting

### Common Issues

**1. "Clients not enabled for this manager"**
```
Error: Clients not enabled for this manager. Use with_clients(true) in builder.
```

**Solution**: Use a manager type that supports clients or enable clients:
```rust
// Wrong: ServiceTestManager doesn't have clients
let manager = ServiceTestsBuilder::default().launch().await?;
// manager.faucet() // ❌ Compile error

// Right: Use WalletTestManager  
let manager = WalletTestsBuilder::default().launch().await?;
manager.faucet(); // ✅ Works

// Or: Enable clients on supported managers
let manager = JsonServerTestsBuilder::default()
    .with_clients(true) // ✅ Enable clients
    .launch().await?;
```

**2. "Method not found" errors**

Make sure you import the required traits:
```rust
use zaino_testutils::{
    ServiceTestsBuilder, 
    WithValidator,      // For generate_blocks, etc.
    WithServiceFactories, // For create_fetch_service, etc.
};
```

**3. Port allocation failures**

Ports are automatically allocated, but can conflict in parallel tests:
```rust
// Tests run in parallel may conflict - this is expected and rare
// The system will retry port allocation automatically
```

**4. Validator startup timeouts**

Real networks (testnet/mainnet) require longer startup times:
```rust
let manager = WalletTestsBuilder::default()
    .mainnet() // Will automatically wait longer for startup
    .launch().await?;
```

### Debug Information

Enable debug logging to see manager startup and configuration:
```rust
tracing_subscriber::fmt::init();

let manager = ServiceTestsBuilder::default()
    .launch().await?;
// Will log validator startup, port allocation, etc.
```

### Performance Tips

1. **Reuse managers** in the same test when possible
2. **Use regtest** for fast tests (mainnet/testnet are slow)  
3. **Enable parallel test execution** - managers handle port conflicts
4. **Cache chain data** when using real networks

---

This comprehensive guide covers the full spectrum of working with Zaino's specialized test managers. The architecture provides type safety, clear intent, and extensibility while dramatically reducing test boilerplate compared to the old monolithic approach.