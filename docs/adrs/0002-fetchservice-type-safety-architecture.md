# ADR-0002: FetchService Type Safety Architecture

## Status

**Ongoing Research** - August 2025

## Context

### Type Safety at Application Boundaries

Zaino's configuration system provides strong compile-time guarantees through its type-safe `BackendConfig` enum:

```rust
pub enum BackendConfig {
    LocalZebra { zebra_state: ZebraStateConfig, indexer_rpc_address: SocketAddr, ... },  
    RemoteZebra { rpc_address: SocketAddr, auth: ZebradAuth, ... },
    RemoteZcashd { rpc_address: SocketAddr, auth: ZcashdAuth, ... },
    RemoteZainod { rpc_address: SocketAddr, auth: ZcashdAuth, ... },
}
```

This design makes invalid configurations unrepresentable - you cannot accidentally create a "RemoteZebra with ZebraStateConfig" or other invalid combinations. The configuration layer successfully enforces correctness at compile time.

### The Translation Problem

However, when translating this type safety from the configuration layer to service implementations, we face architectural questions about how much granularity to preserve:

**Current Service Architecture:**
```rust
StateService    // Only for LocalZebra
FetchService    // For RemoteZebra, RemoteZcashd, RemoteZainod
```

**Current FetchService Implementation:**
```rust
pub struct FetchService {
    fetcher: JsonRpSeeConnector,        // Handles different validator types
    config: FetchServiceConfig,         // Contains full daemon context
    // ... other fields
}
```

### Core Architectural Question

**How do we best translate the type safety from our configuration layer down to service implementations?**

The configuration enum provides 4 distinct backend types, but we collapse them into 2 service types. Are we losing important type safety, or is this the right level of abstraction?

## Problem

### Current State Analysis

**Strengths of Current Approach:**
- `JsonRpSeeConnector` already abstracts different validator protocols and authentication
- Single `FetchService` implementation reduces code duplication
- Backend differences are handled at the network client layer where they belong

**Potential Issues:**
- Runtime dispatch required to handle validator-specific behaviors
- Type safety from config layer doesn't extend to service method signatures
- Behavioral differences between validators handled through runtime checks

### Fundamental Tension: Runtime vs Compile-Time

This problem exists at the **application edge** where user configuration (inherently runtime) meets our type system (compile-time). Configuration files, CLI arguments, and environment variables cannot be known at compile time, creating an inherent boundary where some runtime dispatch is unavoidable.

### Options Considered

## Option 1: Dedicated Service Types

Create specialized service implementations for each backend type:

```rust
LocalZebraService    // Direct state access, indexer RPC
RemoteZebraService   // Zebra JSON-RPC client, zebra-specific behaviors  
RemoteZcashdService  // Zcashd JSON-RPC client, zcashd-specific behaviors
RemoteZainodService  // Zainod JSON-RPC client, zainod-specific behaviors
```

**Pros:**
- Maximum type safety - each service type has methods specific to its capabilities
- Compile-time guarantees about available operations
- No runtime checks within service implementations
- Clear separation of validator-specific logic

**Cons:** 
- Significant code duplication across service implementations
- More complex maintenance - bugs need fixing in multiple places
- Over-engineering if behavioral differences are minimal
- Larger API surface area

## Option 2: Unified Service with Dependency Injection

Maintain single `FetchService` with injected dependencies for behavioral differences:

```rust
struct FetchService {
    client: Box<dyn JsonRpcClient>,     // Injected: ZebraClient vs ZcashdClient  
    validator_type: ValidatorType,      // Runtime enum for behavioral differences
    config: FetchServiceConfig,
}

enum ValidatorType {
    Zebra,
    Zcashd, 
    Zainod,
}

impl FetchService {
    async fn get_block(&self, height: u32) -> Result<Block, Error> {
        let raw_block = self.client.get_block(height).await?;
        
        // Handle behavioral differences only where they exist
        match self.validator_type {
            ValidatorType::Zebra => parse_zebra_block(raw_block),
            ValidatorType::Zcashd => parse_zcashd_block(raw_block), 
            ValidatorType::Zainod => parse_zainod_block(raw_block),
        }
    }
}
```

**Pros:**
- Shared implementation for common logic (95%+ of functionality)
- Dependency injection makes testing easier
- Runtime checks only for genuine behavioral differences
- Clean separation between connection logic and business logic

**Cons:**
- Runtime dispatch required for validator-specific behaviors
- Type safety doesn't extend to method signatures
- Potential for runtime errors if validator type doesn't match client

## Option 3: Current Approach (Status Quo)

Continue with existing `FetchService` + `JsonRpSeeConnector` architecture:

