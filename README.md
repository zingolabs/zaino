# Zaino

Zaino is a Zcash indexer written in Rust. `zainod` fetches blocks from a
[Zebra](https://github.com/ZcashFoundation/zebra) node over its JSON-RPC
interface, builds its own indexes from them, and serves light wallets over the
lightwalletd-compatible `CompactTxStreamer` gRPC service.

Validation stays in Zebra and indexing moves to Zaino: the two run as separate
processes, on separate hosts if you like, and a new index needs no change to the
validator. [Where data lives](./docs/design/boundaries.md) states the rule.

## Release pipeline

Zaino ships through a gated pipeline (`dev → rc → release-ready → stable`),
specified in [docs/release/pipeline.md](./docs/release/pipeline.md).

| Gate | Workflow |
| ---- | -------- |
| Changeset check (`dev`-gate) | [![changeset-check](https://github.com/zingolabs/zaino/actions/workflows/changeset-check.yml/badge.svg)](https://github.com/zingolabs/zaino/actions/workflows/changeset-check.yml) |
| RC gate (nightly) | [![rc-gate](https://github.com/zingolabs/zaino/actions/workflows/rc-gate.yml/badge.svg)](https://github.com/zingolabs/zaino/actions/workflows/rc-gate.yml) |
| Blessing (release) | [![blessing](https://github.com/zingolabs/zaino/actions/workflows/blessing.yml/badge.svg)](https://github.com/zingolabs/zaino/actions/workflows/blessing.yml) |

[![latest RC](https://img.shields.io/github/v/tag/zingolabs/zaino?filter=cycle-*-rc.*&label=latest%20RC)](https://github.com/zingolabs/zaino/tags)
[![latest release](https://img.shields.io/github/v/tag/zingolabs/zaino?filter=cycle-*%2C!*-rc.*&label=latest%20release)](https://github.com/zingolabs/zaino/tags)

## Project structure

```
packages/                          Cargo workspace members
  zainod/                            Daemon binary: config → boot (one task per stage), logging, verify
  # serving
  zaino-grpc/                        Lightwalletd-compatible gRPC: router + validator fallback
  zaino-chainview/                   One view over N validators: quorum tip, quorum mempool, broadcast
  # indexing
  zaino-sync/                        Sync pipeline: producer → BlockSink → per-index follower
  zaino-index-compact-block/         CompactBlockIndex: framed records in append-only files
  zaino-internal-block-hash-to-height/  BlockHashIndex: hash ↔ height, the by-hash locator
  zaino-index-tree-state/            TreeStateIndex: commitment-tree frontiers and subtree roots
  zaino-index-transparent-address/   TransparentAddressIndex: receives and spends as sorted segments
  zaino-persistence/                 Storage core: sorted segments, verify report types
  zaino-chain-head/                  Non-final window of the best chain, in memory: reorg replay
  # validator source
  zaino-source/                      Driven ports + the Zebra JSON-RPC adapter, block decode, fetch pool
  # vocabulary
  zaino-primitives/                  Chain-level domain types and protocol constants
  zaino-proto/                       Lightwallet protocol buffers

live-tests/                        Standalone workspace, run on the ztest Kubernetes harness
  clientless/                        Zaino against a live validator, no wallet
  e2e/                               Wallet → Zaino → validator
  zaino-testutils/                   Shared test utilities
  sync/                              Mainnet sync profiles (own workspace, `ztest sync`)

docs/                              Operator docs, design notes, release process
tools/workbench/                   Repo guards run by `makers lint`
scripts/                           ztest chain-fixture producer
nix/, flake.nix                    Nix build and dev shell
.github/                           CI workflows and issue templates
.githooks/                         pre-push hook (`makers lint`)
.changesets/                       Pending release changesets

Makefile.toml                      cargo-make tasks
relman.toml                        Release targets (governed crate set)
rust-toolchain.toml                Pinned Rust toolchain
deny.toml                          cargo-deny policy
Dockerfile                         Container image
CONTEXT.md                         Glossary of canonical terms
CLAUDE.md, AGENTS.md               AI-contributor guidelines
CONTRIBUTING.md                    Human-contributor guide
```

## Network exposure

zainod serves one interface, the gRPC server on `[serve] grpc_listen_address`,
in plaintext; it links no TLS stack. Expose it beyond a trusted network only
behind a TLS-terminating proxy. The validator connection is plain HTTP JSON-RPC
to `[source] jsonrpc_address`.

The optional admin listener (`metrics_endpoint`, feature `prometheus`) serves
`/metrics` and `/livez` without authentication or encryption. It publishes the
chain tip, sync progress, request volumes and process memory. zainod warns at
startup when it binds a non-private address; restrict it to loopback, a private
interface, or the scraper's network. See [`zainod`'s guide](./packages/zainod/usage.md).

## Container image

`Dockerfile`: `rust:<pin>` build → `debian-slim` runtime, non-root `container_user`.

| Build arg        | Values                                    | Default   |
| ---------------- | ----------------------------------------- | --------- |
| `CARGO_FEATURES` | comma-separated, e.g. `prometheus` | empty (default set) |
| `CARGO_PROFILE`  | `release`, `profiling` (+ line tables & frame pointers, for sampling profilers) | `release` |

```sh
docker build -t zainod --build-arg CARGO_FEATURES=prometheus .
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling --bin zainod  # local profiling build
```

## Running tests

```sh
cargo nextest run --workspace                     # packages/*, no validator
cd live-tests && ztest run -p clientless -p e2e   # live suites, on a ztest cluster
```

Cluster setup and the sync profiles: [docs/testing.md](./docs/testing.md).

## Documentation

Operating it:
- [Running zainod](./docs/running.md): install, configure, a local Zebra + Zaino pair.
- [RPC API](./docs/rpc_api.md): the methods zainod serves, where each is answered from, and the status codes a client must tell apart.
- [Docker](./docs/docker.md): the container image.
- [Testing](./docs/testing.md): unit tests, live tests, sync profiles, ztest cluster setup.
- [Live-test guidelines](./live-tests/CLAUDE.md): rules for writing live tests.
- [Updating Zebra crates](./docs/updating_zebra_crates.md): the one Zebra dependency and how it is pinned.
- [Cargo docs](https://zingolabs.github.io/zaino/).

How it is built, and why:
- [Where data lives](./docs/design/boundaries.md): the consensus / keys / everything-else rule that decides what Zaino indexes.
- [The non-finalised window is pre-commit state](./docs/design/precommit-state.md): one fold, two watermarks, and why a reorg is the same operation as a restart.
- [Index data structures](./docs/design/index-data-structures.md): the two storage shapes every index is an instance of.
- [Persistence architecture](./docs/design/persistence-architecture.md): the measurements behind append-only files and mmap, and the mmap hazards.
- [Durability](./docs/design/durability.md): the manifest commit point, page checksums (the one disk check every index shares), crash testing, and why zainod dies rather than serve a state it cannot vouch for.
- [One view over many validators](./docs/design/chainview.md): quorum tip, quorum mempool, configured membership.
- [What the clients actually call](./docs/client-requirements.md): an audit of the two wallet sync engines.
- [Ironwood activation](./docs/notes/ironwood-activation.md): NU6.3 domain facts the indexes and live suite rely on.

Releasing it:
- [Release pipeline](./docs/release/pipeline.md), [changeset format](./docs/release/changeset-format.md), [implementation](./docs/release/implementation.md).

### Crate usage guides

Working *in* a crate: its scope, its invariants, and the mistakes its design
prevents.
- [`zaino-primitives`](./packages/zaino-primitives/usage.md): the domain vocabulary and protocol constants, and why it depends on nothing.
- [`zaino-source`](./packages/zaino-source/usage.md): the ports, the domain/fetch error split, `ValidatorClient`, and the ordered multi-validator `BlockFetchPool`.
- [`zaino-chainview`](./packages/zaino-chainview/usage.md): one view over N validators — the two-layer model, quorum and failing closed, and why `ours` is the exception.
- [`zaino-chain-head`](./packages/zaino-chain-head/usage.md): the in-memory non-final window, how `advance` resolves an extension or a reorg, and why a reorg replays without fetching.
- [`zaino-sync`](./packages/zaino-sync/usage.md): the one producer (bulk, then the quorum tip), why every index is fed from the rearmost resume point, derived sinks, the `IndexWriter` contract, and what the committed height promises.
- [`zaino-persistence`](./packages/zaino-persistence/usage.md): the on-disk record boundary, immutable sorted segments, and the report vocabulary every index verifier shares.
- [`zaino-index-compact-block`](./packages/zaino-index-compact-block/usage.md): the wire-shaped record store — one pin per request, zero-copy reads, and why there is no RAM cache.
- [`zaino-internal-block-hash-to-height`](./packages/zaino-internal-block-hash-to-height/usage.md): the hash ↔ height locator every by-hash request resolves through, and why the serving index confirms it.
- [`zaino-internal-value-balance`](./packages/zaino-internal-value-balance/usage.md): every transparent output's value, resolving each transaction's fee for compact blocks, and why all of it happens in `deliver`.
- [`zaino-index-tree-state`](./packages/zaino-index-tree-state/usage.md): the retained-node commitment-tree index — why reconstruction needs no hashing, and why subtree roots share its fold.
- [`zaino-index-transparent-address`](./packages/zaino-index-transparent-address/usage.md): the t-address RPCs as two pure projections, why the fold performs no lookups, and what an empty result means.
- [`zaino-grpc`](./packages/zaino-grpc/usage.md): the index/validator split, why the router writes bytes rather than messages, and which status code a client must read as "retry".
- [`zainod`](./packages/zainod/usage.md): `zainod verify` (the read-only page-checksum scrub, its JSON report and exit status), the failure policy, logging, and the admin listener (`/metrics`, `/livez`).

## Security disclosure

Report a time-sensitive security issue on Matrix (contacts in
[CONTRIBUTING.md](./CONTRIBUTING.md)); otherwise email
zingodisclosure@proton.me.

## License

[Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0); see [LICENSE](./LICENSE).
