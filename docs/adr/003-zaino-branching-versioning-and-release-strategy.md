# ADR: Branching, Versioning, Documentation, Public Interfaces, and Release Strategy

## Status

superseded by [zaino/0016](zaino/0016-changeset-derived-release-pipeline.md)

## Context

Zaino is a Rust workspace providing an indexing service and APIs for Zcash clients. ([Zaino Github](https://github.com/zingolabs/zaino))

We need a predictable policy for:
1) branching and CI gates,
2) versioning semantics and when to bump,
3) when/how docs are published (GitHub Pages + crates.io / docs.rs),
4) which public interfaces are governed by these rules, and
5) releases (tags, changelog, crates.io publication, and docs publication).

## Decision

### 1) Branch / development strategy

**Branches**
- `dev`: primary development branch (default branch).
- `stable`: release branch (only release-quality changes land here).

**PR targeting rules**
- PRs may target `dev` directly.
- PRs may target `stable` **only if they are merges from `dev`** (i.e., *no feature branches directly into stable*).

**Review rules**
- Merge into `dev`: **1 approval** from CODEOWNERS.
- Merge into `stable`: **2 approvals** from CODEOWNERS.

**CI / test execution rules**
- PRs into `dev`: run a **fast test set** (unit tests where available, small subset of integration tests included while unit tests are missing).
- Nightly on `dev`: run the **full test suite**.
- PRs into `stable` (i.e., `dev` → `stable` release PRs): run the **full test suite**.

**Dependency rules**
- All non-test dependencies must be crates.io imports on stable.
- Dev may temporarily use feature branches via `[patch.crates-io]`.

**Rationale**
- `dev` optimizes for iteration speed while preserving baseline correctness.
- `stable` optimizes for downstream reliability and release discipline.

### 2) Versioning strategy (SemVer) and what it means in Zaino