```rust
pub struct FetchService {
    fetcher: JsonRpSeeConnector,    // Already handles validator differences
    config: FetchServiceConfig,     // Full daemon context (needs refactoring)
    // ... other fields
}
```

**Pros:**
- `JsonRpSeeConnector` already abstracts validator differences effectively
- Minimal changes required
- Proven architecture that works

**Cons:**
- Config structure carries full daemon context (should be refactored)
- Less explicit about validator-specific behaviors
- Runtime checks hidden in connector layer

## Research Questions

### Behavioral Difference Analysis

**Key Question**: What are the actual behavioral differences between RemoteZebra, RemoteZcashd, and RemoteZainod at the service level?

**Connection-Level Differences** (handled by client layer):
- Authentication schemes (cookie vs password vs disabled)
- JSON-RPC endpoint URLs and protocols
- Request/response formats and error handling

**Service-Level Differences** (may need service-level handling):
- Available RPC methods and their parameters
- Response data structure variations
- Performance characteristics and timeout handling
- Error semantics and recovery patterns

**Research Needed**: Detailed analysis of whether behavioral differences justify separate service types or can be handled through dependency injection patterns.

### Type Safety Boundaries

**Key Question**: Where is the appropriate boundary between compile-time type safety and runtime dispatch?

Current boundaries:
- **Config Layer**: Compile-time type safety via enums ✅
- **Service Layer**: Runtime dispatch via connector ❓
- **Client Layer**: Runtime dispatch via protocol differences ✅

**Research Needed**: Analysis of whether moving the runtime boundary from service layer to client layer provides meaningful benefits.

### Testing and Maintainability

**Key Question**: Which approach provides the best balance of type safety, testability, and maintainability?

**Testing Considerations**:
- Dependency injection enables easier mocking and unit testing
- Separate service types require duplicated test infrastructure  
- Current approach relies on integration testing with real connectors

**Research Needed**: Evaluation of testing complexity and coverage across different architectures.

## Decision

**Status**: Under active research - no decision made yet.

This ADR documents the ongoing architectural research into the best practices for translating type safety from configuration layers to service implementations. The decision will be informed by:

1. **Behavioral difference analysis** - Detailed catalog of actual differences between validator types
2. **Testing approach evaluation** - Comparison of testability across different architectures  
3. **Maintainability assessment** - Long-term maintenance implications of each approach
4. **Performance considerations** - Runtime overhead of different dispatch mechanisms

## Rationale for Research Phase

### Why Not Rush to Implementation?

1. **Application Edge Complexity**: The boundary between configuration and services involves fundamental trade-offs between compile-time and runtime safety
2. **Multiple Valid Approaches**: Each option has legitimate advantages depending on actual behavioral differences
3. **Long-term Impact**: Service architecture decisions affect the entire codebase and are expensive to change
4. **Insufficient Data**: Need empirical analysis of validator behavioral differences before making architectural commitments

### Success Criteria for Research Phase

The research phase will be considered complete when we have:

1. **Behavioral Difference Catalog**: Documented analysis of actual differences between validator types at service level
2. **Testing Strategy Comparison**: Evaluated testability, maintainability, and coverage implications of each approach
3. **Performance Analysis**: Measured runtime overhead of different dispatch mechanisms
4. **Implementation Prototype**: Small-scale prototype demonstrating the chosen approach

## Consequences

### Positive

1. **Informed Decision Making**: Research phase prevents premature architectural commitments
2. **Multiple Options Preserved**: Keeping options open until we have sufficient data
3. **Focus on Real Problems**: Research will reveal whether type safety concerns are theoretical or practical

### Negative  

1. **Implementation Delay**: Research phase delays FetchService improvements
2. **Analysis Paralysis Risk**: Could over-analyze instead of making pragmatic decisions
3. **Resource Investment**: Research requires time that could be spent on other features

### Risks & Mitigations

**Risk**: Research phase extends indefinitely
**Mitigation**: Clear success criteria and timeline for research completion

**Risk**: Over-engineering solution for simple problem
**Mitigation**: Focus research on identifying simplest approach that meets actual requirements

**Risk**: Bikeshedding architectural choices
**Mitigation**: Ground research in empirical analysis of validator behavioral differences

## Next Steps

1. **Behavioral Analysis**: Catalog actual differences between RemoteZebra, RemoteZcashd, and RemoteZainod at service level
2. **Testing Evaluation**: Compare testing approaches across different architectures
3. **Prototype Development**: Implement small-scale examples of each approach
4. **Decision Documentation**: Update this ADR with final architectural decision based on research findings

This ADR serves as living documentation of our architectural thinking process, ensuring that the final FetchService design is based on empirical analysis rather than theoretical concerns.