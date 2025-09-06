# 1. Modular Block and Transaction Parsing Architecture

Date: 2025-09-06

## Status

Accepted

## Context

Zaino's original block and transaction parsing system used a monolithic approach where parsing logic was tightly coupled with data structures, making it difficult to:

1. **Add new transaction versions** - Each new version required significant changes across the codebase
2. **Maintain type safety** - Field reading and validation were ad-hoc, leading to potential parsing errors
3. **Debug parsing issues** - Monolithic parsers made it hard to isolate which specific field or validation was failing
4. **Ensure protocol compliance** - No enforcement of field reading order as specified by Zcash protocol
5. **Optimize performance** - Limited ability to implement version-specific optimizations or partial parsing

The old system in `zaino-fetch/src/chain/` consisted of large, monolithic files (`block.rs` at 738+ lines, `transaction.rs` at 1197+ lines) that mixed parsing logic, data structures, and validation in ways that violated separation of concerns.

With Zcash continuing to evolve (V5 transactions, future protocol upgrades), we needed an architecture that could:
- Cleanly support multiple transaction versions (V1, V4, future V2/V3/V5)
- Provide compile-time guarantees about parsing correctness
- Enable incremental adoption of new features
- Maintain high performance for block indexing operations

## Decision

We implement a **modular parsing architecture** based on the Reader Pattern with the following key components:

### 1. Field-Based Parsing System

Replace monolithic parsing with discrete, composable field parsers:

```rust
// Trait system for type-safe field parsing
pub trait BlockField: Sized {
    type Value;
    const SIZE: BlockFieldSize;
    fn read_from_cursor(cursor: &mut Cursor<&[u8]>) -> Result<Self::Value, ParseError>;
    fn validate(value: &Self::Value, context: &BlockParsingContext) -> Result<(), ParseError>;
}
```

Each field (version, timestamps, hashes, etc.) becomes a separate, testable unit with clear size specifications and validation logic.

### 2. Reader Pattern with Position Enforcement

Implement `BlockFieldReader` and `FieldReader` that enforce protocol-specified field ordering:

```rust
pub fn read_field<F: BlockField>(&mut self, expected_position: usize) -> Result<F::Value, ParseError> {
    if self.position != expected_position {
        return Err(ParseError::FieldOrderViolation { /* ... */ });
    }
    // Parse, validate, and track field
}
```

This provides **compile-time ordering guarantees** and detailed error reporting.

### 3. Version Dispatching Architecture

Create a `TransactionDispatcher` that:
- Peeks transaction version without consuming data
- Routes to version-specific parsers (`TransactionV1Reader`, `TransactionV4Reader`)
- Provides unified interface through `Transaction` enum
- Enables easy addition of future versions

### 4. Context-Driven Validation

Separate parsing context from transaction data:
- `BlockParsingContext` - Network, height, transaction metadata
- `TransactionContext` - Block context, transaction index, TXID  
- `BlockContext` - Activation heights, network parameters

This enables **context-aware validation** during parsing rather than post-parsing checks.

### 5. Module Structure Reorganization

Restructure `zaino-fetch/src/chain/` into focused modules:

```
chain/
├── block/
│   ├── reader.rs      # Field reading infrastructure
│   ├── parser.rs      # Block header/full block parsers
│   ├── fields.rs      # Individual block field implementations
│   └── context.rs     # Block parsing context
├── transaction/
│   ├── reader.rs      # Transaction field reading
│   ├── dispatcher.rs  # Version-agnostic routing
│   ├── fields.rs      # Transaction field implementations
│   └── versions/
│       ├── v1.rs      # V1 transaction implementation
│       └── v4.rs      # V4 transaction implementation
├── types.rs           # Common type definitions
└── error.rs           # Comprehensive error types
```

### 6. Comprehensive Error Handling

Introduce structured error types:
- `FieldOrderViolation` - Field reading order violations
- `FieldSizeMismatch` - Size validation failures
- `UnsupportedVersion` - Clean version handling
- `InvalidData` - Content validation errors

## Consequences

### What Becomes Easier

1. **Adding New Transaction Versions** - Implement `TransactionVersionReader` trait and add to dispatcher
2. **Debugging Parsing Issues** - Field-level errors with exact position and context information
3. **Testing Parsing Logic** - Each field parser is independently testable
4. **Protocol Compliance** - Enforced field ordering prevents protocol violations
5. **Performance Optimization** - Version-specific optimizations and partial parsing capabilities
6. **Code Maintenance** - Clear separation of concerns and focused modules
7. **Documentation** - Self-documenting field specifications and validation rules

### What Becomes More Difficult

1. **Initial Complexity** - More files and abstractions to understand initially
2. **Boilerplate Code** - Each new field requires trait implementation
3. **Compilation Time** - Additional generic code may increase compile times
4. **Memory Usage** - Context tracking adds small runtime overhead

### Risks and Mitigations

**Risk: Performance Regression**
- *Mitigation*: Benchmarking showed negligible impact due to compile-time optimizations
- *Mitigation*: Position tracking uses simple integers, not expensive data structures

**Risk: Developer Learning Curve**  
- *Mitigation*: Comprehensive documentation and examples for common patterns
- *Mitigation*: Gradual migration allows learning while maintaining existing functionality

**Risk: Over-Engineering**
- *Mitigation*: Architecture validated against real Zcash protocol requirements
- *Mitigation*: Future transaction versions (V2, V3, V5) confirm the design's necessity

### Migration Impact

- **Backward Compatibility**: Maintained through wrapper APIs for existing consumers
- **Integration Points**: Updated `zaino-state` backends to use new parsing API
- **Testing**: Enhanced test coverage with field-level and integration tests
- **Build System**: Updated for new module structure

This architecture provides a robust foundation for supporting all current and future Zcash transaction versions while maintaining type safety, performance, and developer productivity.
