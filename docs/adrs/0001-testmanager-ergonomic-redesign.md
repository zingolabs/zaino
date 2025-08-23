# ADR-0001: TestManager Ergonomic Architecture Redesign

## Status

**In development** - August 2025

## Context

### Current State (dev branch @ bf42ec8)

The current TestManager in the `dev` branch (commit `bf42ec8`) uses a monolithic approach with a single struct containing mixed concerns:

```rust
pub struct TestManager {
    pub local_net: LocalNet,
    pub data_dir: PathBuf,
    pub network: Network,
    pub zebrad_rpc_listen_address: SocketAddr,
    pub zebrad_grpc_listen_address: SocketAddr,
    pub zaino_handle: Option<tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>>,
    pub zaino_json_rpc_listen_address: Option<SocketAddr>,
    pub zaino_grpc_listen_address: Option<SocketAddr>,
    pub json_server_cookie_dir: Option<PathBuf>,
    pub clients: Option<Clients>,
}
```

This TestManager is launched via a complex 10-parameter function:

```rust
TestManager::launch(
    validator: &ValidatorKind,
    backend: &BackendType, 
    network: Option<services::network::Network>,
    chain_cache: Option<PathBuf>,
    enable_zaino: bool,
    enable_zaino_jsonrpc_server: bool,
    enable_zaino_jsonrpc_server_cookie_auth: bool,
    zaino_no_sync: bool,
    zaino_no_db: bool,
    enable_clients: bool,
) -> Result<TestManager, std::io::Error>
```

### Current HEAD State (refactor/zaino-state_config @ 9b970e0)

The current HEAD (commit `9b970e0`) represents a failed iteration that attempted to improve the dev TestManager by introducing `IndexerConfig`, `TestConfigBuilder`, and more complex configuration structs. While this iteration identified correct problems (boilerplate, unclear intent, mixed concerns), the solution was too complex and created an impedance mismatch between the old monolithic design and new configuration system.

**Key lessons learned from this failed iteration:**
- Builder pattern approach is correct, but needs to be purpose-built for the target architecture
- Configuration complexity increased rather than decreased 
- Mixed concerns problem persisted - single TestManager still had optional fields for everything
- No type safety improvements - still possible to call wallet methods on service-only setups

Our new design builds on these lessons by taking the builder pattern concept but applying it to specialized managers rather than trying to retrofit the monolithic TestManager.

### Problems Identified

#### 1. **Mixed Concerns**
- Single struct contains validator, indexer, and client fields regardless of test needs
- Optional fields everywhere (`clients`, `zaino_handle`, `zaino_json_rpc_listen_address`) because not all tests need them
- No clear separation between validator operations, indexer operations, and client operations

#### 2. **Integration Test Boilerplate**
Integration tests contain massive amounts of repeated boilerplate:

```rust
// Repeated across every service test (40+ lines)
let fetch_service = FetchService::spawn(FetchServiceConfig::new(
    test_manager.zebrad_rpc_listen_address,
    false, None, None, None, None, None, None, None,
    test_manager.local_net.data_dir().path().to_path_buf().join("zaino"),
    None,
    Network::new_regtest(/* 20 lines of hardcoded activation heights */),
    true, true,
)).await.unwrap();
```

#### 3. **Unclear Intent**
Tests use cryptic 10-parameter launch calls that obscure their actual purpose:
- `TestManager::launch(&ValidatorKind::Zebra, &BackendType::Fetch, None, None, true, false, false, true, true, true)` - unclear what this configuration does
- `test_manager.clients.take().expect()` - boilerplate in every wallet test
- Integration tests have ad-hoc `create_test_manager_and_X()` helper functions that really represent different test scenarios

#### 4. **No Type Safety**
- Can call wallet methods on service-only managers (runtime panics)
- No compile-time prevention of invalid combinations
- Optional field unwrapping throughout test code

#### 5. **Observability Limitations**
- Monolithic structure makes it hard to add targeted metrics/tracing
- No clear separation of concerns for error logging
- Difficult to instrument different test scenarios differently

## Decision

Complete architectural redesign replacing the monolithic TestManager with a **trait-based system** using **specialized managers** for different test scenarios.

### Core Architecture

#### **Specialized Test Managers**
- `ServiceTestManager` - Validator + service creation factories
- `WalletTestManager` - Validator + indexer + clients (always available)
- `JsonServerTestManager` - Validator + indexer + JSON server + optional clients

#### **Trait System**
- `WithValidator` - Core validator operations (all managers implement)
- `WithClients` - Wallet operations (wallet + JSON server managers)
- `WithIndexer` - Indexer state access (wallet + JSON server managers)  
- `WithServiceFactories` - Service creation helpers

#### **Purpose-Built Configuration**
Replace `TestConfigBuilder` with scenario-specific configs:
- `ServiceTestConfig` - Just validator configuration
- `WalletTestConfig` - Validator + indexer + client configuration
- `JsonServerTestConfig` - Validator + indexer + JSON server configuration

#### **Ergonomic API**
```rust
// Zero-config shortcuts
let manager = TestManagerBuilder::for_wallet_tests().await?;

// Customizable builders  
let manager = TestManagerBuilder::wallet_tests()
    .validator(ValidatorKind::Zcashd)
    .testnet()
    .launch().await?;
```

### File Organization
```
zaino-testutils/src/
├── manager/
│   ├── traits/
│   │   ├── with_validator.rs
│   │   ├── with_clients.rs
│   │   ├── with_indexer.rs
│   │   ├── with_service_factories.rs
│   │   └── config_builder.rs
│   ├── tests/
│   │   ├── service.rs        // ServiceTestManager + ServiceTestsBuilder
│   │   ├── wallet.rs         // WalletTestManager + WalletTestsBuilder
│   │   └── json_server.rs    // JsonServerTestManager + JsonServerTestsBuilder
│   └── factories.rs          // Service creation helpers
├── config.rs                 // Purpose-built configuration types
└── lib.rs                    // TestManagerBuilder facade + re-exports
```