Zaino follows **Semantic Versioning (SemVer)**: `MAJOR.MINOR.PATCH`. ([semver.org](https://semver.org/spec/v2.0.0.html))

**Scope choice**
- Zaino versions are treated as **crate-specific**  meaning each publishable crates in this repository will have an individual version number which will be bumped when changes to that repo necessitate it.

**Definitions for Zaino**
- **MAJOR**: any *backward-incompatible* change to a governed public interface (see “Public interfaces” section), including:
  - breaking changes to gRPC service behavior/requests/responses,
  - removing or changing semantics/signatures of public Rust items intended for external users,
  - breaking configuration/CLI contract for `zainod` where it impacts operators in a non-compatible way.
- **MINOR**: backward-compatible feature additions, including:
  - new RPC endpoints/services added without breaking existing ones,
  - new fields added in a backward-compatible way (where supported by the protocol/encoding),
  - new public Rust APIs that do not break old ones.
- **PATCH**: backward-compatible bug fixes, performance fixes, and internal refactors with no externally observable contract change.

**ZainoDB versioning Note**
- ZainoDB uses a separate versioning policy to the Zaino crates:
  - **MAJOR**: Distinct database implementations, providing differing sets of functionality (Currently V1 is the only supported major version. A lightweight V2 database that only holds the minimal set of data required to produce the extra indexes (compared to zebrad) required in Zaino is planned but not yet implemented. V0 is a backwards compatibility layer for the legacy local cache implementation).
  - **MINOR**: Updates that contain changes to either the public APIs or the on disk schema.
  - **PATCH**: Internal bug fixes / performance improvements that do not touch the public APIs or on disk schema.
- Due to this, version changes in ZainoDB may not dictate a change of the same type at the library level.

**Pre-1.0 note**
- While Zaino remains in the 0.y.z phase, version bumps will be treated as one level “less critical” than post-1.0.0. Specifically, changes that would normally require a major bump will instead require a minor bump, and changes that would normally require a minor bump will instead require a patch bump. Patch bumps keep the same meaning as post-1.0.0.

### 3) GitHub Pages + crates.io documentation update strategy

**Docs targets**
- **GitHub Pages (gh-pages)**: the canonical “workspace documentation” site ([gh-pages](https://zingolabs.org/zaino/), [gh-pages branch](https://github.com/zingolabs/zaino/tree/gh-pages?tab=readme-ov-file)).
- **docs.rs (crates.io)**: Rust API docs are automatically built for crates published to crates.io. ([docs.rs](https://docs.rs/about/builds))

**Update rules**
- Every time `stable` is updated as part of a release (and crates.io is updated), **GitHub Pages MUST be updated** to match that release state.
- docs.rs updates automatically when crates are published to crates.io.

**Possible Implementation**
- Use a GitHub Actions Pages deployment (e.g., GitHub’s `actions/deploy-pages`) as part of the release workflow so the site always reflects `stable` HEAD (or the release tag). ([github.com](https://github.com/actions/deploy-pages))

### 4) Changelog policy

Zaino maintains curated changelogs to record notable, user-impacting changes in a consistent way across the workspace and its crates. A changelog is a curated, chronologically ordered list of notable changes for each version. ([Keep a Changelog](https://keepachangelog.com/en/1.0.0/))

**Changelog locations**
- **Workspace changelog:** one primary changelog for the repository/workspace (covers cross-cutting changes and release-level summaries).
- **Per-crate changelogs:** each publishable crate maintains its own changelog for crate-specific changes.
- **ZainoDB changelog:** ZainoDB maintains an additional database-specific changelog, following the ZainoDB versioning policy defined in this ADR (separate from the crate/workspace SemVer policy).

**What must be recorded**
- Any change to a governed **public interface** (as defined in this ADR) must be recorded in:
  - the **workspace changelog**, and
  - the **relevant crate’s changelog**.
- Any change that affects the **ZainoDB on-disk schema** or database behaviour covered by the ZainoDB versioning policy must be recorded in the **ZainoDB changelog**, and does not necessarily imply a crate/workspace version bump of the same type.

**Release alignment**
- Changelog entries are written to communicate impact to users/operators and must align with the SemVer intent described in this ADR. ([Semantic Versioning](https://semver.org/spec/v2.0.0.html))

### 5) Public interfaces governed by this ADR (and officially supported in zaino)

This section defines the “compatibility surface” that drives SemVer bumps and stable-branch gatekeeping.

#### Included crates (governed)

##### `zainod` (daemon)
Public interfaces:
- Zainod daemon: Main indexing daemon
  - Zcash JsonRPC service
  - Zcash LightClient gRPC service

Public items:
- CLI arguments
- Config format
- RPC Specs

##### `zainodlib` (daemon library)
Public interfaces:
- `indexer::Indexer`: Full indexing server

Public items:
- `config::*`
- `error::*`

##### `zaino_serve` (gRPC + JsonRPC servers)
Public interfaces:
- `server::{grpc::TonicServer, jsonrpc::JsonRpcServer}`: gRPC / JsonRPC server implementations


Public items:
- `rpc::{GrpcClient, JsonRpcClient}`
- `rpc::jsonrpc::service::ZcashIndexerRpc`
- `server::config::*`
- `server::error::*`

##### `zaino_state` (Core indexing library)
Public interfaces:
- `chain_index::source::ValidatorConnector`: Validator agnostic Chain data fetch service
- `chain_index::{NodeBackedChainIndex, NodeBackedChainIndexSubscriber}`: Core chain indexing service
- `backends::{fetch::{FetchService, FetchServiceSubscriber}, state::{StateService, StateServiceSubscriber}}`: Indexing API (IndexerService / IndexerSubscriber) based on the zcash RPC services for compatibility, utilising Zaino's underlying indexing services

Public items:
- `indexer::{IndexerService, ZcashService, IndexerSubscriber, ZcashIndexer, LightWalletIndexer, LightWalletService}`
- `chain_index::{ChainIndex, NonFinalizedSnapshot}`
- `chain_index::source::{BlockchainSource, State, BlockchainSourceResult}`
- `chain_index::encoding::*`
- `chain_index::types::*`
- `status::*`
- `stream::*`
- `config::*`
- `error::*`
- ZainoDB's on disk schema.

##### `zaino_fetch` (Zcash specific JsonRPC client + block / transaction parsing logic)
Public interfaces:
- `jsonrpc::connector::JsonRpcConnector`: Zcash specific JsonRPC client with full chain data fetch and block / transaction parsing capability

Public items:
- `chain::utils::ParseFromSlice`
- `chain::transaction::*`
- `chain::block::*`
- `chain::error::*`
- `jsonrpc::connector::test_node_and_return_url`
- `jsonrpc::response::*`
- `jsonrpc::error::*`

##### `zaino_proto` (LightClient protocol implementation + utility methods / types)
Public items:
- `::*`

##### `zaino_common` (Common types used by other crates + utility methods)
Public items:
- `::*`

#### Excluded crates (not governed)
- `zaino-testvectors`
- `zaino-testutils`
- `integration-tests`

These may change freely without affecting SemVer, except where they force changes to governed public crates.

**Note** The codebase does not currently reflect this in some places, with entities that should be private currently publicised (or error / config types in the wrong locations). Where this is the case issues / PRs should be opened to provide fixes (make entities pub(crate) or move to the correct location), or a subsequest ADR opened to update the public interface officially maintained.

### 6) Release strategy

A “release” is a coordinated update of:
1) `stable` branch,
2) version numbers for publishable crates/crates.io publication,
3) GitHub Pages documentation publication,
4) a Git tag + release notes,
5) A zainod image published to a container repository (currently dockerhub)

**Release prerequisites**
- `dev` is green on the **full test suite** (nightly run or equivalent).
- Release PR is opened as **`dev` → `stable`**.
- Release PR passes **full test suite**.
- Release PR receives **2 CODEOWNER approvals**.

**Release steps**
0. **Prepare branch for release** Decide on a release candidate commit which is ready for release
   - TODO: Establish more explicit patterns for creating/validating release candidates. 
1. **Bump versions** (workspace-wide) according to SemVer rules.
2. **Update CHANGELOG / release notes** summarizing:
   - breaking changes (if any),
   - features,
   - fixes,
   - operator notes (config/CLI changes).
3. Merge `dev` → `stable` and then release from `stable`.
4. **Tag the release**
5. **Publish crates to crates.io**
   - docs.rs will then build and host per-version API docs automatically.
6. **Update GitHub Pages**
   - Deploy via GitHub Actions Pages tooling (currently unimplemnted meaning manual update will be necessary).
7. **Build and publish container images**.
   - Images MUST be tagged with the release version (`vMAJOR.MINOR.PATCH`) and SHOULD also be tagged with the Git commit SHA (immutable identifier).

**Cadence**
- Stable updated on version bumps, and crates.io release updated accordingly: if there is no version bump, there is no `stable` update.
- A stable release schedule should be set in a later ADR but may not be helpful at this stage of development.

## Consequences

**Benefits**
- Predictable quality gates for production users/operators.
- Clear SemVer meaning for downstream consumers.
- Docs are always aligned with what is actually released.
- The public API surface that drives compatibility is explicit and reviewable.

**Costs**
- Slightly more process around releases (release PRs, extra approvals, full test suite gating).
- Requires maintaining a “fast test set” vs “full suite” split and nightly CI plumbing.

## Actions

- Define what exactly constitutes the **fast test set** (e.g., a dedicated `cargo nextest run -E <expression>` profile) and encode it in CI.
- Ensure CODEOWNERS is configured so approvals map correctly to “1 for dev / 2 for stable”.
- Add stable branch and set PR / release protocols.
- Update the zaino repo docs to specify branching, versioning and release strategy laid out in this ADR.
- Add a release workflow checklist in `docs/` that mirrors “Release steps”.
- Update the zaino repo docs to specify the oficially supported public interfaces.
- Update public interfaces in the codebase (and documentation) to follow the public interfaces set out in this file.
- Define the process of creating and testing release candidates.

**Issues should be opened in relevant repos for each action listed here once this ADR is confirmed.**
