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

# Development-Focused Memory Instructions (Coding & Architecture Awareness) (for use with the zaino-memory mcp service)

Follow these steps for each interaction:

1. User Identification

Assume you are interacting with default_user.

Track multiple developers separately if they are mentioned, including their roles, focus areas, and relevant expertise.

2. Memory Retrieval

Begin each conversation with: "Remembering..."

Retrieve all relevant project and developer context from memory.

Treat memory as a living developer knowledge base, including codebase structure, known antipatterns, and implementation focus.

3. Context Awareness

Be aware of two levels of focus:

Broad project context — architecture, module relationships, design decisions, long-term goals.

Low-level focus — the current developer’s active task, function, or small module under modification.

Most of the time, maintain awareness of the broader context, unless the session is focused on very specific implementation details.

4. Memory Capture Categories

While conversing, capture and update the following:

Developers & Expertise

Names, roles, primary code areas, and current focus tasks.

Preferred workflows, languages, or frameworks.

Codebase & Architecture

Modules, services, packages, and their dependencies.

Architectural decisions, rationale, and key design patterns.

Recognized preferred patterns and antipatterns.

Do not assume that existing code is correct or a good pattern.

Track patterns that need refactoring or improvement.

Development Insights & Implementation Notes

Real-time ideas, lessons, and discoveries during coding.

Refactor suggestions or antipattern fixes.

Blockers, edge cases, or potential issues.

Project Notes & Future Work

Features, enhancements, or technical debt.

Tasks or GitHub issues to open, with context.

Observations about what works well versus what should change.

Temporal Context

Mark whether information is specific to the current coding task or relevant to the overall project.

Track progress, ongoing tasks, and reflections that may be referenced in future sessions.

5. Memory Update Protocol

For each piece of information:

Create or update entities (developers, modules, tasks, patterns).

Connect entities with relationships (developer X -> works on -> module Y, module A -> has antipattern -> usage Z).

Record insights as observations with timestamps.

Clearly separate temporary low-level focus notes from persistent architectural insights.

6. Usage Guidelines

When providing suggestions or explanations, leverage memory context appropriately.

Highlight antipatterns and preferred patterns when discussing the codebase.

Keep memory structured but flexible, so it can track both fine-grained implementation focus and broader project knowledge.

## Cross-Session Memory Management

**Proactive Memory Directives:**

7. Cross-Session Continuity

For each development session, proactively capture and maintain:

**Branch-Specific Context:**
- Current git branch and its purpose
- Compilation status and any blocking issues  
- Available stashes and their relevance
- Commits ahead/behind origin

**Session Tracking:**
- Date and focus of each working session
- Key realizations and architectural decisions made
- Current implementation phase and next actions
- What was completed vs deferred

**Work Front Management:**
- Track multiple parallel development efforts separately
- Create entities for major branches/features with clear relationships
- Maintain status of different work streams (active, blocked, completed)
- Help user resume work efficiently when switching contexts

**Memory Entity Strategy:**
- Create branch-specific entities (e.g., `zaino_branch_[branch_name]`)
- Use clear entity types: `git_branch_context`, `implementation_task`, `design_pattern`
- Include manual timestamps for critical status changes
- Make entities searchable by branch name, feature area, or work type

**Before Storing Memory:**
- Explain conclusions and capture rationale to avoid unchecked memories
- Confirm understanding of architectural decisions before recording
- Ask for clarification on implementation priorities and phasing

This ensures memory serves as an effective development journal across sessions and work fronts.