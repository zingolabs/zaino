# Zaino
Zaino is an indexer for the Zcash blockchain implemented in Rust.

Zaino provides all necessary functionality for "light" clients (wallets and other applications that don't rely on the complete history of blockchain) and "full" clients / wallets and block explorers providing access to both the finalized chain and the non-finalized best chain and mempool held by either a Zebra or Zcashd full validator.


### Motivations
With the ongoing Zcashd deprecation project, there is a push to transition to a modern, Rust-based software stack for the Zcash ecosystem. By implementing Zaino in Rust, we aim to modernize the codebase, enhance performance and improve overall security. This work will build on the foundations laid down by [Librustzcash](https://github.com/zcash/librustzcash) and [Zebra](https://github.com/ZcashFoundation/zebra), helping to ensure that the Zcash infrastructure remains robust and maintainable for the future.

Due to current potential data leaks / security weaknesses highlighted in [revised-nym-for-zcash-network-level-privacy](https://forum.zcashcommunity.com/t/revised-nym-for-zcash-network-level-privacy/46688) and [wallet-threat-model](https://zcash.readthedocs.io/en/master/rtd_pages/wallet_threat_model.html), there is a need to use anonymous transport protocols (such as Nym or Tor) to obfuscate clients' identities from Zcash's indexing servers ([Lightwalletd](https://github.com/zcash/lightwalletd), [Zcashd](https://github.com/zcash/zcash), Zaino). As Nym has chosen Rust as their primary SDK ([Nym-SDK](https://github.com/nymtech/nym)), and Tor is currently implementing Rust support ([Arti](https://gitlab.torproject.org/tpo/core/arti)), Rust is a straightforward and well-suited choice for this software.

Zebra has been designed to allow direct read access to the finalized state and RPC access to the non-finalized state through its ReadStateService. Integrating directly with this service enables efficient access to chain data and allows new indices to be offered with minimal development.

Separation of validation and indexing functionality serves several purposes. First, by removing indexing functionality from the Validator (Zebra) will lead to a smaller and more maintainable codebase. Second, by moving all indexing functionality away from Zebra into Zaino will unify this paradigm and simplify Zcash's security model. Separating these concerns (consensus node and blockchain indexing) serves to create a clear trust boundary between the Indexer and Validator allowing the Indexer to take on this responsibility. Historically, this had been the case for "light" clients/wallets using [Lightwalletd](https://github.com/zcash/lightwalletd) as opposed to "full-node" client/wallets and block explorers that were directly served by the [Zcashd full node](https://github.com/zcash/zcash).


### Goals
Our primary goal with Zaino is to serve all non-miner clients -such as wallets and block explorers- in a manner that prioritizes security and privacy while also ensuring the time efficiency critical to a stable currency. We are committed to ensuring that these clients can access all necessary blockchain data and services without exposing sensitive information or being vulnerable to attacks. By implementing robust security measures and privacy protections, Zaino will enable users to interact with the Zcash network confidently and securely.

To facilitate a smooth transition for existing users and developers, Zaino is designed (where possible) to maintain backward compatibility with Lightwalletd and Zcashd. This means that applications and services currently relying on these platforms can switch to Zaino with minimal adjustments. By providing compatible APIs and interfaces, we aim to reduce friction in adoption and ensure that the broader Zcash ecosystem can benefit from Zaino's enhancements without significant rewrites or learning curves.

### Scope
Zaino will implement a comprehensive RPC API to serve all non-miner client requests effectively. This API will encompass all functionality currently in the LightWallet gRPC service ([CompactTxStreamer](https://github.com/zcash/librustzcash/blob/main/zcash_client_backend/proto/service.proto)), currently served by Lightwalletd, and a subset of the [Zcash RPCs](https://zcash.github.io/rpc/) required by wallets and block explorers, currently served by Zcashd. Zaino will unify these two RPC services and provide a single, straightforward interface for Zcash clients and service providers to access the data and services they require.

In addition to the RPC API, Zaino will offer a client library allowing developers to integrate Zaino's functionality directly into their Rust applications. Along with the RemoteReadStateService mentioned below, this will allow both local and remote access to the data and services provided by Zaino without the overhead of using an RPC protocol, and also allows Zebra to stay insulated from directly interfacing with client software.

Currently Zebra's `ReadStateService` only enables direct access to chain data (both Zebra and any process interfacing with the `ReadStateService` must be running on the same hardware). Zaino will extend this functionality, using a Hyper wrapper, to allow Zebra and Zaino (or software built using Zaino's `IndexerStateService` as its backend) to run on different hardware and should enable a much greater range of deployment strategies (eg. running validator, indexer or wallet processes on separate hardware). It should be noted that this will primarily be designed as a remote link between Zebra and Zaino and it is not intended for developers to directly interface with this service, but instead to use functionality exposed by the client library in Zaino (`IndexerStateService`).


## Project Structure

```
packages/                          Cargo workspace member crates, in dependency order
  zaino-status/                      How a component reports whether it is working
  zaino-consensus/                   Zcash consensus constants and protocol limits
  zaino-primitives/                  Domain vocabulary (thiserror only; no serde)
  zaino-address/                     Zcash address classification
  zaino-source/                      Driven ports: one trait per chain question
  zaino-rpc/                         JSON-RPC transport (no parsing)
  zaino-convert-zebra/               zebra-chain -> domain conversions
  zaino-source-zebra-rpc/            JSON-RPC adapter + response parsing
  zaino-source-zebra-readstate/      Zebra ReadStateService adapter
  zaino-source-zebra/                ZebraValidator composite + routing
  zaino-mempool/                     Mempool domain types and ports (no node library)
  zaino-mempool-service/             The mempool runtime: poll loop, read handles, coherence
  zaino-common/                      Shared utilities and configuration
  zaino-proto/                       Protocol buffer definitions
  zaino-state/                       Chain state and indexer service library
  zaino-serve/                       gRPC + JSON-RPC servers, and the served JSON schema
  zainod/                            Daemon binary

live-tests/                        Live-test suite — root-workspace members, run against zcashd/zebrad
  e2e/                               End-to-end partition (wallet client -> Zaino -> validator)
  clientless/                        Clientless partition (Zaino services -> live validator, no client)
  zaino-testutils/                   Shared test harness and utilities
  test_binaries/                     Symlinked zcashd/zebrad/zcash-cli binaries
  test_environment/                  Container build context
    Containerfile                      CI/test container image definition
    entrypoint.sh                      Container entrypoint (binary symlink setup)
    test-container-permissions.sh      Container permission / volume-mount tests

docs/                              Architecture diagrams, specs, and usage guides
tools/                             Development tools, shell helpers, makefiles
  scripts/                           Shell scripts (CI tag computation, helpers, lints)
  makefiles/                         cargo-make task definitions (lints, rocksdb, notify)
.github/                           CI workflows and issue templates
.githooks/                         Git hooks (pre-push)
.config/containers.conf            Rootless podman defaults (userns, security)

Cargo.toml                         Top-level workspace manifest
Cargo.lock                         Resolved dependency graph (committed)
Makefile.toml                      cargo-make task definitions
rust-toolchain.toml                Pinned Rust toolchain
deny.toml                          cargo-deny policy (licenses, advisories)
.env.testing-artifacts             Version pins for test container (Rust, zcashd, zebrad)

Dockerfile                         Production container image
entrypoint.sh                      Production container entrypoint
.dockerignore                      Docker build context exclusions

README.md                          This file
CHANGELOG.md                       Release notes
CLAUDE.md                          AI-contributor guidelines
CONTRIBUTING.md                    Human-contributor guide
LICENSE                            Apache-2.0 license text
.gitignore                         Git ignore patterns
```

## Server network exposure

Zaino exposes two servers, with different defaults reflecting their transport
security:

- **gRPC** (`[grpc_settings]`): may bind to a public address only when TLS is
  configured (`[grpc_settings.tls]` with `cert_path` / `key_path`). Binding to a
  non-private address without TLS is rejected at startup. The
  `no_tls_use_unencrypted_traffic` build feature disables this enforcement (and
  logs a startup warning) — for testing or trusted networks only.
- **JSON-RPC** (`[json_server_settings]`): has **no transport encryption** and
  is intended for loopback or trusted private networks only. By default it may
  bind only to private/loopback addresses (RFC1918, IPv6 ULA, or loopback);
  public or unspecified (`0.0.0.0` / `::`) bind addresses are rejected at
  startup. The `allow_unencrypted_public_json_rpc_bind` build feature lifts this
  restriction (and logs a startup warning) for deployments on trusted private
  networks where encryption is handled externally (e.g. containers behind a
  service mesh or proxy that terminates TLS).

**Security implication:** the JSON-RPC interface transmits unencrypted traffic.
Do not expose it to untrusted networks, and only enable
`allow_unencrypted_public_json_rpc_bind` when an external layer secures the
connection.

## Running tests

The test suites run inside a **podman** container via `makers` (cargo-make):

```sh
makers test            # packages/* tests that need no live validator (default)
makers test live       # both live partitions (clientless + e2e) + combined summary
makers test all        # everything: packages then live
```

zcashd-backed tests are **off by default**; add `--with-zcashd` to include them
(there is no implicit or env-var path — see docs/adr/0005). On lower-resource machines you
may hit occasional contention flakes under full parallelism — re-run, or lower
`test-threads` in the nextest config. See [docs/testing.md](./docs/testing.md)
for full instructions.

## Documentation
- [Use Cases](./docs/use_cases.md): Holds instructions and example use cases.
- [Testing](./docs/testing.md): Holds instructions for running tests.
- [Live Service System Architecture](./docs/zaino_live_system_architecture.pdf): Holds the Zcash system architecture diagram for the Zaino live service.
- [Library System Architecture](./docs/zaino_lib_system_architecture.pdf): Holds the Zcash system architecture diagram for the Zaino client library.
- [ZainoD (Live Service) Internal Architecture](./docs/zaino_serve_architecture_v020.pdf): Holds an internal Zaino system architecture diagram.
- [Zaino-State (Library) Internal Architecture](./docs/zaino_state_architecture_v020.pdf): Holds an internal Zaino system architecture diagram.
- [Internal Specification](./docs/internal_spec.md): Holds a specification for Zaino and its crates, detailing their functionality, interfaces and dependencies.
- [RPC API Spec](./docs/rpc_api.md): Holds a full specification of all of the RPC services served by Zaino.
- [Cargo Docs](https://zingolabs.github.io/zaino/): Holds a full code specification for Zaino.

### Architecture Decision Records
Decisions that shape the codebase, with the reasoning that produced them. Read
these before changing the structure they describe.
- [ADR-0001](./docs/adr/0001-zcashd-support-feature-gate.md) / [ADR-0005](./docs/adr/0005-zcashd-support-default-off.md): the `zcashd_support` feature gate, and why it is opt-in.
- [ADR-0002](./docs/adr/0002-live-tests-rejoin-root-workspace.md), [ADR-0003](./docs/adr/0003-live-test-taxonomy-and-two-crate-split.md), [ADR-0004](./docs/adr/0004-rename-integration-partition-to-clientless.md): the live-test suite's workspace membership, taxonomy and naming.
- [ADR-0006](./docs/adr/0006-aws-lc-rs-preferred-crypto-provider.md): aws-lc-rs as the preferred rustls CryptoProvider.
- [ADR-0007](./docs/adr/0007-block-persistence-is-a-row-set-boundary.md): block persistence is a row-set boundary.
- [ADR-0008](./docs/adr/0008-source-ports-and-domain-primitives.md): validator access is a set of single-question ports over domain primitives.
- [ADR-0009](./docs/adr/0009-served-json-schema-lives-in-zaino-serve.md): the served JSON schema lives in `zaino-serve`.

### Crate usage guides
Practical guidance for working *in* a crate — its scope, its invariants, and the
mistakes its design is trying to prevent.
- [`zaino-status`](./packages/zaino-status/usage.md): the status vocabulary, and why it stays vocabulary.
- [`zaino-consensus`](./packages/zaino-consensus/usage.md): the protocol constants, and why they are stated rather than borrowed.
- [`zaino-primitives`](./packages/zaino-primitives/usage.md): the domain vocabulary, and why it depends on nothing.
- [`zaino-source`](./packages/zaino-source/usage.md): the ports, the domain/fetch error split, and `Resilient`.
- [`zaino-rpc`](./packages/zaino-rpc/usage.md): JSON-RPC transport, and what it deliberately does not do.
- [`zaino-convert-zebra`](./packages/zaino-convert-zebra/usage.md): `zebra-chain` → domain conversions.
- [`zaino-source-zebra-rpc`](./packages/zaino-source-zebra-rpc/usage.md): the JSON-RPC adapter and its error classification.
- [`zaino-source-zebra-readstate`](./packages/zaino-source-zebra-readstate/usage.md): the read-state adapter, and what it deliberately cannot answer.
- [`zaino-source-zebra`](./packages/zaino-source-zebra/usage.md): the composite and its three routing rules.
- [`zaino-address`](./packages/zaino-address/usage.md): address classification, and what is not classified.
- [`zaino-mempool`](./packages/zaino-mempool/usage.md): the two-layer model, the ports, and the bounds.
- [`zaino-mempool-service`](./packages/zaino-mempool-service/usage.md): spawning and consuming the mempool.


## Security Vulnerability Disclosure
If you believe you have discovered a security issue, and it is time sensitive, please contact us online on Matrix. See our [CONTRIBUTING.md document](./CONTRIBUTING.md) for contact points.
Otherwise you can send an email to:
zingodisclosure@proton.me


## License
This project is licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0). See the [LICENSE](./LICENSE) file for details.
