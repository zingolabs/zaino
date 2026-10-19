# Zaino

Zaino is a Zcash indexer written in Rust. `zainod` follows the chain from one or more trusted
[Zebra](https://github.com/ZcashFoundation/zebra) validators (JSON-RPC, or blocks over their
indexer gRPC) and, optionally, Zcash p2p peers
([architecture §3](./docs/architecture.md#3-sources-and-what-each-is-trusted-for)), builds its
own indexes, and serves light wallets over the lightwalletd-compatible `CompactTxStreamer` gRPC
service.

Validation stays in Zebra and indexing moves to Zaino: the two run as separate
processes, on separate hosts if you like, and a new index needs no change to the
validator. [The architecture](./docs/architecture.md#1-who-owns-what) states the rule.

## Releases

Every crate shares one version. Each PR adds a change file (`knope document-change`), and a
release turns them into the version bump, the changelog and the `vX.Y.Z` tag with
[Knope](https://knope.tech): see [RELEASING.md](./RELEASING.md).

[![latest release](https://img.shields.io/github/v/tag/zingolabs/zaino?filter=v*&label=latest%20release)](https://github.com/zingolabs/zaino/tags)

## Project structure

```
packages/                          Cargo workspace members
  zainod/                            Daemon binary: config → boot (one task per stage), logging, verify
  # serving
  zaino-grpc/                        Lightwalletd-compatible gRPC over GrpcSnapshots, chain view, validators
  zaino-chainview/                   One view over N validators: verified tip, mempool, submission
  zaino-grpc-snapshot/               One GrpcSnapshot of everything served, its serving status
  # indexing
  zaino-nfs/                         Non-finalized state: fetch + fold above the durable tips, publications
  zaino-header-chain/                Validated Header tree (by a trusted validator or by Zaino)
  zaino-sync/                        Finalized stream (sink + queues) + the writers' apply / commit / publish
  zaino-index-compact-block/         Height → gRPC-framed compact block (one sequence)
  zaino-internal-block-hash-to-height/  Hash → height, the by-hash locator
  zaino-internal-txid-location/      Txid → mined block + position (GetTransaction with no validator)
  zaino-index-utxo-set/              Unspent outpoint → value: each transaction's fee for compact blocks
  zaino-index-tree-state/            Retained commitment-tree nodes (any past frontier) and subtree roots
  zaino-index-transparent-address/   Transparent address → receives; outpoint → spend (two maps)
  zaino-persistence/                 Persistence port + DiskEngine (files for sequences, LSM for maps)
  # validator source
  zaino-source/                      `ChainDataSource` + its JSON-RPC and indexer-gRPC clients, block decode
  zaino-traffic/                     One scheduler for every request to validators and peers
  zaino-peers/                       zebra-network peer set: headers, blocks, mempool listings, submission
  # vocabulary
  zaino-primitives/                  Chain-level domain types and protocol constants
  zaino-proto/                       Lightwallet protocol buffers

live-tests/                        Standalone workspace, run on the ztest Kubernetes harness
  clientless/                        Zaino against a live validator, no wallet
  e2e/                               Wallet → Zaino → validator
  non-finalized-state/               Reorgs, the served tip, every tip-dependent RPC
  zaino-testutils/                   Shared test utilities
  sync/                              Mainnet sync profiles (own workspace, `ztest sync`)

docs/                              Operator docs, architecture, RPC API, testing
nix/, flake.nix                    Nix build and dev shell (every dev, lint and release tool, pinned)
.github/                           CI workflows and issue templates
.changeset/                        Pending change files (Knope)

.pre-commit-config.yaml            Every lint CI runs (pre-commit hooks)
knope.toml                         Release config: one version, changelog, GitHub release
rust-toolchain.toml                Pinned Rust toolchain
deny.toml                          cargo-deny policy
dupes.toml, .dupes-ignore.toml     code-dupes policy and reviewed exceptions
Dockerfile                         Container image
CONTEXT.md                         Glossary of canonical terms
CLAUDE.md, AGENTS.md               AI-contributor guidelines
CONTRIBUTING.md                    Human-contributor guide
RELEASING.md                       Release process
```

## Quick start

```sh
cargo run --release -p zainod -- generate-config   # then edit; every key: docs/example_configs/zainod.toml
cargo run --release -p zainod -- start
cargo nextest run --workspace                      # unit tests, no validator
```

## Documentation

Operating it:
- [Running zainod](./docs/running.md): install, configure, network exposure, a local Zebra + Zaino pair.
- [RPC API](./docs/rpc_api.md): the methods zainod serves, where each is answered from, and the status codes a client must tell apart.
- [Docker](./docs/docker.md): the container image.
- [Testing](./docs/testing.md): unit tests, live tests, sync profiles, ztest cluster setup.
- [Live-test guidelines](./live-tests/CLAUDE.md): rules for writing live tests.
- [Updating Zebra](./docs/updating_zebra_crates.md): testing against a new zebrad release, and how the librustzcash forks are pinned.

How it is built, and why:
- [Architecture](./docs/architecture.md): **start here.** Who owns what, the map, what each source is trusted for, the best chain and finality, the mempool, submission, indexing, serving from one `GrpcSnapshot`, failure modes.
- [Storage](./packages/zaino-persistence/usage.md): the port every index stores through, the on-disk layout, the commit and crash contract, and why it is append-only files and mmap. Each index's own guide (below) covers what it keeps.
- [What the clients actually call](./docs/client-requirements.md): an audit of the two wallet sync engines.

Releasing it:
- [RELEASING.md](./RELEASING.md): change files and releases.

### Crate usage guides

Working *in* a crate: its scope, its invariants, and the mistakes its design
prevents.
- [`zaino-primitives`](./packages/zaino-primitives/usage.md): the domain vocabulary and protocol constants, and why it depends on no other Zaino crate.
- [`zaino-source`](./packages/zaino-source/usage.md): the `ChainDataSource` port, the domain / non-domain error split, and its two clients, `ZcashLegacyRpcClient` (JSON-RPC) and `ZebraIndexerGrpcClient` (indexer gRPC).
- [`zaino-traffic`](./packages/zaino-traffic/usage.md): one scheduler for every request to trusted validators and peers — request classes with reserved permits, one hedge / retry / blame policy under one budget, `report` benching exactly the member that misanswered, and every trusted validator's poll.
- [`zaino-chainview`](./packages/zaino-chainview/usage.md): one view over N trusted validators and, optionally, peers — header sync into the verified chain, each validator's poll merged (who holds the tip, agreement, alarms), the mempool view (trusted listings and the peer quorum), and submission.
- [`zaino-peers`](./packages/zaino-peers/usage.md): zebra-network embedded as Zaino's p2p layer — headers and blocks (checked by proof of work and merkle root, so the tip follows the network while every trusted validator is down), each peer's own mempool listing (the chain view's peer quorum: the mempool keeps serving while every trusted validator is down), attributed block announcements (finality on peers' word) and the submission push over the peer set's existing connections; advertises no services.
- [`zaino-header-chain`](./packages/zaino-header-chain/usage.md): the Validated Header tree, each header validated by the trusted validator that served it or by Zaino, anchored at a trusted tip − depth (no history from genesis) — the most-work tip, vouched finality, and `validate`, the consensus rules kept as the fallback for an untrusted source.
- [`zaino-nfs`](./packages/zaino-nfs/usage.md): the non-finalized state — `Nfs` fetches and folds every block above the indexes' durable tips (all indexes, in dependency order) as it joins the verified best, and publishes one `NfsPublication` across every index that moves to the fork point the moment a reorg lands.
- [`zaino-grpc-snapshot`](./packages/zaino-grpc-snapshot/usage.md): one published `GrpcSnapshot` of everything served — the chain view's and the NFS's latest in one atomic publication, read once per gRPC request, probe and scrape; the mempool stream keyed to the served tip; the `ServingStatus`, the gauges and the alarm logs derived from it.
- [`zaino-sync`](./packages/zaino-sync/usage.md): `FinalizedFollower` and the finalized stream (`Step`, `Run`), byte-bounded queues, fees from one index to another, and the `apply` / `commit` / `IndexPublisher` every writer's loop runs on.
- [`zaino-persistence`](./packages/zaino-persistence/usage.md): the persistence port every index stores through (schema, changes, views, verify) and `DiskEngine`, the files + LSM engine behind it.
- [`zaino-index-compact-block`](./packages/zaino-index-compact-block/usage.md): the wire-shaped record store — one pin per request, zero-copy reads, and why there is no RAM cache.
- [`zaino-internal-block-hash-to-height`](./packages/zaino-internal-block-hash-to-height/usage.md): the hash → height locator every by-hash request resolves through, and why the serving index confirms it.
- [`zaino-internal-txid-location`](./packages/zaino-internal-txid-location/usage.md): where each mined transaction sits, so `GetTransaction` can fetch its block from p2p peers when no validator answers.
- [`zaino-index-utxo-set`](./packages/zaino-index-utxo-set/usage.md): each unspent transparent output's value, resolving each transaction's fee for compact blocks, and why all of it happens in one `fold`.
- [`zaino-index-tree-state`](./packages/zaino-index-tree-state/usage.md): the retained-node commitment-tree index — why reconstruction needs no hashing, and why subtree roots share its fold.
- [`zaino-index-transparent-address`](./packages/zaino-index-transparent-address/usage.md): the t-address RPCs as two pure projections, why the fold performs no lookups, and what an empty result means.
- [`zaino-proto`](./packages/zaino-proto/README.md): the vendored lightwallet protocol, Zebra's indexer protocol, and the gRPC frame helpers.
- [`zaino-grpc`](./packages/zaino-grpc/usage.md): `RouteDeps` and the route registry, the serve stack (caps, adaptive admission, deadlines, TLS, PROXY protocol, health paths), read lanes, the serving log, and why stored records go on the wire as bytes.
- [`zainod`](./packages/zainod/usage.md): the admin listener and its states (`/livez`, `/readyz`, `/metrics`) and the public `/status`, operator hold, systemd, index snapshots, `zainod verify`, the failure policy and logging.

## Security disclosure

Report security issues privately, never in a public issue: see
[CONTRIBUTING.md](./CONTRIBUTING.md#security-issues).

## License

[Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0); see [LICENSE](./LICENSE).
