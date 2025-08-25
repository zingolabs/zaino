# TestManager Redesign Implementation Guide

This document provides a comprehensive guide for implementing the TestManager architecture redesign. It serves as the technical implementation roadmap for the architectural decisions documented in [ADR-0001](adrs/0001-testmanager-ergonomic-redesign.md).

## Table of Contents

- [Overview](#overview)
- [Architecture Overview](#architecture-overview)
- [File Structure](#file-structure)
- [Trait System](#trait-system)
- [Manager Types](#manager-types)
- [Configuration System](#configuration-system)
- [Service Factories](#service-factories)
- [Public API](#public-api)
- [Implementation Stages](#implementation-stages)
- [Migration Examples](#migration-examples)
- [Testing Strategy](#testing-strategy)

## Overview

The TestManager redesign replaces the monolithic 10-parameter approach with a trait-based system using specialized managers. This provides:

- **Type safety** - Compile-time prevention of invalid operations
- **Clear intent** - `TestManagerBuilder::for_wallet_tests()` vs cryptic parameter lists
- **Massive boilerplate reduction** - 50+ line service setups become 3-5 lines
- **Better observability** - Targeted metrics and logging per test scenario
- **Extensibility** - Easy addition of new test scenarios

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    TestManagerBuilder                       │
│                      (Public Facade)                       │
├─────────────────────────────────────────────────────────────┤
│  for_wallet_tests()  │  for_service_tests()  │  for_json_*  │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                 Specialized Managers                        │
├─────────────────┬─────────────────┬─────────────────────────┤
│ ServiceTestMgr  │ WalletTestMgr   │ JsonServerTestMgr       │
│                 │                 │                         │
│ • Validator     │ • Validator     │ • Validator             │
│ • Factories     │ • Indexer       │ • Indexer               │
│                 │ • Clients       │ • JSON Server           │
│                 │ • Factories     │ • Optional Clients      │
└─────────────────┴─────────────────┴─────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     Trait System                           │
├──────────────┬──────────────┬──────────────┬───────────────┤
│WithValidator │ WithClients  │ WithIndexer  │WithFactories  │
│              │              │              │               │
│• generate_*  │• clients()   │• config()    │• create_*     │
│• addresses   │• faucet()    │• addresses   │• factories    │
│• network()   │• recipient() │• handles     │               │
│• close()     │• sync_*()    │              │               │
└──────────────┴──────────────┴──────────────┴───────────────┘
```

## File Structure

```
zaino-testutils/src/
├── manager/
│   ├── traits/
│   │   ├── with_validator.rs          # Core validator operations
│   │   ├── with_clients.rs            # Wallet client operations
│   │   ├── with_indexer.rs            # Indexer state access
│   │   ├── with_service_factories.rs  # Service creation helpers
│   │   └── config_builder.rs          # Generic builder traits
│   ├── tests/
│   │   ├── service.rs                 # ServiceTestManager + Builder
│   │   ├── wallet.rs                  # WalletTestManager + Builder
│   │   └── json_server.rs             # JsonServerTestManager + Builder
│   └── factories.rs                   # Service creation builders
├── config.rs                          # Purpose-built configuration types
├── ports.rs                           # Existing port allocation
├── validator.rs                       # Existing validator wrapper
├── clients.rs                         # Existing client wrapper
└── lib.rs                             # Public API and re-exports
```

### File Responsibilities

- **`traits/`** - Core trait definitions with default implementations
- **`tests/`** - Complete manager implementations (struct + builder + trait impls)
- **`factories.rs`** - Service creation with sensible defaults
- **`config.rs`** - Domain-specific configuration types
- **`lib.rs`** - Public facade and ergonomic re-exports

## Trait System

The trait system provides composable capabilities that managers can implement based on their needs.

### Core Traits

#### `WithValidator`
**Purpose**: Core validator operations available to all managers.
**Location**: `manager/traits/with_validator.rs`

```rust
pub trait WithValidator {
    fn validator_rpc_address(&self) -> SocketAddr;
    fn validator_grpc_address(&self) -> SocketAddr;
    fn network(&self) -> &Network;
    
    // Default implementations
    async fn generate_blocks(&self, count: u32) -> Result<(), Error>;
    async fn generate_blocks_with_delay(&self, count: u32) -> Result<(), Error>;
    async fn wait_for_validator_ready(&self) -> Result<(), Error>;
    async fn close(&mut self);
}
```

**Implemented by**: All manager types
**Key methods**:
- `generate_blocks()` - Basic block generation
- `generate_blocks_with_delay()` - Block generation with sync delays
- `validator_rpc_address()` - Access to validator RPC endpoint
- `wait_for_validator_ready()` - Validator startup synchronization

#### `WithClients` 
**Purpose**: Wallet client operations for managers that have lightclients.
**Location**: `manager/traits/with_clients.rs`

```rust
pub trait WithClients {
    fn clients(&self) -> &Clients;
    
    // Convenience methods
    fn faucet(&self) -> &LightClient { &self.clients().faucet }
    fn recipient(&self) -> &LightClient { &self.clients().recipient }
    
    async fn sync_clients(&self) -> Result<(), Error>;
    async fn get_faucet_address(&self, addr_type: &str) -> String;
    async fn get_recipient_address(&self, addr_type: &str) -> String;
    
    // Workflow helpers
    async fn prepare_for_shielding(&self, blocks: u32) -> Result<(), Error>
    where Self: WithValidator;
}
```

**Implemented by**: `WalletTestManager`, `JsonServerTestManager` (if clients enabled)
**Key methods**:
- `clients()` - Direct client access (no Option unwrapping)
- `prepare_for_shielding()` - Common wallet workflow automation
- `get_*_address()` - Address generation helpers

#### `WithIndexer`
**Purpose**: Access to indexer state and configuration.
**Location**: `manager/traits/with_indexer.rs`

```rust
pub trait WithIndexer {
    fn indexer_config(&self) -> &IndexerConfig;
    fn zaino_grpc_address(&self) -> Option<SocketAddr>;
    fn zaino_json_address(&self) -> Option<SocketAddr>;
    fn json_server_cookie_dir(&self) -> Option<&PathBuf>;
    
    // Indexer management
    fn indexer_handle(&self) -> &JoinHandle<Result<(), IndexerError>>;
}
```

**Implemented by**: `WalletTestManager`, `JsonServerTestManager`
**Key methods**:
- `indexer_config()` - Access to indexer configuration
- `zaino_*_address()` - Zaino service endpoints
- `json_server_cookie_dir()` - Authentication directory for JSON server

#### `WithServiceFactories`
**Purpose**: Service creation with sensible defaults.
**Location**: `manager/traits/with_service_factories.rs`

```rust
pub trait WithServiceFactories: WithValidator {
    fn create_fetch_service(&self) -> FetchServiceBuilder;
    fn create_state_service(&self) -> StateServiceBuilder;
    fn create_json_connector(&self) -> Result<JsonRpSeeConnector, Error>;
    fn create_block_cache(&self) -> BlockCacheBuilder;
}
```

**Implemented by**: `ServiceTestManager`, `WalletTestManager`
**Key methods**:
- `create_fetch_service()` - Pre-configured FetchService builder
- `create_state_service()` - Pre-configured StateService builder
- `create_json_connector()` - Authenticated JSON RPC connector

### Configuration Traits

#### `ConfigurableBuilder`
**Purpose**: Common interface for all test manager builders.
**Location**: `manager/traits/config_builder.rs`

```rust
pub trait ConfigurableBuilder: Sized {
    type Manager;
    type Config: TestConfiguration;
    
    fn build_config(&self) -> Self::Config;
    async fn launch(self) -> Result<Self::Manager, Error>;
    
    // Standard builder methods
    fn validator(self, kind: ValidatorKind) -> Self;
    fn network(self, network: Network) -> Self;
    fn chain_cache(self, path: PathBuf) -> Self;
    
    // Convenience methods
    fn zebra(self) -> Self { self.validator(ValidatorKind::Zebra) }
    fn zcashd(self) -> Self { self.validator(ValidatorKind::Zcashd) }
    fn regtest(self) -> Self { self.network(Network::Regtest) }
    fn testnet(self) -> Self { self.network(Network::Testnet) }
}
```

#### `LaunchManager<M>`
**Purpose**: Generic trait for launching any manager from any compatible config.
**Location**: `manager/traits/config_builder.rs`

```rust
pub trait LaunchManager<M> {
    async fn launch_manager(self) -> Result<M, Error>;
}
```

## Manager Types

### ServiceTestManager
**Purpose**: For tests that need validator + manual service creation.
**File**: `manager/tests/service.rs`
**Traits**: `WithValidator`, `WithServiceFactories`

```rust
pub struct ServiceTestManager {
    local_net: LocalNet,
    ports: TestPorts,
    network: Network,
    chain_cache: Option<PathBuf>,
}

pub struct ServiceTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
}
```

**Usage Pattern**:
```rust
let manager = TestManagerBuilder::for_service_tests().await?;
let (service, subscriber) = manager.create_fetch_service()
    .with_sync(false)
    .build().await?;
```

### WalletTestManager
**Purpose**: For wallet integration tests requiring validator + indexer + clients.
**File**: `manager/tests/wallet.rs`
**Traits**: `WithValidator`, `WithClients`, `WithIndexer`, `WithServiceFactories`

```rust
pub struct WalletTestManager {
    local_net: LocalNet,
    ports: TestPorts,
    network: Network,
    indexer_config: IndexerConfig,
    indexer_handle: JoinHandle<Result<(), IndexerError>>,
    clients: Clients, // Always present, not Option!
}

pub struct WalletTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    chain_cache: Option<PathBuf>,
    // Wallet-specific options
}
```

**Usage Pattern**:
```rust
let manager = TestManagerBuilder::for_wallet_tests().await?;
manager.prepare_for_shielding(100).await?;
let recipient = manager.get_recipient_address("unified").await;
```

### JsonServerTestManager
**Purpose**: For JSON RPC server tests requiring validator + indexer + JSON server.
**File**: `manager/tests/json_server.rs`
**Traits**: `WithValidator`, `WithIndexer`, optionally `WithClients`

```rust
pub struct JsonServerTestManager {
    local_net: LocalNet,
    ports: TestPorts,
    network: Network,
    indexer_config: IndexerConfig,
    indexer_handle: JoinHandle<Result<(), IndexerError>>,
    json_server_cookie_dir: Option<PathBuf>,
    clients: Option<Clients>, // Optional for JSON server tests
}

pub struct JsonServerTestsBuilder {
    validator_kind: ValidatorKind,
    network: Network,
    enable_cookie_auth: bool,
    enable_clients: bool,
}
```

## Configuration System

Purpose-built configurations designed specifically for the trait-based manager system.

### Core Configuration

```rust
#[derive(Debug, Clone)]
pub struct TestConfig {
    pub network: Network,
    pub validator_kind: ValidatorKind,
    pub chain_cache: Option<PathBuf>,
}
```

### Scenario-Specific Configurations

#### ServiceTestConfig
```rust
#[derive(Debug, Clone)]
pub struct ServiceTestConfig {
    pub base: TestConfig,
    // No additional fields - just validator needed
}

impl LaunchManager<ServiceTestManager> for ServiceTestConfig {
    async fn launch_manager(self) -> Result<ServiceTestManager, Error> {
        // Launch validator only
    }
}
```

#### WalletTestConfig
```rust
#[derive(Debug, Clone)]
pub struct WalletTestConfig {
    pub base: TestConfig,
    pub indexer: IndexerConfig,
    pub enable_clients: bool,
}

impl LaunchManager<WalletTestManager> for WalletTestConfig {
    async fn launch_manager(self) -> Result<WalletTestManager, Error> {
        // Launch validator + indexer + clients
    }
}
```

#### JsonServerTestConfig
```rust
#[derive(Debug, Clone)]
pub struct JsonServerTestConfig {
    pub base: TestConfig,
    pub indexer: IndexerConfig,
    pub json_auth: JsonRpcAuth,
    pub enable_clients: bool,
}
```

## Service Factories

Service factories eliminate the 40+ line boilerplate for creating services by providing builders with sensible defaults.

**File**: `manager/factories.rs`

### FetchServiceBuilder
```rust
pub struct FetchServiceBuilder {
    validator_address: SocketAddr,
    network: Network,
    enable_sync: bool,
    enable_db: bool,
    auth_enabled: bool,
    data_dir: PathBuf,
}

impl FetchServiceBuilder {
    pub fn new(validator_address: SocketAddr, network: Network) -> Self;
    
    // Customization methods
    pub fn with_sync(mut self, enable: bool) -> Self;
    pub fn with_db(mut self, enable: bool) -> Self;
    pub fn with_auth(mut self, auth: AuthConfig) -> Self;
    
    // Build final service
    pub async fn build(self) -> Result<(FetchService, FetchServiceSubscriber), Error>;
}
```

**Usage**:
```rust
// Before (40+ lines of boilerplate)
let fetch_service = FetchService::spawn(FetchServiceConfig::new(
    test_manager.zebrad_rpc_listen_address,
    false, None, None, None, None, None, None, None,
    test_manager.local_net.data_dir().path().to_path_buf().join("zaino"),
    None,
    Network::new_regtest(/* 20 lines of activation heights */),
    true, true,
)).await.unwrap();

// After (3 lines)
let (fetch_service, subscriber) = manager.create_fetch_service()
    .with_sync(false)
    .build().await?;
```

### StateServiceBuilder
Similar pattern for StateService with zebra-state configuration defaults.

### BlockCacheBuilder
Similar pattern for BlockCache with performance-optimized defaults.

## Public API

**File**: `lib.rs`

### TestManagerBuilder Facade
```rust
pub struct TestManagerBuilder;

impl TestManagerBuilder {
    // Zero-config shortcuts
    pub async fn for_service_tests() -> Result<ServiceTestManager, Error> {
        ServiceTestsBuilder::default().launch().await
    }
    
    pub async fn for_wallet_tests() -> Result<WalletTestManager, Error> {
        WalletTestsBuilder::default().launch().await
    }
    
    pub async fn for_json_server_tests() -> Result<JsonServerTestManager, Error> {
        JsonServerTestsBuilder::default().launch().await
    }
    
    // Customizable builders
    pub fn service_tests() -> ServiceTestsBuilder {
        ServiceTestsBuilder::default()
    }
    
    pub fn wallet_tests() -> WalletTestsBuilder {
        WalletTestsBuilder::default()
    }
    
    pub fn json_server_tests() -> JsonServerTestsBuilder {
        JsonServerTestsBuilder::default()
    }
}
```

### Ergonomic Re-exports
```rust
// Manager types
pub use manager::tests::{
    service::{ServiceTestManager, ServiceTestsBuilder},
    wallet::{WalletTestManager, WalletTestsBuilder},
    json_server::{JsonServerTestManager, JsonServerTestsBuilder},
};

// Traits
pub use manager::traits::{
    WithValidator, WithClients, WithIndexer, WithServiceFactories
};

// Factories
pub use manager::factories::{
    FetchServiceBuilder, StateServiceBuilder, BlockCacheBuilder
};
```

## Implementation Stages

### Stage 1: Clean Slate (Critical First Step)
**Purpose**: Remove old code to avoid confusion during development.

1. **Delete current manager.rs entirely**
2. **Create empty directory structure**:
   ```bash
   mkdir -p zaino-testutils/src/manager/{traits,tests}
   ```
3. **Update lib.rs** to remove old TestManager exports
4. **Verify clean slate** - ensure compilation fails cleanly (no old code)

### Stage 2: Trait Infrastructure
**Purpose**: Build the foundation trait system.

1. **Create `manager/traits/with_validator.rs`**:
   - Core validator operations
   - Default implementations for block generation, waiting, cleanup
   - Address access methods

2. **Create `manager/traits/with_clients.rs`**:
   - Client access without Option unwrapping
   - Wallet workflow automation
   - Address generation helpers

3. **Create `manager/traits/with_indexer.rs`**:
   - Indexer configuration access
   - Service endpoint access
   - Authentication directory access

4. **Create `manager/traits/with_service_factories.rs`**:
   - Factory method signatures
   - Dependency on `WithValidator` trait

5. **Create `manager/traits/config_builder.rs`**:
   - `ConfigurableBuilder` trait
   - `LaunchManager` generic trait

**Testing**: Create simple stub implementations to verify trait compilation.

### Stage 3: Service Factories
**Purpose**: Eliminate service creation boilerplate.

1. **Create `manager/factories.rs`**:
   - `FetchServiceBuilder` with sensible defaults
   - `StateServiceBuilder` with zebra-state defaults
   - `BlockCacheBuilder` with performance defaults
   - `JsonRpSeeConnector` factory with authentication

**Key Requirements**:
- Pre-configure regtest network parameters
- Handle authentication automatically
- Provide builder pattern for customization
- Return service + subscriber tuples

**Testing**: Verify factories can create services equivalent to manual setup.

### Stage 4: Configuration System
**Purpose**: Replace old TestConfigBuilder with purpose-built configs.

1. **Create `config.rs`**:
   - `TestConfig` base configuration
   - `ServiceTestConfig`, `WalletTestConfig`, `JsonServerTestConfig`
   - `LaunchManager` implementations for each config type

**Key Requirements**:
- Each config type knows exactly what it can launch
- Clean separation of concerns
- Type-safe conversions

### Stage 5: Manager Implementations
**Purpose**: Implement the actual manager types.

1. **Create `manager/tests/service.rs`**:
   - `ServiceTestManager` struct
   - `ServiceTestsBuilder` struct
   - Trait implementations for `WithValidator`, `WithServiceFactories`
   - `ConfigurableBuilder` implementation

2. **Create `manager/tests/wallet.rs`**:
   - `WalletTestManager` struct (clients field is NOT Optional)
   - `WalletTestsBuilder` struct
   - All trait implementations
   - Wallet-specific builder methods

3. **Create `manager/tests/json_server.rs`**:
   - `JsonServerTestManager` struct
   - `JsonServerTestsBuilder` struct
   - JSON server-specific trait implementations

**Key Requirements**:
- No Optional fields where trait guarantees presence
- Domain-specific builder methods on each builder type
- Complete trait implementations with error handling

### Stage 6: Public API
**Purpose**: Provide ergonomic public interface.

1. **Update `lib.rs`**:
   - `TestManagerBuilder` facade
   - Clean re-exports
   - Zero-config shortcuts
   - Customizable builder access

**Testing**: Verify all usage patterns work as expected:
```rust
// Zero-config
let manager = TestManagerBuilder::for_wallet_tests().await?;

// Customized
let manager = TestManagerBuilder::wallet_tests().validator(Zcashd).launch().await?;

// Direct import
use zaino_testutils::{WalletTestManager, WithClients};
```

### Stage 7: Integration Test Migration
**Purpose**: Update all existing tests to use new API.

**Migration by test file**:
1. **wallet_to_validator.rs** → Use `WalletTestManager`
2. **fetch_service.rs** → Use `ServiceTestManager` with factories
3. **state_service.rs** → Use `ServiceTestManager` with factories
4. **json_server.rs** → Use `JsonServerTestManager`
5. **chain_cache.rs** → Use `ServiceTestManager` with custom configs
6. **local_cache.rs** → Use `ServiceTestManager` with factories

**Migration pattern**:
```rust
// Old
let test_manager = TestManager::launch(
    &ValidatorKind::Zebra, &BackendType::Fetch, None, None, 
    true, false, false, true, true, true
).await.unwrap();

// New
let manager = TestManagerBuilder::for_wallet_tests().await?;
```

## Migration Examples

### Wallet Test Migration

**Before**:
```rust
async fn send_to_orchard(validator: &ValidatorKind, backend: &BackendType) {
    let mut test_manager = TestManager::launch(
        validator, backend, None, None, true, false, false, true, true, true,
    ).await.unwrap();
    
    let mut clients = test_manager.clients.take().expect("Clients not initialized");
    
    clients.faucet.sync_and_await().await.unwrap();
    
    if matches!(validator, ValidatorKind::Zebrad) {
        test_manager.generate_blocks_with_delay(100).await;
        clients.faucet.sync_and_await().await.unwrap();
        clients.faucet.quick_shield().await.unwrap();
        test_manager.generate_blocks_with_delay(1).await;
        clients.faucet.sync_and_await().await.unwrap();
    }
    
    let recipient_ua = clients.get_recipient_address("unified").await;
    from_inputs::quick_send(&mut clients.faucet, vec![(&recipient_ua, 250_000, None)])
        .await.unwrap();
    
    test_manager.close().await;
}
```

**After**:
```rust
async fn send_to_orchard(validator: &ValidatorKind) {
    let manager = TestManagerBuilder::wallet_tests()
        .validator(*validator)
        .launch().await?;
    
    manager.prepare_for_shielding(100).await?;
    let recipient_ua = manager.get_recipient_address("unified").await;
    
    from_inputs::quick_send(manager.faucet(), vec![(&recipient_ua, 250_000, None)])
        .await?;
}
```

### Service Test Migration

**Before**:
```rust
async fn create_test_manager_and_fetch_service(
    validator: &ValidatorKind,
    chain_cache: Option<PathBuf>,
    enable_zaino: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> (TestManager, FetchService, FetchServiceSubscriber) {
    let test_manager = TestManager::launch(
        validator, &BackendType::Fetch, None, chain_cache,
        enable_zaino, false, false, zaino_no_sync, zaino_no_db, enable_clients,
    ).await.unwrap();

    let fetch_service = FetchService::spawn(FetchServiceConfig::new(
        test_manager.zebrad_rpc_listen_address,
        false, None, None, None, None, None, None, None,
        test_manager.local_net.data_dir().path().to_path_buf().join("zaino"),
        None,
        Network::new_regtest(/* 20 lines of activation heights */),
        true, true,
    )).await.unwrap();
    
    let subscriber = fetch_service.get_subscriber().inner();
    (test_manager, fetch_service, subscriber)
}
```

**After**:
```rust
async fn service_test_setup(validator: ValidatorKind) -> (ServiceTestManager, FetchService, FetchServiceSubscriber) {
    let manager = TestManagerBuilder::service_tests()
        .validator(validator)
        .launch().await?;
    
    let (fetch_service, subscriber) = manager.create_fetch_service()
        .with_sync(true)
        .with_db(true)
        .build().await?;
    
    (manager, fetch_service, subscriber)
}
```

## Testing Strategy

### Unit Testing

**Trait Testing**:
- Test each trait's default implementations
- Verify trait composition works correctly
- Test error handling in trait methods

**Factory Testing**:
- Verify factories produce equivalent services to manual setup
- Test customization options
- Test error conditions

**Builder Testing**:
- Test all builder combinations
- Verify invalid combinations are prevented
- Test configuration generation

### Integration Testing

**Manager Testing**:
- Test each manager type in isolation
- Verify trait implementations work correctly
- Test cleanup and error handling

**End-to-End Testing**:
- Migrate existing tests incrementally
- Verify new API provides equivalent functionality
- Test performance and resource usage

### Regression Testing

**Before Migration**:
- Document current test behavior and performance
- Create baseline measurements

**After Migration**:
- Verify all tests still pass
- Verify performance is maintained or improved
- Verify resource usage (memory, ports, etc.)

## Benefits Realization

### Immediate Benefits

1. **Boilerplate Reduction**: 50+ line service setups → 3-5 lines
2. **Type Safety**: Compile-time prevention of invalid operations
3. **Clear Intent**: Self-documenting test setup

### Long-term Benefits

1. **Extensibility**: Easy addition of new test scenarios
2. **Maintainability**: Changes isolated to specific manager types
3. **Observability**: Targeted metrics and logging per scenario
4. **Developer Experience**: Better IDE support, clearer error messages

### Metrics to Track

- Lines of code reduction in test files
- Time to write new tests
- Number of runtime errors vs compile-time errors
- Test execution time and resource usage

---

This implementation guide provides the complete technical roadmap for the TestManager redesign. Each stage builds on the previous one, ensuring a systematic and reliable implementation process.