## Rationale

### Why Specialized Managers Over Monolithic?

1. **Type Safety**: Compile-time guarantee that wallet methods are only called on managers with clients
2. **Clear Intent**: `TestManagerBuilder::for_wallet_tests()` immediately communicates purpose
3. **No Irrelevant Fields**: Each manager contains exactly what that test scenario needs
4. **Better Observability**: Can instrument each manager type with scenario-specific metrics and logging

### Why Trait Composition?

1. **Flexibility**: Managers implement only the traits they need
2. **Generic Functions**: Can write functions that work with any `WithValidator` manager
3. **Extensibility**: Easy to add new capabilities without breaking existing managers
4. **Clear Contracts**: Traits define exact capabilities available

### Why Not Enhance Existing 10-Parameter API?

1. **Parameter Explosion**: Already at 10 parameters, would grow worse with new test scenarios
2. **Poor Discoverability**: Hard to understand what each boolean parameter does
3. **No Type Safety**: All combinations accepted at compile time, invalid ones fail at runtime
4. **Maintenance Burden**: Every new test scenario requires modifying the central launch function

### Why Clean Break Over Backward Compatibility?

1. **Lessons from Failed Iteration**: Current HEAD shows that retrofitting the monolithic approach doesn't work
2. **Config Struct Changes**: Integration tests already broken due to config refactoring in current HEAD
3. **No Legacy Baggage**: Clean architecture without compromise, informed by previous attempt
4. **Better End State**: Purpose-built system vs adapted legacy system

## Important Caveats Considered

### **Legacy Compatibility**
**Considered**: Maintaining backward compatibility with existing TestManager API
**Decision**: Clean break because config structs already changed, making integration tests incompatible
**Trade-off**: Requires updating all tests, but results in much cleaner architecture

### **TestConfigBuilder Reuse**
**Considered**: Adapting the TestConfigBuilder approach from current HEAD to work with new managers
**Decision**: Replace with purpose-built configs designed specifically for specialized managers
**Lessons from Failed Iteration**: Current HEAD's TestConfigBuilder showed that retrofitting builder patterns onto monolithic design increases rather than decreases complexity
**Trade-off**: More initial work, but eliminates impedance mismatch that plagued the previous attempt

### **Monolithic vs Specialized**
**Considered**: Single TestManager with all optional fields vs multiple specialized managers
**Decision**: Multiple specialized managers with trait composition
**Trade-off**: More types to maintain, but much better type safety and clarity

### **Custom Manager Support**
**Considered**: How to handle edge cases that don't fit standard patterns
**Decision**: Architecture allows future `CustomTestManager` without redesign, but focus on 90% use cases first
**Trade-off**: Some edge cases may be harder initially, but architecture is extensible

### **API Flexibility Levels**
**Considered**: How much control to give test writers
**Decision**: Progressive disclosure - simple cases simple, complex cases possible through multiple API levels
**Trade-off**: More API surface, but accommodates both simple and advanced use cases

## Consequences

### **Positive**

1. **Massive Boilerplate Reduction**
   - Service creation: 50+ lines → 3-5 lines
   - Wallet test setup: 20+ lines → 1-3 lines
   - No more `create_test_manager_and_X()` helper functions

2. **Type Safety**
   - Compile-time prevention of calling wallet methods on service managers
   - No more `Option.expect()` panics in test code
   - Clear contracts about what operations are available

3. **Clear Intent**
   - `TestManagerBuilder::for_wallet_tests()` vs cryptic 10-parameter calls
   - Self-documenting API that shows test purpose

4. **Better Observability**
   - Each manager type can have targeted metrics collection
   - Scenario-specific error logging and tracing
   - Clear separation allows focused instrumentation
   - Trait-based approach enables cross-cutting concerns (logging, metrics) without code duplication

5. **Extensibility**
   - Easy to add new test manager types
   - Trait system allows new capabilities without breaking changes
   - Service factory pattern eliminates repeated configuration code

6. **Developer Experience**
   - IDE autocomplete shows only relevant methods for each manager type
   - Generic functions can work with any manager implementing required traits
   - Progressive API levels accommodate both simple and complex test needs

### **Negative**

1. **Migration Required**
   - All integration tests need updating to new API
   - Learning curve for developers familiar with old system

2. **More Types**
   - Multiple manager types instead of single TestManager
   - More files and traits to understand initially

3. **Potential Over-Engineering**
   - Complex architecture for what might be simple test orchestration
   - Risk of premature abstraction

### **Risks & Mitigations**

**Risk**: Architecture too complex for simple use cases
**Mitigation**: Zero-config shortcuts provide simple API, complexity is opt-in

**Risk**: Difficult to handle edge cases
**Mitigation**: Architecture designed to allow future `CustomTestManager`, escape hatches through direct component access

**Risk**: Integration test migration effort
**Mitigation**: Staged implementation allows incremental migration, new API is more maintainable long-term

## Implementation Strategy

1. **Clean Slate**: Remove current manager.rs entirely before implementing new system
2. **Traits First**: Build trait infrastructure before manager implementations  
3. **Service Factories**: Eliminate repeated boilerplate before building managers
4. **Staged Migration**: Implement managers incrementally, update tests in batches
5. **Documentation**: Provide migration examples for common test patterns

This ADR represents a significant architectural improvement that prioritizes type safety, developer experience, and maintainability over backward compatibility, with the understanding that the migration effort will pay dividends in reduced test maintenance and improved clarity of test intent.
