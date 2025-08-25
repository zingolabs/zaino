# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Zaino is a Rust-based blockchain indexer for Zcash, designed to replace Lightwalletd and serve all non-miner clients. It provides efficient access to both finalized and non-finalized blockchain data through gRPC and JSON-RPC APIs.

## Architecture

### Core Modules

- **zainod**: Main executable service that runs the indexer with gRPC/JSON-RPC APIs
- **zaino-state**: Core indexing library with configurable backends (FetchService for Zcashd RPC, StateService for Zebra ReadStateService)
- **zaino-serve**: gRPC/JSON-RPC server implementation for CompactTxStreamer and Zcash RPC services
- **zaino-fetch**: JSON-RPC client for legacy Zcashd compatibility
- **zaino-proto**: Protocol buffer definitions for gRPC services
- **zaino-testutils**: Testing utilities and test management
- **integration-tests**: End-to-end integration tests

### Key Backend Types

- **FetchService**: JSON-RPC based backend for Zcashd compatibility
- **StateService**: Zebra ReadStateService backend for efficient chain access
- **Chain Index**: New modular parsing architecture with encoding traits

## Development Commands

### Building and Testing

```bash
# Build all crates
cargo build

# Format code
makers fmt

# Run lints (clippy + docs)
makers clippy
makers doc

# Run all lints (fmt + clippy + docs)
makers lint

# Run integration tests in Docker environment
makers container-test

# Run specific test package
cargo test --package zaino-testutils

# Run single test
cargo test --package zaino-testutils --lib launch_testmanager::zcashd::basic

# List all available tests
cargo nextest list

# Validate test targets match CI
makers validate-test-targets

# Update CI test targets
makers update-test-targets
```

### Development Environment

The project uses Docker containers for integration testing with external dependencies (zcashd, zebra). Test binaries are managed in `test_binaries/bins/` and environment configuration is in `.env.testing-artifacts`.

### Build System

- Uses `cargo-make` (makers) for task automation
- Main tasks defined in `Makefile.toml` with linting tasks in `makefiles/lints.toml`
- Docker-based testing environment defined in `test_environment/`
- Rust toolchain: stable with rustfmt and clippy components

## Code Patterns

### Error Handling
- Uses `thiserror` for error types
- Backend-specific errors: `FetchServiceError`, `StateServiceError`

### Async/Concurrency  
- Built on `tokio` runtime with `async-trait` for async traits
- Uses `crossbeam-channel` and `dashmap` for concurrent data structures

### Database/Storage
- LMDB for persistent storage
- In-memory caching with `dashmap` and custom cache structures

### Parsing Architecture
- Recent modular parsing implementation with `ParseFromHex` trait
- Block and transaction parsing separated into discrete modules
- Uses encoding traits for serialization/deserialization

## Important Notes

- Current branch: `feat/modular-block-transaction-parsing` - recent parsing architecture changes
- All code forbids `unsafe_code` and requires missing docs warnings
- Uses specific Zebra git revisions (not crates.io) for latest features
- Test environment requires Docker for full integration testing
- Git hooks available in `.githooks/` - use `makers toggle-hooks` to enable
