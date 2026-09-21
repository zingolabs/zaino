# Zaino

Zaino is an indexer for the Zcash blockchain implemented in Rust.

Zaino provides all necessary functionality for "light" clients (wallets and other applications that don't rely on the complete history of blockchain) and "full" clients / wallets and block explorers providing access to both the finalized chain and the non-finalized best chain and mempool held by a Zebra full validator.

## Release pipeline

Zaino ships through a gated pipeline (`dev → rc → release-ready → stable`); the
gates, tags, and blessing flow are specified in the
[release decision record](./docs/release/pipeline.md).

| Gate | Workflow |
| ---- | -------- |
| Changeset check (`dev`-gate) | [![changeset-check](https://github.com/zingolabs/zaino/actions/workflows/changeset-check.yml/badge.svg)](https://github.com/zingolabs/zaino/actions/workflows/changeset-check.yml) |
| RC gate (nightly) | [![rc-gate](https://github.com/zingolabs/zaino/actions/workflows/rc-gate.yml/badge.svg)](https://github.com/zingolabs/zaino/actions/workflows/rc-gate.yml) |
| Blessing (release) | [![blessing](https://github.com/zingolabs/zaino/actions/workflows/blessing.yml/badge.svg)](https://github.com/zingolabs/zaino/actions/workflows/blessing.yml) |

Latest cycle tags:
[![latest RC](https://img.shields.io/github/v/tag/zingolabs/zaino?filter=cycle-*-rc.*&label=latest%20RC)](https://github.com/zingolabs/zaino/tags)
[![latest release](https://img.shields.io/github/v/tag/zingolabs/zaino?filter=cycle-*%2C!*-rc.*&label=latest%20release)](https://github.com/zingolabs/zaino/tags)

The "latest RC" badge shows the newest `cycle-<N>-rc.<M>` prerelease tag; "latest
release" filters those out to show the newest blessed `cycle-<N>` tag. (Both use
shields.io tag filtering; if a shields update ever stops distinguishing the two,
fall back to a single unfiltered `github/v/tag` "latest cycle tag" badge.)

### Motivations
With the ongoing legacy-full-node deprecation project, there is a push to transition to a modern, Rust-based software stack for the Zcash ecosystem. By implementing Zaino in Rust, we aim to modernize the codebase, enhance performance and improve overall security. This work will build on the foundations laid down by [Librustzcash](https://github.com/zcash/librustzcash) and [Zebra](https://github.com/ZcashFoundation/zebra), helping to ensure that the Zcash infrastructure remains robust and maintainable for the future.

Due to current potential data leaks / security weaknesses highlighted in [revised-nym-for-zcash-network-level-privacy](https://forum.zcashcommunity.com/t/revised-nym-for-zcash-network-level-privacy/46688) and [wallet-threat-model](https://zcash.readthedocs.io/en/master/rtd_pages/wallet_threat_model.html), there is a need to use anonymous transport protocols (such as Nym or Tor) to obfuscate clients' identities from Zcash's indexing servers ([Lightwalletd](https://github.com/zcash/lightwalletd), [the legacy Zcash full node](https://github.com/zcash/zcash), Zaino). As Nym has chosen Rust as their primary SDK ([Nym-SDK](https://github.com/nymtech/nym)), and Tor is currently implementing Rust support ([Arti](https://gitlab.torproject.org/tpo/core/arti)), Rust is a straightforward and well-suited choice for this software.

Zaino sources chain data from Zebra over its JSON-RPC interface and builds its own indexes from the blocks it fetches, so Zaino and Zebra can run on separate hardware and new indices can be offered without changes to the validator.

Separation of validation and indexing functionality serves several purposes. First, by removing indexing functionality from the Validator (Zebra) will lead to a smaller and more maintainable codebase. Second, by moving all indexing functionality away from Zebra into Zaino will unify this paradigm and simplify Zcash's security model. Separating these concerns (consensus node and blockchain indexing) serves to create a clear trust boundary between the Indexer and Validator allowing the Indexer to take on this responsibility. Historically, this had been the case for "light" clients/wallets using [Lightwalletd](https://github.com/zcash/lightwalletd) as opposed to "full-node" client/wallets and block explorers that were directly served by the [the legacy Zcash full node](https://github.com/zcash/zcash).

### Goals

Our primary goal with Zaino is to serve all non-miner clients -such as wallets and block explorers- in a manner that prioritizes security and privacy while also ensuring the time efficiency critical to a stable currency. We are committed to ensuring that these clients can access all necessary blockchain data and services without exposing sensitive information or being vulnerable to attacks. By implementing robust security measures and privacy protections, Zaino will enable users to interact with the Zcash network confidently and securely.

To facilitate a smooth transition for existing users and developers, Zaino is designed (where possible) to maintain backward compatibility with Lightwalletd and the legacy Zcash full node. This means that applications and services currently relying on these platforms can switch to Zaino with minimal adjustments. By providing compatible APIs and interfaces, we aim to reduce friction in adoption and ensure that the broader Zcash ecosystem can benefit from Zaino's enhancements without significant rewrites or learning curves.

### Scope
Zaino will implement a comprehensive RPC API to serve all non-miner client requests effectively. This API will encompass all functionality currently in the LightWallet gRPC service ([CompactTxStreamer](https://github.com/zcash/librustzcash/blob/main/zcash_client_backend/proto/service.proto)), currently served by Lightwalletd, and a subset of the [Zcash RPCs](https://zcash.github.io/rpc/) required by wallets and block explorers, currently served by the legacy Zcash full node. Zaino will unify these two RPC services and provide a single, straightforward interface for Zcash clients and service providers to access the data and services they require.

In addition to the RPC API, Zaino will offer a client library allowing developers to integrate Zaino's functionality directly into their Rust applications, without the overhead of using an RPC protocol, while Zebra stays insulated from directly interfacing with client software.

## Project Structure

```
packages/                          Cargo workspace member crates
  zainod/                            Daemon binary: config → runtime boot
  # runtime and supervision
  zaino-runtime/                     Orchestra: boots and supervises the components
  zaino-component/                   Supervised subsystems: lifecycle, health, and tasks
  zaino-async/                       Named, panic-rendering tasks
  zaino-logging/                     Tracing setup and panic hook
  zaino-status/                      How a component reports whether it is working
  # serving
  zaino-service/                     Inner driving surface: the capability traits clients consume
  zaino-core/                        Vocabulary of that surface
  zaino-lightserve/                  Lightwalletd-compatible gRPC server
  zaino-noderpc/                     Node JSON-RPC adapter (not yet served by zainod)
  zaino-wallet/                      Embedded-wallet facade (not yet wired)
  zaino-store-service/               Engine: finalised store ⊕ chain head behind one service
  zaino-chainview/                   The composed FS ⊕ NFS snapshot over a watermark seam
  zaino-store/                       Finalised reads: compose-on-read over the KV backend
  # indexing
  zaino-indexer/                     Sync driver + concurrent source-backed block provisioner
  zaino-sync/                        DAG-driven parallel index sync engine
  zaino-indexes/                     Index definitions and the index sets zainod builds
  zaino-persistence/                 Storage backend port
  zaino-persistence-codec/           Typed, versioned entries over the backend
  zaino-persistence-macros/          Derive for on-disk record layouts
  zaino-backend-lmdb/                LMDB backend
  zaino-chain-head/                  Non-finalised chain head: vocabulary and ports
  zaino-chain-head-service/          Non-finalised chain head: the runtime
  # validator source
  zaino-source/                      Driven ports: one trait per chain question
  zaino-source-macros/               Derives the resilient ports from their one-shot twins
  zaino-source-zebra-rpc/            Zebra JSON-RPC adapter: the only validator source
  zaino-rpc/                         JSON-RPC client transport (no parsing)
  # vocabulary
  zaino-primitives/                  Chain-level domain types
  zaino-consensus/                   Consensus constants and protocol limits
  zaino-proto/                       Lightwallet protocol buffers
  # not yet wired into zainod
  zaino-mempool/                     Mempool domain types and ports
  zaino-mempool-service/             The mempool runtime: poll loop, read handles, coherence
  zaino-address/                     Zcash address classification

bench/sync-bench/                  Sync throughput bench over the production pipeline

live-tests/                        Live-test suite — standalone workspace, run on the ztest k8s harness
  e2e/                               End-to-end partition (wallet client -> Zaino -> validator)
  clientless/                        Clientless partition (Zaino services -> live validator, no client)
  zaino-testutils/                   Shared test harness and utilities

docs/                              Architecture diagrams, specs, and usage guides
tools/                             Development tools
  workbench/                         Repo guards run by `makers lint`
.github/                           CI workflows and issue templates
.githooks/                         Git hooks (pre-push)

Cargo.toml                         Top-level workspace manifest
Cargo.lock                         Resolved dependency graph (committed)
Makefile.toml                      cargo-make task definitions
rust-toolchain.toml                Pinned Rust toolchain
deny.toml                          cargo-deny policy (licenses, advisories)

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

## Network exposure

zainod serves one interface: the lightwalletd-compatible gRPC server
(`[serve] grpc_listen_address`), in plaintext. zainod links no TLS stack. Expose
it beyond a trusted network only behind a proxy that terminates TLS. The
validator connection is plain HTTP JSON-RPC to `[source] jsonrpc_address`.

## Running tests

Production-crate tests run on your host; the live suites run on a Kubernetes
cluster via [`ztest`](https://crates.io/crates/ztest_cli):

```sh
cargo nextest run                          # packages/*, no live validator
cd live-tests && ztest run -p clientless -p e2e   # both live partitions
```

The live suites need the `ztest` CLI and a registered cluster
(`cargo install ztest_cli`, then `kind create cluster` and `ztest cluster
setup`) — see [docs/testing.md](./docs/testing.md) for the full setup.

On lower-resource machines you may hit occasional contention flakes under full
parallelism — re-run, or lower `--test-threads`.

## Documentation

- [Use Cases](./docs/use_cases.md): Holds instructions and example use cases.
- [Testing](./docs/testing.md): Holds instructions for running tests.
- [Live-test guidelines](./live-tests/CLAUDE.md): The rules for writing live tests — the live oracle, QoS tiers, parameterization.
- [Docker](./docs/docker.md): Running zainod in a container.
- [RPC API](./docs/rpc_api.md): The gRPC methods zainod serves today.
- [Cargo Docs](https://zingolabs.github.io/zaino/): Holds a full code specification for Zaino.

### Architecture Decision Records
Decisions that shape the codebase, with the reasoning that produced them. Read
these before changing the structure they describe. Every record lives in
[zingolabs/zingo-adrs](https://github.com/zingolabs/zingo-adrs). This
repository checks in only a submodule pointer to it at `docs/adr/`, so the
directory is empty until you materialise it, and zaino's own records then sit
under `docs/adr/zaino/`. Propose a record in zingo-adrs, never here.

```sh
# materialise the records after cloning
git submodule update --init docs/adr

# advance the pointer to the current dev of zingo-adrs, then commit
git submodule update --remote docs/adr
```

Records a newcomer needs first:
- [ADR-0007](./docs/adr/zaino/0007-block-persistence-is-a-row-set-boundary.md): block persistence is a row-set boundary.
- [ADR-0008](./docs/adr/zaino/0008-source-ports-and-domain-primitives.md): validator access is a set of single-question ports over domain primitives.
- [ADR-0010](./docs/adr/zaino/0010-mempool-subsystem-separation.md): the mempool subsystem is separated into `zaino-mempool` behind ports.
- [ADR-0011](./docs/adr/zaino/0011-chain-head-subsystem-separation.md): the non-finalised chain head is a self-synchronising subsystem.

### Crate usage guides
Practical guidance for working *in* a crate — its scope, its invariants, and the
mistakes its design is trying to prevent.
- [`zaino-status`](./packages/zaino-status/usage.md): the status vocabulary, and why it stays vocabulary.
- [`zaino-async`](./packages/zaino-async/usage.md): the low-level async/tokio primitives (named, panic-rendering `Task`) the component layer is built on.
- [`zaino-logging`](./packages/zaino-logging/usage.md): centralized tracing setup, and the panic-at-origin hook that routes panics through the same sink.
- [`zaino-component`](./packages/zaino-component/usage.md): the component abstraction, its two independent axes, and the observed/owned line.
- [`zaino-consensus`](./packages/zaino-consensus/usage.md): the protocol constants, and why they are stated rather than borrowed.
- [`zaino-primitives`](./packages/zaino-primitives/usage.md): the domain vocabulary, and why it depends on nothing.
- [`zaino-persistence`](./packages/zaino-persistence/usage.md): the storage backend port, and why index code never names a concrete store.
- [`zaino-source`](./packages/zaino-source/usage.md): the ports, the domain/fetch error split, and `Resilient`.
- [`zaino-rpc`](./packages/zaino-rpc/usage.md): JSON-RPC transport, and what it deliberately does not do.
- [`zaino-source-zebra-rpc`](./packages/zaino-source-zebra-rpc/usage.md): the validator source — connecting, tip polling, and error classification.
- [`zaino-address`](./packages/zaino-address/usage.md): address classification, and what is not classified.
- [`zaino-mempool`](./packages/zaino-mempool/usage.md): the two-layer model, the ports, and the bounds.
- [`zaino-mempool-service`](./packages/zaino-mempool-service/usage.md): spawning and consuming the mempool.
- [`zaino-chain-head`](./packages/zaino-chain-head/usage.md): the chain head's ports, why reads live on the snapshot, and why there is no way to make it synchronise.
- [`zaino-chain-head-service`](./packages/zaino-chain-head-service/usage.md): the chain head runtime, its two testing styles, and the properties to keep when editing the advance path.
- [`zaino-chainview`](./packages/zaino-chainview/usage.md): composes the finalised store and the non-finalised view into one served compact-block snapshot over a watermark-governed seam, and why the initial-build gap is an explicit policy knob.


## Security Vulnerability Disclosure

If you believe you have discovered a security issue, and it is time sensitive, please contact us online on Matrix. See our [CONTRIBUTING.md document](./CONTRIBUTING.md) for contact points.
Otherwise you can send an email to:
zingodisclosure@proton.me

## License

This project is licensed under the [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0). See the [LICENSE](./LICENSE) file for details.
